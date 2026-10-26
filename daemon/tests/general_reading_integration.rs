//! Reading ordinary sentences when there is no reading model, end to end
//! against a real Postgres (pgvector). Skipped without DATABASE_URL.
//!
//! These live apart from tests/extraction_integration.rs on purpose. An
//! extraction pass claims every pending row in the database, whoever uploaded
//! it, and the sentence reader runs only when the pass was started with
//! `extraction_general` on and no model. Beside tests that run passes with it
//! off, a sibling's pass could claim a note first and read it with the rules
//! alone. Here every pass is started the same way, and the tests take turns
//! so that the one that runs a pass with a model is not overtaken by one
//! without.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::Row;
use tokio::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::extract;
use gather_daemon::scan;
use gather_daemon::{db, routes, AppState};

/// Held for the whole of each test.
static TURN: Mutex<()> = Mutex::const_new(());

async fn test_state() -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return None;
    };
    let pool = db::connect(&database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    let mut config = Config::for_tests(database_url);
    config.extraction_general = true;
    Some(AppState {
        pool,
        config: Arc::new(config),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        ollama: None,
        rate_limiter: None,
    })
}

fn multipart_request(filename: &str, content_type: &str, bytes: &[u8]) -> Request<Body> {
    let boundary = "gathergeneralboundary";
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

/// Run extraction passes until every queue is empty.
async fn drain_extraction(state: &AppState, ollama: Option<&extract::ollama::OllamaClient>) {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, ollama)
            .await
            .expect("extraction pass");
        let (busy,): (i64,) = sqlx::query_as(
            r#"
            SELECT
              (SELECT count(*) FROM documents d JOIN artifacts a ON a.id = d.artifact_id
               WHERE extraction_status IN ('pending','processing') AND a.retracted_at IS NULL)
            + (SELECT count(*) FROM document_segments s JOIN documents d ON d.id = s.document_id
               JOIN artifacts a ON a.id = d.artifact_id
               WHERE units_extracted_at IS NULL AND a.retracted_at IS NULL)
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

/// A stand-in for Ollama that reads nothing in anything.
async fn spawn_fake_ollama() -> String {
    use axum::routing::post;
    use axum::Json;

    async fn chat(Json(_): Json<Value>) -> Json<Value> {
        Json(json!({ "message": { "content": json!({ "units": [] }).to_string() } }))
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

/// Remove any prior run's artifact of this name, so the upload is read afresh
/// rather than skipped as a duplicate.
async fn delete_fixture_artifact(state: &AppState, filename: &str) {
    sqlx::query("DELETE FROM artifacts WHERE original_filename = $1")
        .bind(filename)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// Units this artifact's text produced, as (statement, attrs).
async fn units_of(state: &AppState, filename: &str) -> Vec<(String, Value)> {
    sqlx::query(
        "SELECT DISTINCT u.statement, u.attrs FROM atomic_units u \
         JOIN atomic_unit_provenance p ON p.atomic_unit_id = u.id \
         JOIN artifacts a ON a.id = p.artifact_id WHERE a.original_filename = $1 \
         ORDER BY u.statement",
    )
    .bind(filename)
    .fetch_all(&state.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("statement"), r.get("attrs")))
    .collect()
}

async fn upload_text(state: &AppState, name: &str, text: &str) {
    delete_fixture_artifact(state, name).await;
    let res = routes::build_router(state.clone())
        .oneshot(multipart_request(name, "text/markdown", text.as_bytes()))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED, "{name}");
}

/// Ordinary notes hold few of the rules' fixed sentence shapes, so without a
/// reading model almost nothing was found in them, and nothing downstream
/// (graph, contradictions, merges, the review tray) had anything to work on.
#[tokio::test]
async fn ordinary_sentences_are_read_without_a_model() {
    let _turn = TURN.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let m = Uuid::new_v4().simple().to_string();
    let name = format!("ordinary-{m}.md");
    upload_text(
        &state,
        &name,
        &format!(
            "The Zorblat{m} venue holds 300 people. Marta confirmed that the audit starts on 9 September.\n\n\
             Thanks so much for yesterday, it was lovely to see everyone! Can you forward me the agenda?\n"
        ),
    )
    .await;
    drain_extraction(&state, None).await;

    let units = units_of(&state, &name).await;
    let read: Vec<&(String, Value)> = units
        .iter()
        .filter(|(_, attrs)| attrs["reader"] == "general")
        .collect();
    assert_eq!(read.len(), 2, "{units:?}");
    assert!(
        units
            .iter()
            .all(|(s, _)| !s.contains("Thanks") && !s.contains("agenda")),
        "small talk and requests state nothing: {units:?}"
    );
    // The figure carries what the contradiction scan compares, and its subject
    // became a thing in the graph.
    let venue = read.iter().find(|(s, _)| s.contains("venue")).unwrap();
    assert_eq!(venue.1["pattern"], "numeric");
    assert_eq!(venue.1["value"], "300");
    assert_eq!(venue.1["unit"], "people");
    let (entities,): (i64,) = sqlx::query_as("SELECT count(*) FROM entities WHERE name ILIKE $1")
        .bind(format!("%Zorblat{m}%"))
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(entities >= 1, "the venue is an entity");
    // Below the rules' own 0.6.
    let (confidence,): (f32,) = sqlx::query_as(
        "SELECT max(u.confidence) FROM atomic_units u WHERE u.statement LIKE '%' || $1 || '%'",
    )
    .bind(format!("Zorblat{m}"))
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(confidence < 0.7, "{confidence}");
}

/// A model's own statements are better than sentences copied whole, so the
/// ordinary reader is for when there is none.
#[tokio::test]
async fn a_reading_model_takes_the_place_of_the_ordinary_reader() {
    let _turn = TURN.lock().await;
    let Some(mut state) = test_state().await else {
        return;
    };
    let mut config = (*state.config).clone();
    config.ollama_url = Some(spawn_fake_ollama().await);
    config.ollama_model = Some("chat-model".to_string());
    let client = extract::ollama::OllamaClient::from_config(&config)
        .unwrap()
        .unwrap();
    state.config = Arc::new(config);

    let m = Uuid::new_v4().simple().to_string();
    let name = format!("modelled-{m}.md");
    upload_text(
        &state,
        &name,
        &format!("The Zorblat{m} venue holds 300 people. Marta confirmed that the audit starts on 9 September.\n"),
    )
    .await;
    drain_extraction(&state, Some(&client)).await;
    let units = units_of(&state, &name).await;
    assert!(
        units.iter().all(|(_, attrs)| attrs["reader"] != "general"),
        "{units:?}"
    );
}

/// The point of reading ordinary sentences: two notes that disagree about a
/// figure are found, with no model at all.
#[tokio::test]
async fn two_notes_that_disagree_about_a_figure_are_set_against_each_other_without_a_model() {
    let _turn = TURN.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let m = Uuid::new_v4().simple().to_string();
    for (name, text) in [
        (
            format!("depot-a-{m}.md"),
            format!(
                "Notes from the March visit. The Quorvex{m} depot holds 300 people when full.\n"
            ),
        ),
        (
            format!("depot-b-{m}.md"),
            format!(
                "Notes from the June visit. The Quorvex{m} depot holds 250 people when full.\n"
            ),
        ),
    ] {
        upload_text(&state, &name, &text).await;
    }
    for _ in 0..200 {
        drain_extraction(&state, None).await;
        scan::run_one_scan(&state.pool, &state.config, None)
            .await
            .expect("scan pass");
        let (open,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM atomic_units WHERE contradiction_scanned_at IS NULL \
             AND status = 'active' AND statement LIKE '%' || $1 || '%'",
        )
        .bind(format!("Quorvex{m}"))
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if open == 0 {
            break;
        }
    }
    let found: Vec<(String, String)> = sqlx::query(
        "SELECT c.detection_method, c.status::text AS status FROM contradictions c \
         JOIN atomic_units a ON a.id = c.unit_a_id JOIN atomic_units b ON b.id = c.unit_b_id \
         WHERE a.statement LIKE '%' || $1 || '%' AND b.statement LIKE '%' || $1 || '%'",
    )
    .bind(format!("Quorvex{m}"))
    .fetch_all(&state.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("detection_method"), r.get("status")))
    .collect();
    assert_eq!(
        found,
        vec![("rule:numeric-mismatch".to_string(), "open".to_string())]
    );
}
