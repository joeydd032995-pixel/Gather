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
        rate_limiter: None,
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
              (SELECT count(*) FROM documents d JOIN artifacts a ON a.id = d.artifact_id
               WHERE extraction_status IN ('pending','processing') AND a.retracted_at IS NULL)
            + (SELECT count(*) FROM images i JOIN artifacts a ON a.id = i.artifact_id
               WHERE ocr_status IN ('pending','processing') AND a.retracted_at IS NULL)
            + (SELECT count(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id
               JOIN artifacts a ON a.id = c.artifact_id
               WHERE units_extracted_at IS NULL AND a.retracted_at IS NULL)
            + (SELECT count(*) FROM document_segments s JOIN documents d ON d.id = s.document_id
               JOIN artifacts a ON a.id = d.artifact_id
               WHERE units_extracted_at IS NULL AND a.retracted_at IS NULL)
            + (SELECT count(*) FROM images i JOIN artifacts a ON a.id = i.artifact_id
               WHERE a.retracted_at IS NULL AND units_extracted_at IS NULL AND ocr_status = 'completed'
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

/// A section that always fails to save is set aside with its error, and the
/// rest of the queue is read anyway: it used to fail every pass first and
/// keep everything behind it waiting.
#[tokio::test]
async fn a_failing_chunk_is_set_aside_and_the_rest_are_read() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let marker = Uuid::new_v4().simple().to_string();
    let poison = format!("PoisonDB{marker}");
    let good = format!("GoodDB{marker}");

    // Fail any unit mentioning `poison`, and only those: other tests run
    // against the same database meanwhile.
    let func = format!("fail_poison_{marker}");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {func}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
           IF NEW.statement LIKE '%{poison}%' THEN RAISE EXCEPTION 'poisoned unit'; END IF; \
           RETURN NEW; END $$"
    )))
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER {func} BEFORE INSERT ON atomic_units \
         FOR EACH ROW EXECUTE FUNCTION {func}()"
    )))
    .execute(&state.pool)
    .await
    .unwrap();

    let export = json!({
        "platform": "generic",
        "data": {
            "schema": "gather-generic-v1",
            "conversations": [{
                "id": format!("conv-{marker}"),
                "messages": [
                    {"role": "user", "content": format!("I use {poison} for storage."),
                     "created_at": "2026-02-01T09:00:00Z"},
                    {"role": "user", "content": format!("I use {good} for storage."),
                     "created_at": "2026-02-01T09:01:00Z"}
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

    drain_extraction(&state).await;

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {func} ON atomic_units"
    )))
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP FUNCTION {func}()")))
        .execute(&state.pool)
        .await
        .unwrap();

    let rows = sqlx::query(
        "SELECT content, units_extracted_at IS NOT NULL AS done, units_extract_error \
         FROM messages WHERE content LIKE '%' || $1 || '%'",
    )
    .bind(&marker)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert!(r.get::<bool, _>("done"), "both messages leave the queue");
        let error: Option<String> = r.get("units_extract_error");
        if r.get::<String, _>("content").contains(&poison) {
            assert!(
                error
                    .as_deref()
                    .is_some_and(|e| e.contains("poisoned unit")),
                "the failing message keeps its error: {error:?}"
            );
        } else {
            assert_eq!(error, None);
        }
    }
    let (good_units,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM atomic_units WHERE statement LIKE '%' || $1 || '%'")
            .bind(&good)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        good_units, 1,
        "the message after the failing one is still read"
    );

    // The status endpoint counts what was set aside.
    let res = app
        .clone()
        .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let status = body_json(res).await;
    assert!(
        status["reading"]["failed"].as_i64().unwrap() >= 1,
        "{status}"
    );
    assert_eq!(status["ai"]["enabled"], false);
    assert_eq!(
        status["ai"]["model"],
        Value::Null,
        "no model named while AI is off"
    );
}

// ---------------------------------------------------------------------------
// Re-reading earlier files with the AI model
// ---------------------------------------------------------------------------

/// A stand-in for Ollama: finds the `Tool…` word of a chunk and answers with
/// a unit about it, fails on a `Broken…` word, and reads nothing otherwise.
async fn spawn_fake_ollama() -> String {
    use axum::routing::post;
    use axum::Json;

    fn word(text: &str, prefix: &str) -> Option<String> {
        text.split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .find(|w| w.starts_with(prefix) && w.len() == prefix.len() + 32)
            .map(String::from)
    }
    async fn chat(Json(body): Json<Value>) -> Result<Json<Value>, StatusCode> {
        let text = body
            .pointer("/messages/1/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        if word(text, "Broken").is_some() {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        let units = match word(text, "Tool") {
            Some(tool) => json!([{
                "kind": "fact",
                "statement": format!("The user relies on {tool}"),
                "evidence_span": tool,
                "confidence": 0.9,
            }]),
            None => json!([]),
        };
        Ok(Json(
            json!({ "message": { "content": json!({ "units": units }).to_string() } }),
        ))
    }
    async fn embed(Json(body): Json<Value>) -> Json<Value> {
        let n = body["input"].as_array().map_or(0, Vec::len);
        Json(json!({ "embeddings": vec![vec![0.0f32; 768]; n] }))
    }

    let app = axum::Router::new()
        .route("/api/chat", post(chat))
        .route("/api/embed", post(embed));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn finish_reread(state: &AppState, client: &extract::ollama::OllamaClient) {
    for _ in 0..3000 {
        extract::reread::run_pass(&state.pool, &state.config, client)
            .await
            .expect("re-read pass");
        if extract::reread::running(&state.pool)
            .await
            .unwrap()
            .is_none()
        {
            return;
        }
    }
    panic!("the re-read never finished");
}

#[tokio::test]
async fn re_reading_adds_what_the_model_finds_in_earlier_files() {
    let Some(mut state) = test_state().await else {
        return;
    };
    let marker = Uuid::new_v4().simple().to_string();
    let tool = format!("Tool{marker}");
    let broken = format!("Broken{marker}");
    let model = format!("fake-{marker}");
    let stored = format!("ollama:{model}");

    // Earlier files, read by the rules alone (no model was set up yet).
    let app = routes::build_router(state.clone());
    let export = json!({
        "platform": "generic",
        "data": {
            "schema": "gather-generic-v1",
            "conversations": [{
                "id": format!("conv-reread-{marker}"),
                "messages": [
                    {"role": "user", "content": format!("I use {tool} for storage."),
                     "created_at": "2026-03-01T09:00:00Z"},
                    {"role": "user", "content": format!("I use {broken} for backups."),
                     "created_at": "2026-03-01T09:01:00Z"}
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
    drain_extraction(&state).await;

    // Now a model is set up.
    let mut config = Config::for_tests(std::env::var("DATABASE_URL").unwrap());
    config.ollama_url = Some(spawn_fake_ollama().await);
    config.ollama_model = Some(model.clone());
    let client = extract::ollama::OllamaClient::from_config(&config)
        .unwrap()
        .unwrap();
    state.config = Arc::new(config);
    state.ollama = Some(Arc::new(client));
    let client = state.ollama.clone().unwrap();
    let app = routes::build_router(state.clone());

    let post = |path: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(Request::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = res.status();
            (status, body_json(res).await)
        }
    };

    // Started, then stopped: nothing more is read.
    let (status, started) = post("/api/v1/reread").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(started["job"]["status"], "running");
    assert_eq!(started["job"]["model"], model);
    assert!(started["job"]["total"].as_i64().unwrap() >= 2);
    let (_, again) = post("/api/v1/reread").await;
    assert_eq!(
        again["job"]["id"], started["job"]["id"],
        "one job at a time"
    );
    let (_, stopped) = post("/api/v1/reread/cancel").await;
    assert_eq!(stopped["job"]["status"], "cancelled");
    finish_reread(&state, &client).await;
    let (unread,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM messages WHERE content LIKE '%' || $1 || '%' \
         AND units_llm_model IS NOT NULL",
    )
    .bind(&marker)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(unread, 0, "a stopped re-read reads nothing");

    // Started and left to finish.
    let (_, started) = post("/api/v1/reread").await;
    let job_id: Uuid = started["job"]["id"].as_str().unwrap().parse().unwrap();
    finish_reread(&state, &client).await;

    let units = sqlx::query(
        "SELECT u.extraction_method::text AS method, u.extraction_model, \
                (SELECT count(*) FROM atomic_unit_provenance p WHERE p.atomic_unit_id = u.id) AS sources \
         FROM atomic_units u WHERE u.statement LIKE '%' || $1 || '%'",
    )
    .bind(&tool)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    // The rules' unit about the tool and the model's own.
    let model_units: Vec<_> = units
        .iter()
        .filter(|r| r.get::<String, _>("method") == "llm_local")
        .collect();
    assert_eq!(model_units.len(), 1, "the model adds its own unit");
    assert_eq!(
        model_units[0].get::<Option<String>, _>("extraction_model"),
        Some(stored.clone())
    );
    assert_eq!(model_units[0].get::<i64, _>("sources"), 1);

    let rows = sqlx::query(
        "SELECT content, units_llm_model, units_reread_job, units_extracted_at IS NOT NULL AS done \
         FROM messages WHERE content LIKE '%' || $1 || '%'",
    )
    .bind(&marker)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert!(r.get::<bool, _>("done"));
        if r.get::<String, _>("content").contains(&broken) {
            // The model failed on it: not marked as read by the model, and
            // left alone by this job instead of retried forever.
            assert_eq!(r.get::<Option<String>, _>("units_llm_model"), None);
            assert_eq!(r.get::<Option<Uuid>, _>("units_reread_job"), Some(job_id));
        } else {
            assert_eq!(
                r.get::<Option<String>, _>("units_llm_model"),
                Some(stored.clone())
            );
        }
    }

    let (status, body) = {
        let res = app
            .clone()
            .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        (res.status(), body_json(res).await)
    };
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reread"]["status"], "done");
    assert!(body["reread"]["failed"].as_i64().unwrap() >= 1);

    // A second job reads nothing twice: the same unit, still one source.
    post("/api/v1/reread").await;
    finish_reread(&state, &client).await;
    let (sources,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM atomic_unit_provenance p JOIN atomic_units u ON u.id = p.atomic_unit_id \
         WHERE u.statement LIKE '%' || $1 || '%' AND u.extraction_method = 'llm_local'",
    )
    .bind(&tool)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(sources, 1);
}

#[tokio::test]
async fn re_reading_needs_an_ai_model() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state);
    let res = app
        .oneshot(Request::post("/api/v1/reread").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_perplexity_markdown_export_arrives_as_a_conversation() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let marker = Uuid::new_v4().simple().to_string();
    let filename = format!("perplexity-{marker}.md");
    let markdown = format!(
        "<img src=\"https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png\"/>\n\n\
         # Which database suits notes {marker}?\n\n\
         Postgres suits notes {marker} well.[^1_1]\n\n\
         <div align=\"center\">⁂</div>\n\n\
         [^1_1]: https://example.com/postgres\n\
         [^1_2]: projects.some.memory_tag\n\n\
         ---\n\n\
         # And for search?\n\n\
         Use a tsvector column.[^2_1]\n\n\
         <div align=\"center\">⁂</div>\n\n\
         [^2_1]: https://example.com/fts\n"
    );
    delete_fixture_artifact(&state, &filename).await;

    let res = app
        .clone()
        .oneshot(multipart_request(
            &filename,
            "text/markdown",
            markdown.as_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = body_json(res).await;
    let file = &body["files"][0];
    assert_eq!(file["kind"], "chat_export", "{body}");
    assert_eq!(file["status"], "accepted");

    let rows = sqlx::query(
        "SELECT m.role, m.content, c.source_platform, a.original_filename \
         FROM messages m \
         JOIN conversations c ON c.id = m.conversation_id \
         JOIN artifacts a ON a.id = c.artifact_id \
         WHERE a.original_filename = $1 ORDER BY m.seq",
    )
    .bind(&filename)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 4, "two questions, two answers");
    let roles: Vec<String> = rows.iter().map(|r| r.get("role")).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(rows[0].get::<String, _>("source_platform"), "perplexity");
    let first_answer: String = rows[1].get("content");
    assert!(first_answer.contains("[1] https://example.com/postgres"));
    assert!(!first_answer.contains("memory_tag"));

    // The same file again is recognised, not stored twice.
    let res = app
        .oneshot(multipart_request(
            &filename,
            "text/markdown",
            markdown.as_bytes(),
        ))
        .await
        .unwrap();
    let again = body_json(res).await;
    assert_eq!(again["files"][0]["status"], "deduplicated");
}

#[tokio::test]
async fn withdrawal_during_extraction_cannot_create_claims() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let marker = Uuid::new_v4().simple().to_string();
    let text = format!("I use Withdrawn{marker} for storage.");
    let export = json!({
        "platform": "generic",
        "data": {
            "schema": "gather-generic-v1",
            "conversations": [{
                "id": format!("withdrawal-{marker}"),
                "messages": [{"role": "user", "content": text}]
            }]
        }
    });
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/ingest/chat-export")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(export.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let (message_id, artifact_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT m.id, c.artifact_id FROM messages m
         JOIN conversations c ON c.id = m.conversation_id WHERE m.content = $1",
    )
    .bind(&text)
    .fetch_one(&state.pool)
    .await
    .unwrap();

    // This chunk represents work already taken before withdrawal/model latency.
    let chunk = extract::persist::Chunk {
        anchor: extract::persist::ChunkAnchor::Message(message_id),
        artifact_id,
        text: text.clone(),
        source_time: None,
        user_authored: true,
        ocr_confidence: None,
    };
    sqlx::query("UPDATE artifacts SET retracted_at = now() WHERE id = $1")
        .bind(artifact_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let units: Vec<_> = extract::rules::extract_units(&text)
        .into_iter()
        .map(|unit| (unit, "rule_based", None))
        .collect();
    let result = extract::persist::persist_chunk_units(
        &state.pool,
        &chunk,
        &extract::persist::Claim::Fresh { llm_model: None },
        &units,
        0.0,
        0.0,
    )
    .await
    .unwrap();
    assert!(result.is_none());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM atomic_unit_provenance WHERE artifact_id = $1")
            .bind(artifact_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);

    // Simple browser requests must be stopped before the mutation handler.
    for origin in ["https://untrusted.example", "null"] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/ingest/chat-export")
                    .header(header::ORIGIN, origin)
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(export.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    sqlx::query("DELETE FROM artifacts WHERE id = $1")
        .bind(artifact_id)
        .execute(&state.pool)
        .await
        .unwrap();
}
