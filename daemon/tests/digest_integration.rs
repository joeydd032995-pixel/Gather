//! Document digests, end to end against a real Postgres (pgvector). Skipped
//! without DATABASE_URL, like the other integration tests.
//!
//! A document is uploaded and read; its digest (key sentences, topics,
//! outline) appears at GET /artifacts/{id}/digest. Then a mock local model
//! (bound to loopback; nothing leaves the machine) rewords a second document:
//! what the text supports is kept, what the model invented is dropped. One
//! test fn, so the two phases never race for each other's documents.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::extract;
use gather_daemon::extract::ollama::OllamaClient;
use gather_daemon::{db, routes, AppState};

async fn test_state(config: Config) -> AppState {
    let pool = db::connect(&config.database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    AppState {
        pool,
        config: Arc::new(config),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        ollama: None,
        rate_limiter: None,
    }
}

/// A stand-in local model: an answer that is part grounded, part invented.
/// It refuses to embed anything.
async fn spawn_mock_ollama(grounded_takeaway: String) -> String {
    let app = Router::new()
        .route(
            "/api/chat",
            post(move |Json(_body): Json<Value>| {
                let grounded = grounded_takeaway.clone();
                async move {
                    let content = json!({
                        "summary": "A plan to move the nightly backup target to the new host before the deadline.",
                        "takeaways": [
                            grounded,
                            "The company will also open a Berlin office and hire twelve engineers."
                        ],
                        "open_questions": [],
                        "units": []
                    });
                    Json(json!({ "message": { "content": content.to_string() } }))
                }
            }),
        )
        // No embeddings: stored ones would leak into other tests' entity
        // suggestions in the shared database. A refused request just leaves
        // them empty.
        .route(
            "/api/embed",
            post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "no embeddings here") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn upload_markdown(state: &AppState, filename: &str, text: &str) -> Uuid {
    let boundary = "gatherdigestboundary";
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

async fn get_digest(state: &AppState, id: Uuid) -> (StatusCode, Value) {
    let response = routes::build_router(state.clone())
        .oneshot(
            Request::get(format!("/api/v1/artifacts/{id}/digest"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Run extraction passes until the document's digest exists (and, if
/// `method_prefix` is given, has that method).
async fn digest_when_ready(
    state: &AppState,
    client: Option<&OllamaClient>,
    id: Uuid,
    method_prefix: &str,
) -> Value {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, client)
            .await
            .expect("extraction pass");
        let (status, body) = get_digest(state, id).await;
        if status == StatusCode::OK
            && body["method"]
                .as_str()
                .is_some_and(|m| m.starts_with(method_prefix))
        {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("no digest with method {method_prefix:?} for {id}");
}

fn plan(m: &str) -> String {
    format!(
        "# Backup migration plan {m}\n\n\
         The backup migration moves the nightly backup target from the old server to Hetzner{m}. \
         We decided on Hetzner{m} for the backup target because the monthly cost is lower. \
         The migration must finish before the 2026-03-01 deadline, otherwise the old contract renews. \
         Thanks everyone for joining the call today. \
         The risk is that the first full backup to the new target takes longer than the nightly window. \
         Please see the attached spreadsheet for details. \
         The backup target will keep thirty days of nightly backups and one monthly archive.\n\n\
         ## Next steps\n\n\
         - Dana owns the migration and reports progress every Friday.\n\
         - Test a full restore from the new backup target before the deadline.\n"
    )
}

#[tokio::test]
async fn a_document_gets_a_digest_and_a_model_cannot_add_to_it() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return;
    };

    // ---- Phase 1: the plain digest, no model.
    let state = test_state(Config::for_tests(database_url.clone())).await;
    let m = Uuid::new_v4().simple().to_string();
    let id = upload_markdown(&state, &format!("plan-{m}.md"), &plan(&m)).await;

    let (status, _) = get_digest(&state, Uuid::new_v4()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown file has no digest"
    );

    let digest = digest_when_ready(&state, None, id, "extractive").await;
    let points: Vec<&str> = digest["key_points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap())
        .collect();
    let joined = points.join(" | ");
    assert!(
        joined.contains(&format!("We decided on Hetzner{m}")),
        "{joined}"
    );
    assert!(joined.contains("2026-03-01 deadline"), "{joined}");
    assert!(!joined.contains("Thanks everyone"), "{joined}");
    assert!(!digest["summary"].as_str().unwrap().is_empty());
    assert!(digest["topics"].as_array().unwrap().iter().any(|t| t
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("backup target")));
    let headings: Vec<&str> = digest["outline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["text"].as_str().unwrap())
        .collect();
    assert!(headings.contains(&"Next steps"), "{headings:?}");
    assert_eq!(digest["takeaways"], json!([]), "no model, no takeaways");

    // A file with nothing to say still gets a (thin) digest, once.
    let thin_id = upload_markdown(
        &state,
        &format!("thin-{m}.md"),
        &format!("# {m}\n\nHello there.\n"),
    )
    .await;
    let thin = digest_when_ready(&state, None, thin_id, "extractive").await;
    assert_eq!(thin["key_points"], json!([]));
    assert_eq!(thin["summary"], json!(""));

    // ---- Phase 2: a local model rewords; only what the text supports stays.
    let m2 = Uuid::new_v4().simple().to_string();
    let mut config = Config::for_tests(database_url);
    config.ollama_url = Some(
        spawn_mock_ollama(format!(
            "The team chose Hetzner{m2} for the backup target because the monthly cost is lower."
        ))
        .await,
    );
    config.ollama_model = Some("mock-chat".to_string());
    let state = test_state(config).await;
    let client = OllamaClient::from_config(&state.config)
        .expect("client")
        .expect("ollama is configured");
    let id2 = upload_markdown(&state, &format!("plan-{m2}.md"), &plan(&m2)).await;
    let digest = digest_when_ready(&state, Some(&client), id2, "llm:").await;
    assert_eq!(digest["method"], json!("llm:mock-chat"));
    let takeaways: Vec<&str> = digest["takeaways"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(takeaways.len(), 1, "{takeaways:?}");
    assert!(takeaways[0].contains(&format!("Hetzner{m2}")));
    assert!(
        !takeaways.iter().any(|t| t.contains("Berlin")),
        "what the document never said is not kept"
    );
    assert!(digest["summary"]
        .as_str()
        .unwrap()
        .contains("backup target"));
    // The key sentences are still the file's own.
    assert!(!digest["key_points"].as_array().unwrap().is_empty());
}
