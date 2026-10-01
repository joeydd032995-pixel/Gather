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

// These workers operate on the whole database, so fixture tests take turns.
// Intentional concurrency is driven inside each individual regression.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
         VALUES ('document_text', $1, $2, $3) RETURNING id",
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
    let _guard = LOCK.lock().await;
    let Some(state) = state().await else { return };
    let text = format!("My Budget{} is $50 per month.", Uuid::new_v4().simple());
    let first = chunk(&state, &text, 1).await;
    let result = persist(&state, &first, Claim::Fresh { llm_model: None }).await;
    assert_eq!(result.units_created, 1);
    let old = result.new_units[0].0;
    let ended = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
    let middle_text = text.replace("$50", "$75");
    let middle = chunk(&state, &middle_text, 3).await;
    let middle_result = persist(&state, &middle, Claim::Fresh { llm_model: None }).await;
    let middle_unit = middle_result.new_units[0].0;
    let second = chunk(&state, &text, 5).await;
    let result = persist(&state, &second, Claim::Fresh { llm_model: None }).await;
    assert_eq!(result.units_created, 1);
    assert_ne!(old, result.new_units[0].0);
    let new = result.new_units[0].0;
    // Extraction gets ahead of scanning: all three assertions already exist.
    // Dates are more than the configured 30-day succession threshold apart.
    for _ in 0..200 {
        gather_daemon::scan::run_one_scan(&state.pool, &state.config, None)
            .await
            .unwrap();
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM atomic_units WHERE id = ANY($1) AND status <> 'superseded'",
        )
        .bind(vec![old, middle_unit])
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if pending == 0 {
            break;
        }
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM atomic_units WHERE id = ANY($1) AND status <> 'superseded'",
    )
    .bind(vec![old, middle_unit])
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(pending, 0);
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
    let third = chunk(&state, &text, 7).await;
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
    let _guard = LOCK.lock().await;
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
    let _guard = LOCK.lock().await;
    let Some(state) = state().await else { return };
    let project = Uuid::new_v4();
    let records = [
        json!({ "type": "manifest", "row": { "format": "gather-bundle-v1" } }),
        json!({ "type": "projects", "row": {
            "id": project, "name": "Rollback fixture", "source": "folder",
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

#[tokio::test]
async fn similarity_cache_changes_when_unit_status_changes_without_new_rows() {
    let _guard = LOCK.lock().await;
    let Some(state) = state().await else {
        return;
    };
    let text = format!("I use Cache{} for storage.", Uuid::new_v4().simple());
    let chunk = chunk(&state, &text, 1).await;
    let result = persist(&state, &chunk, Claim::Fresh { llm_model: None }).await;
    let unit = result.new_units[0].0;
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (name, source) VALUES ($1, 'folder') RETURNING id",
    )
    .bind(format!("Cache fixture {}", Uuid::new_v4()))
    .fetch_one(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO project_items
            (project_id, item_kind, name, path, depth, artifact_id, status)
         VALUES ($1, 'file', 'cache.txt', 'cache.txt', 0, $2, 'ingested')",
    )
    .bind(project)
    .bind(chunk.artifact_id)
    .execute(&state.pool)
    .await
    .unwrap();
    let before = gather_daemon::projects::similarity::load(&state.pool, 1000)
        .await
        .unwrap();
    assert!(!before.signatures[&project].entities.is_empty());
    let mut tx = state.pool.begin().await.unwrap();
    sqlx::query("UPDATE atomic_units SET status = 'retracted' WHERE id = $1")
        .bind(unit)
        .execute(&mut *tx)
        .await
        .unwrap();
    let pending = gather_daemon::projects::similarity::load(&state.pool, 1000)
        .await
        .unwrap();
    assert!(!pending.signatures[&project].entities.is_empty());
    tx.commit().await.unwrap();
    let after = gather_daemon::projects::similarity::load(&state.pool, 1000)
        .await
        .unwrap();
    assert!(after.signatures[&project].entities.is_empty());
    assert!(!Arc::ptr_eq(&before, &after));
}

#[tokio::test]
async fn local_embedding_retry_revision_guard_and_model_change() {
    let _guard = LOCK.lock().await;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    struct Mock {
        fail: AtomicBool,
        block: AtomicBool,
        entered: Notify,
        release: Notify,
    }
    async fn embed(
        State(mock): State<Arc<Mock>>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        if mock.fail.load(Ordering::SeqCst) {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        if mock.block.load(Ordering::SeqCst) {
            mock.entered.notify_one();
            mock.release.notified().await;
        }
        let mut vector = vec![0.0f32; 768];
        vector[0] = 1.0;
        let embeddings = vec![vector; body["input"].as_array().unwrap().len()];
        (StatusCode::OK, Json(json!({ "embeddings": embeddings })))
    }
    let Some(state) = state().await else {
        return;
    };
    let mock = Arc::new(Mock {
        fail: AtomicBool::new(true),
        block: AtomicBool::new(false),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/api/embed", post(embed))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut config = (*state.config).clone();
    config.ollama_url = Some(format!("http://{address}"));
    config.ollama_embed_model = "offline-test-model-a".into();
    config.ollama_one_at_a_time = false;
    let client = Arc::new(
        extract::ollama::OllamaClient::from_config(&config)
            .unwrap()
            .unwrap(),
    );
    persist::ensure_embedding_model(&state.pool, &client.embed_model)
        .await
        .unwrap();
    let text = format!("I use Retry{} for storage.", Uuid::new_v4().simple());
    let chunk = chunk(&state, &text, 1).await;
    let result = persist(&state, &chunk, Claim::Fresh { llm_model: None }).await;
    let unit = result.new_units[0].0;

    assert!(persist::embed_pending_units(&state.pool, &client, 1000)
        .await
        .is_err());
    let waiting: bool = sqlx::query_scalar(
        "SELECT embedding IS NULL AND embedding_attempts = 1
                AND embedding_retry_at > now() FROM atomic_units WHERE id = $1",
    )
    .bind(unit)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(waiting);
    sqlx::query("UPDATE atomic_units SET embedding_retry_at = now() WHERE id = $1")
        .bind(unit)
        .execute(&state.pool)
        .await
        .unwrap();
    mock.fail.store(false, Ordering::SeqCst);
    persist::embed_pending_units(&state.pool, &client, 1000)
        .await
        .unwrap();
    let stored: bool = sqlx::query_scalar(
        "SELECT embedding IS NOT NULL AND embedding_model = $2 FROM atomic_units WHERE id = $1",
    )
    .bind(unit)
    .bind(&client.embed_model)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(stored);
    let ChunkAnchor::Segment(segment) = chunk.anchor else {
        unreachable!();
    };
    let mut vector = vec![0.0f32; 768];
    vector[0] = 1.0;
    sqlx::query("UPDATE document_segments SET embedding = $2, embedding_model = $3 WHERE id = $1")
        .bind(segment)
        .bind(pgvector::Vector::from(vector.clone()))
        .bind(&client.embed_model)
        .execute(&state.pool)
        .await
        .unwrap();
    let response = routes::build_router(state.clone())
        .oneshot(
            Request::post("/api/v1/search/semantic")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "embedding": vector, "scope": "document_segments"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let hits: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(hits["hits"][0]["id"], segment.to_string());

    let corrected = format!("Corrected wording {}", Uuid::new_v4());
    routes::feedback::edit_unit_core(&state.pool, unit, &corrected, None)
        .await
        .unwrap();
    mock.block.store(true, Ordering::SeqCst);
    let pool = state.pool.clone();
    let delayed_client = client.clone();
    let old_text = corrected.clone();
    let delayed = tokio::spawn(async move {
        persist::embed_new_units(&pool, &delayed_client, &[(unit, old_text)]).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified())
        .await
        .unwrap();
    let current = format!("Another correction {}", Uuid::new_v4());
    routes::feedback::edit_unit_core(&state.pool, unit, &current, None)
        .await
        .unwrap();
    mock.block.store(false, Ordering::SeqCst);
    mock.release.notify_one();
    assert_eq!(delayed.await.unwrap().unwrap(), 0);

    persist::embed_new_units(&state.pool, &client, &[(unit, current.clone())])
        .await
        .unwrap();
    // Albums are based on EXIF, not the embedding model, and must survive a switch.
    let album: Uuid =
        sqlx::query_scalar("INSERT INTO clusters (kind) VALUES ('album') RETURNING id")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let photo: Uuid = sqlx::query_scalar(
        "INSERT INTO images (artifact_id, width, height, album_cluster_id)
         VALUES ($1, 1, 1, $2) RETURNING id",
    )
    .bind(chunk.artifact_id)
    .bind(album)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO cluster_members (cluster_id, member_kind, member_id)
         VALUES ($1, 'image', $2)",
    )
    .bind(album)
    .bind(photo)
    .execute(&state.pool)
    .await
    .unwrap();
    persist::ensure_embedding_model(&state.pool, "offline-test-model-b")
        .await
        .unwrap();
    let empty: bool =
        sqlx::query_scalar("SELECT embedding IS NULL FROM atomic_units WHERE id = $1")
            .bind(unit)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(empty);
    assert_eq!(
        persist::embed_new_units(&state.pool, &client, &[(unit, current)])
            .await
            .unwrap(),
        0
    );

    let album_members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cluster_members WHERE cluster_id = $1")
            .bind(album)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(album_members, 1);
    sqlx::query("DELETE FROM images WHERE id = $1")
        .bind(photo)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM clusters WHERE id = $1")
        .bind(album)
        .execute(&state.pool)
        .await
        .unwrap();

    // Leave this shared integration database ready for the other test binaries.
    sqlx::query(
        "UPDATE atomic_units SET embedding = NULL, embedding_model = NULL,
         embedding_retry_at = NULL, embedding_attempts = 0
         WHERE embedding_model LIKE 'offline-test-model-%' OR embedding IS NULL",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE embedding_state SET model = NULL WHERE singleton")
        .execute(&state.pool)
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn restore_cannot_reactivate_a_claim_after_its_only_source_was_withdrawn() {
    let _guard = LOCK.lock().await;
    let Some(state) = state().await else {
        return;
    };
    let text = format!("I use Gone{} for storage.", Uuid::new_v4().simple());
    let chunk = chunk(&state, &text, 1).await;
    let result = persist(&state, &chunk, Claim::Fresh { llm_model: None }).await;
    let unit = result.new_units[0].0;
    gather_daemon::safety::service::retract_artifact(
        &state.pool,
        chunk.artifact_id,
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert!(routes::feedback::restore_unit_core(&state.pool, unit, None)
        .await
        .is_err());
    let status: String = sqlx::query_scalar("SELECT status::text FROM atomic_units WHERE id = $1")
        .bind(unit)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(status, "retracted");
    let response = routes::build_router(state.clone())
        .oneshot(
            Request::post("/api/v1/search/semantic")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "text": text, "scope": "document_segments"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let results: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(results["hits"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn importing_unknown_vectors_drops_their_topic_memberships() {
    let _guard = LOCK.lock().await;
    let Some(state) = state().await else {
        return;
    };
    let unit = Uuid::new_v4();
    let topic = Uuid::new_v4();
    let now = Utc::now();
    let records = [
        json!({ "type": "manifest", "row": { "format": "gather-bundle-v1" } }),
        json!({ "type": "clusters", "row": {
            "id": topic, "kind": "topic", "label": "Imported topic",
            "cohesion": 1.0, "size": 1, "created_at": now, "updated_at": now
        }}),
        json!({ "type": "atomic_units", "row": {
            "id": unit, "kind": "fact", "statement": format!("Imported {unit}"),
            "statement_hash": format!("{:0<64}", unit.simple()),
            "confidence": 0.7, "extraction_method": "rule_based",
            "attrs": {}, "status": "active", "created_at": now, "updated_at": now,
            "embedding": format!("[{}]", vec!["1"; 768].join(",")),
            "embedding_model": "unknown-import-model", "topic_cluster_id": topic
        }}),
        json!({ "type": "cluster_members", "row": {
            "cluster_id": topic, "member_kind": "unit", "member_id": unit, "sim": 1.0
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
    assert_eq!(response.status(), StatusCode::OK);
    let invalidated: bool = sqlx::query_scalar(
        "SELECT embedding IS NULL AND topic_cluster_id IS NULL
         FROM atomic_units WHERE id = $1",
    )
    .bind(unit)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(invalidated);
    let members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cluster_members WHERE cluster_id = $1")
            .bind(topic)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(members, 0);
    sqlx::query("DELETE FROM atomic_units WHERE id = $1")
        .bind(unit)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM clusters WHERE id = $1")
        .bind(topic)
        .execute(&state.pool)
        .await
        .unwrap();
}
