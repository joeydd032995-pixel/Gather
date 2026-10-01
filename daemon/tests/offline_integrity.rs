//! Regression coverage for local persistence, snapshots and atomic imports.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{TimeZone, Utc};
use gather_daemon::config::Config;
use gather_daemon::extract::persist::{self, Chunk, ChunkAnchor, Claim};
use gather_daemon::{db, extract, routes, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

async fn state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&database_url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    Some(AppState {
        pool,
        config: Arc::new(Config::for_tests(database_url)),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        ollama: None,
        rate_limiter: None,
    })
}

async fn chunk(state: &AppState, text: &str, day: u32) -> Chunk {
    let hash = format!("{:0<64}", Uuid::new_v4().simple());
    let artifact: Uuid = sqlx::query_scalar(
        "INSERT INTO artifacts (kind, byte_size, content_hash, raw_content)
         VALUES ('document_txt', $1, $2, $3) RETURNING id",
    )
    .bind(text.len() as i64)
    .bind(&hash)
    .bind(text.as_bytes())
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let document: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (artifact_id, extraction_status) VALUES ($1, 'completed')
         RETURNING id",
    )
    .bind(artifact)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let segment: Uuid = sqlx::query_scalar(
        "INSERT INTO document_segments (document_id, seq, content, content_hash)
         VALUES ($1, 0, $2, $3) RETURNING id",
    )
    .bind(document)
    .bind(text)
    .bind(hash)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    Chunk {
        anchor: ChunkAnchor::Segment(segment),
        artifact_id: artifact,
        text: text.into(),
        source_time: Some(Utc.with_ymd_and_hms(2026, day, 1, 0, 0, 0).unwrap()),
        user_authored: true,
        ocr_confidence: None,
    }
}

async fn persist(state: &AppState, chunk: &Chunk, claim: Claim) -> persist::PersistOutcome {
    let units: Vec<_> = extract::rules::extract_units(&chunk.text)
        .into_iter()
        .map(|unit| (unit, "rule_based", None))
        .collect();
    persist::persist_chunk_units(&state.pool, chunk, &claim, &units, 0.0, 0.0)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn recurring_claim_has_a_new_episode_without_reversing_rejection() {
    let Some(state) = state().await else { return };
    let text = format!("My Budget{} is $50 per month.", Uuid::new_v4().simple());
    let first = chunk(&state, &text, 1).await;
    let result = persist(&state, &first, Claim::Fresh { llm_model: None }).await;
    assert_eq!(result.units_created, 1);
    let old = result.new_units[0].0;
    let ended = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
    sqlx::query("UPDATE atomic_units SET status = 'superseded', valid_to = $2 WHERE id = $1")
        .bind(old)
        .bind(ended)
        .execute(&state.pool)
        .await
        .unwrap();
    let second = chunk(&state, &text, 3).await;
    let result = persist(&state, &second, Claim::Fresh { llm_model: None }).await;
    assert_eq!(result.units_created, 1);
    assert_ne!(old, result.new_units[0].0);
    let new = result.new_units[0].0;
    let old_end: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT valid_to FROM atomic_units WHERE id = $1")
            .bind(old)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(old_end, Some(ended));
    let reread = persist(
        &state,
        &first,
        Claim::Reread {
            job: Uuid::new_v4(),
            model: "local-test".into(),
        },
    )
    .await;
    assert_eq!(reread.units_created, 0);
    assert_eq!(reread.units_reasserted, 0);

    routes::feedback::reject_unit_core(&state.pool, new, None)
        .await
        .unwrap();
    let third = chunk(&state, &text, 4).await;
    let result = persist(&state, &third, Claim::Fresh { llm_model: None }).await;
    assert_eq!(result.units_created, 0);
    let status: String = sqlx::query_scalar("SELECT status::text FROM atomic_units WHERE id = $1")
        .bind(new)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(status, "retracted");
}

#[tokio::test]
async fn export_keeps_source_and_claim_state_from_one_snapshot() {
    let Some(state) = state().await else { return };
    let text = format!("I use Snapshot{} for storage.", Uuid::new_v4().simple());
    let chunk = chunk(&state, &text, 1).await;
    let result = persist(&state, &chunk, Claim::Fresh { llm_model: None }).await;
    let unit = result.new_units[0].0;
    let app = routes::build_router(state.clone());
    let response = app
        .oneshot(Request::get("/api/v1/export").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut pending = Vec::new();
    let mut artifact_seen = false;
    let mut unit_seen = false;
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            pending.extend_from_slice(&data);
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = pending.drain(..=end).collect();
                let value: Value = serde_json::from_slice(&line).unwrap();
                if value["type"] == "artifacts"
                    && value["row"]["id"] == chunk.artifact_id.to_string()
                {
                    assert!(value["row"]["retracted_at"].is_null());
                    artifact_seen = true;
                    let mut tx = state.pool.begin().await.unwrap();
                    sqlx::query("UPDATE artifacts SET retracted_at = now() WHERE id = $1")
                        .bind(chunk.artifact_id)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    sqlx::query("UPDATE atomic_units SET status = 'retracted' WHERE id = $1")
                        .bind(unit)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();
                }
                if value["type"] == "atomic_units" && value["row"]["id"] == unit.to_string() {
                    assert_eq!(value["row"]["status"], "active");
                    unit_seen = true;
                }
            }
        }
    }
    assert!(artifact_seen && unit_seen);
}

#[tokio::test]
async fn invalid_import_rolls_back_prior_records() {
    let Some(state) = state().await else { return };
    let project = Uuid::new_v4();
    let records = [
        json!({ "type": "manifest", "row": { "format": "gather-bundle-v1" } }),
        json!({ "type": "projects", "row": {
            "id": project, "name": "Rollback fixture", "source": "manual",
            "created_at": Utc::now(), "updated_at": Utc::now()
        }}),
        json!({ "type": "project_items", "row": {
            "id": Uuid::new_v4(), "project_id": project, "item_kind": "invalid",
            "name": "Invalid child", "path": "bad", "depth": 0, "status": "stored",
            "created_at": Utc::now()
        }}),
    ];
    let bundle = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let response = routes::build_router(state.clone())
        .oneshot(
            Request::post("/api/v1/import")
                .header(header::CONTENT_TYPE, "application/x-ndjson")
                .body(Body::from(bundle))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1)")
        .bind(project)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(!exists);
}
