//! End-to-end extraction pipeline tests against a real Postgres (pgvector).
//! Skipped without DATABASE_URL, like tests/api_integration.rs.
//!
//! Flow exercised: upload PDF + image + chat export through the real router,
//! then drive extract::run_one_pass() until the queues drain, and assert the
//! resulting documents/segments/units/provenance/entities/relationships.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::extract;
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
    })
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn multipart_request(filename: &str, content_type: &str, bytes: &[u8]) -> Request<Body> {
    let boundary = "gatherextractionboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::post("/api/v1/ingest/files")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// Remove any prior run's fixture artifact (dedup would otherwise skip the
/// pipeline stages this test asserts on). Cascades to documents/segments/
/// images/provenance.
async fn delete_fixture_artifact(state: &AppState, filename: &str) {
    sqlx::query("DELETE FROM artifacts WHERE original_filename = $1")
        .bind(filename)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// Run extraction passes until every queue is empty. The integration tests
/// run in parallel, so "my pass claimed nothing" is not the same as "nothing
/// is in flight" — a sibling test's pass may hold rows in 'processing' or be
/// mid-write. Wait on the actual queue state instead, with a generous cap.
async fn drain_extraction(state: &AppState) {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .expect("extraction pass");
        let (busy,): (i64,) = sqlx::query_as(
            r#"
            SELECT
              (SELECT count(*) FROM documents
               WHERE extraction_status IN ('pending','processing'))
            + (SELECT count(*) FROM images
               WHERE ocr_status IN ('pending','processing'))
            + (SELECT count(*) FROM messages WHERE units_extracted_at IS NULL)
            + (SELECT count(*) FROM document_segments WHERE units_extracted_at IS NULL)
            + (SELECT count(*) FROM images
               WHERE units_extracted_at IS NULL AND ocr_status = 'completed'
                 AND ocr_text IS NOT NULL AND length(trim(ocr_text)) > 0)
            "#,
        )
        .fetch_one(&state.pool)
        .await
        .expect("queue state query");
        if busy == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("extraction queues did not drain within the deadline");
}

#[tokio::test]
async fn pdf_upload_is_extracted_segmented_and_unitized() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    delete_fixture_artifact(&state, "budget.pdf").await;

    // The fixture text includes decision/numeric/first-person sentences.
    let pdf = include_bytes!("fixtures/tiny.pdf");
    let res = app
        .clone()
        .oneshot(multipart_request("budget.pdf", "application/pdf", pdf))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let out = body_json(res).await;
    let artifact_id: Uuid = out["files"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(out["files"][0]["kind"], json!("document_pdf"));
    assert_eq!(out["files"][0]["segments"], json!(0)); // deferred to the worker

    drain_extraction(&state).await;

    // Document row completed with real text and segments.
    let doc = sqlx::query(
        r#"SELECT d.extraction_status::text AS status, d.extracted_text, d.page_count,
                  (SELECT count(*) FROM document_segments s WHERE s.document_id = d.id) AS segments
           FROM documents d WHERE d.artifact_id = $1"#,
    )
    .bind(artifact_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(doc.get::<String, _>("status"), "completed");
    assert!(doc
        .get::<Option<String>, _>("extracted_text")
        .unwrap()
        .contains("budget"));
    assert_eq!(doc.get::<Option<i32>, _>("page_count"), Some(1));
    assert!(doc.get::<i64, _>("segments") >= 1);

    // Units extracted from the PDF text, with segment-anchored provenance
    // pointing back to this artifact.
    let units = sqlx::query(
        r#"SELECT u.kind::text AS kind, u.statement, p.document_segment_id, p.quote
           FROM atomic_units u
           JOIN atomic_unit_provenance p ON p.atomic_unit_id = u.id
           WHERE p.artifact_id = $1"#,
    )
    .bind(artifact_id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert!(
        units.len() >= 2,
        "expected decision + claim units from the PDF, got {}",
        units.len()
    );
    assert!(units
        .iter()
        .all(|r| r.get::<Option<Uuid>, _>("document_segment_id").is_some()));
    assert!(units
        .iter()
        .any(|r| r.get::<String, _>("kind") == "decision"
            && r.get::<String, _>("statement").contains("Hetzner")));

    // The decision produced a graph edge Me -[decided_on]-> Hetzner CX22.
    let edges = sqlx::query(
        r#"SELECT e1.name AS source, r.relation_type, e2.name AS target
           FROM relationships r
           JOIN entities e1 ON e1.id = r.source_entity_id
           JOIN entities e2 ON e2.id = r.target_entity_id
           WHERE r.relation_type = 'decided_on' AND e1.name = 'Me'"#,
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert!(edges
        .iter()
        .any(|r| r.get::<String, _>("target").contains("Hetzner")));

    // Backlog fully drained for this artifact.
    let (pending,): (i64,) = sqlx::query_as(
        r#"SELECT count(*) FROM document_segments s
           JOIN documents d ON d.id = s.document_id
           WHERE d.artifact_id = $1 AND s.units_extracted_at IS NULL"#,
    )
    .bind(artifact_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn image_upload_gets_metadata_and_ocr_units_flow() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());

    delete_fixture_artifact(&state, "screen.png").await;
    let png = include_bytes!("fixtures/tiny.png");
    let res = app
        .clone()
        .oneshot(multipart_request("screen.png", "image/png", png))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let out = body_json(res).await;
    let artifact_id: Uuid = out["files"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(out["files"][0]["kind"], json!("image_screenshot"));

    drain_extraction(&state).await;

    let img = sqlx::query(
        "SELECT id, width, height, ocr_status::text AS status, ocr_text FROM images WHERE artifact_id = $1",
    )
    .bind(artifact_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let image_id: Uuid = img.get("id");
    assert_eq!(img.get::<Option<i32>, _>("width"), Some(1800));
    assert_eq!(img.get::<Option<i32>, _>("height"), Some(280));
    let status: String = img.get("status");
    assert!(
        ["completed", "skipped"].contains(&status.as_str()),
        "unexpected ocr status {status}"
    );

    if status == "skipped" {
        eprintln!("tesseract not installed; exercising OCR-text unit path via injected text");
    }
    // Make the unit-extraction-from-OCR path deterministic regardless of
    // what tesseract read: inject known OCR text and reopen the chunk.
    let marker = Uuid::new_v4().simple().to_string();
    sqlx::query(
        r#"UPDATE images
           SET ocr_status = 'completed', ocr_confidence = 0.95,
               ocr_text = 'We decided on marker' || $2 || ' for the ocr test.',
               units_extracted_at = NULL
           WHERE id = $1"#,
    )
    .bind(image_id)
    .bind(&marker)
    .execute(&state.pool)
    .await
    .unwrap();

    drain_extraction(&state).await;

    let units = sqlx::query(
        r#"SELECT u.kind::text AS kind, u.statement
           FROM atomic_units u
           JOIN atomic_unit_provenance p ON p.atomic_unit_id = u.id
           WHERE p.image_id = $1"#,
    )
    .bind(image_id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert!(
        units
            .iter()
            .any(|r| r.get::<String, _>("kind") == "decision"
                && r.get::<String, _>("statement").contains(&marker)),
        "expected a decision unit extracted from the injected OCR text"
    );
}

#[tokio::test]
async fn chat_messages_produce_units_with_dedup_across_sources() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());

    // Two conversations asserting the same normalized statement: one unit,
    // two provenance rows.
    let marker = Uuid::new_v4().simple().to_string();
    let statement = format!("I use ChunkDB{marker} for storage");
    for conv in ["a", "b"] {
        let export = json!({
            "platform": "generic",
            "data": {
                "schema": "gather-generic-v1",
                "conversations": [{
                    "id": format!("conv-{marker}-{conv}"),
                    "messages": [
                        {"role": "user", "content": format!("{statement}."),
                         "created_at": "2026-02-01T09:00:00Z"}
                    ]
                }]
            }
        });
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/ingest/chat-export")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(export.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
    }

    drain_extraction(&state).await;

    let rows = sqlx::query(
        r#"SELECT u.id, u.kind::text AS kind, u.valid_from, u.confidence,
                  (SELECT count(*) FROM atomic_unit_provenance p
                   WHERE p.atomic_unit_id = u.id) AS provenance_count
           FROM atomic_units u WHERE u.statement LIKE '%' || $1 || '%'"#,
    )
    .bind(&marker)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "same normalized statement must be one unit");
    let row = &rows[0];
    assert_eq!(row.get::<String, _>("kind"), "fact");
    assert_eq!(row.get::<i64, _>("provenance_count"), 2);
    // valid_from from the message timestamp; user-authored bonus applied (0.6+0.1).
    assert_eq!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("valid_from")
            .unwrap()
            .to_rfc3339(),
        "2026-02-01T09:00:00+00:00"
    );
    assert!((row.get::<f32, _>("confidence") - 0.7).abs() < 0.01);

    // Units are visible through the public API.
    let res = app
        .clone()
        .oneshot(
            Request::get("/api/v1/atomic-units?kind=fact&limit=200")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let listed = body_json(res).await;
    assert!(listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|u| u["statement"].as_str().unwrap().contains(&marker)));
}
