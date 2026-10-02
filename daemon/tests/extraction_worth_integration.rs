//! What Gather stores from a file, end to end against a real Postgres
//! (pgvector). Skipped without DATABASE_URL, like the other integration tests.
//!
//! A document mixing real statements with arithmetic, fragments, filler and a
//! question is uploaded and extracted; only the real statements become units,
//! and only real names become entities. Then the optional-review cap is
//! exercised: with the tray full of optional items, a new "stated or not?"
//! unit is still kept, but not queued.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::extract;
use gather_daemon::extract::persist::OPTIONAL_REVIEW_OPEN_CAP;
use gather_daemon::safety::modality;
use gather_daemon::{db, routes, AppState};

async fn test_state() -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return None;
    };
    let pool = db::connect(&database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
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

async fn upload_markdown(state: &AppState, filename: &str, text: &str) -> Uuid {
    let boundary = "gatherworthboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\nContent-Type: text/markdown\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(text.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::post("/api/v1/ingest/files")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = routes::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let out: Value = serde_json::from_slice(&bytes).unwrap();
    out["files"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// Run extraction until this artifact's segments have all been read.
async fn drain(state: &AppState, artifact_id: Uuid) {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .expect("extraction pass");
        let (unread,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM document_segments s
             JOIN documents d ON d.id = s.document_id
             WHERE d.artifact_id = $1 AND s.units_extracted_at IS NULL",
        )
        .bind(artifact_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        let (segments,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM document_segments s
             JOIN documents d ON d.id = s.document_id WHERE d.artifact_id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if segments > 0 && unread == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("extraction did not read the document in time");
}

async fn statements(state: &AppState, artifact_id: Uuid) -> Vec<String> {
    let mut found: Vec<String> = sqlx::query(
        "SELECT u.statement FROM atomic_units u
         JOIN atomic_unit_provenance p ON p.atomic_unit_id = u.id
         WHERE p.artifact_id = $1",
    )
    .bind(artifact_id)
    .fetch_all(&state.pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("statement"))
    .collect();
    found.sort();
    found
}

#[tokio::test]
async fn only_statements_worth_keeping_are_stored_and_the_tray_stays_small() {
    let Some(state) = test_state().await else {
        return;
    };
    let m = Uuid::new_v4().simple().to_string();

    // ---- Junk is left out; real statements and short entity names are kept.
    let text = format!(
        "# Notes {m}\n\n\
         We decided on Hetzner{m} for the backup target.\n\n\
         I use Dark{m} mode in every editor.\n\n\
         2 + 1 is 3 is equal to = 3. The page is 3 + 4 = 7. x = 4 so y = 5.\n\n\
         I have no idea what to do next. I am happy.\n\n\
         What is 2+2? Is the budget $75 per month?\n"
    );
    let artifact = upload_markdown(&state, &format!("worth-{m}.md"), &text).await;
    drain(&state, artifact).await;

    assert_eq!(
        statements(&state, artifact).await,
        vec![
            format!("I use Dark{m} mode in every editor"),
            format!("We decided on Hetzner{m} for the backup target"),
        ],
        "arithmetic, filler, fragments and questions are not stored"
    );

    // Entities are the names inside the phrases, not the phrases.
    let mut names: Vec<String> = sqlx::query("SELECT name FROM entities WHERE name LIKE $1")
        .bind(format!("%{m}%"))
        .fetch_all(&state.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    names.sort();
    assert_eq!(names, vec![format!("Dark{m} mode"), format!("Hetzner{m}")]);
    let (junk,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM entities
         WHERE name IN ('2+1', '3', 'x', 'y', 'page') OR name ILIKE '%no idea what%'
            OR name ILIKE '%for the backup target%'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        junk, 0,
        "numbers, expressions and fragments are not entities"
    );

    // ---- The tray keeps at most OPTIONAL_REVIEW_OPEN_CAP optional items.
    let (open,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM review_queue
         WHERE target_kind = 'unit' AND state = 'open'
           AND reason IN ('low-confidence', 'modality-uncertain')",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let filler = (OPTIONAL_REVIEW_OPEN_CAP - open).max(0);
    let mut filler_ids = Vec::new();
    for _ in 0..filler {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO review_queue (target_kind, target_id, reason) \
             VALUES ('unit', $1, 'low-confidence')",
        )
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
        filler_ids.push(id);
    }

    let ambiguous = format!("I don't use Foo{m} for storage, maybe");
    assert!(
        modality::classify(&ambiguous).ambiguous,
        "the premise: this reads as 'stated or not?'"
    );
    let artifact = upload_markdown(
        &state,
        &format!("ambiguous-{m}.md"),
        &format!("{ambiguous}.\n"),
    )
    .await;
    drain(&state, artifact).await;

    let kept: Option<(Uuid,)> = sqlx::query_as(
        "SELECT u.id FROM atomic_units u
         JOIN atomic_unit_provenance p ON p.atomic_unit_id = u.id
         WHERE p.artifact_id = $1 AND u.statement = $2",
    )
    .bind(artifact)
    .bind(&ambiguous)
    .fetch_optional(&state.pool)
    .await
    .unwrap();
    let (unit_id,) = kept.expect("the unit is kept even when it isn't queued");
    let (queued,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM review_queue WHERE target_kind = 'unit' AND target_id = $1",
    )
    .bind(unit_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(queued, 0, "a full tray takes no more optional items");

    sqlx::query("DELETE FROM review_queue WHERE target_id = ANY($1)")
        .bind(&filler_ids)
        .execute(&state.pool)
        .await
        .unwrap();
}
