//! End-to-end API tests against a real Postgres (with pgvector).
//!
//! These run when DATABASE_URL is set (CI provides a pgvector service
//! container; locally, `docker compose up postgres` and
//! `export DATABASE_URL=postgres://gather:gather@localhost:5432/gather`).
//! Without DATABASE_URL they are skipped so `cargo test` stays green offline.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use gather_daemon::config::Config;
use gather_daemon::{db, routes, AppState};

async fn test_state() -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return None;
    };
    let pool = db::connect(&database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    let config = Config::for_tests(database_url);
    Some(AppState {
        pool,
        config: Arc::new(config),
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

#[tokio::test]
async fn health_ready_and_ingest_flow() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());

    // healthz
    let res = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // readyz confirms pgvector is loaded
    let res = app
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // ingest a chat export (claude adapter), twice: second is deduplicated
    let export = json!({
        "platform": "claude",
        "data": [{
            "uuid": format!("it-{}", uuid::Uuid::new_v4()),
            "name": "integration test conversation",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:10:00Z",
            "chat_messages": [
                {"uuid": "m1", "sender": "human", "text": "My favorite database is Postgres",
                 "created_at": "2026-01-01T00:00:01Z"},
                {"uuid": "m2", "sender": "assistant", "text": "Noted!",
                 "created_at": "2026-01-01T00:00:02Z"}
            ]
        }]
    });
    let req = |b: &Value| {
        Request::post("/api/v1/ingest/chat-export")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap()
    };
    let res = app.clone().oneshot(req(&export)).await.unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let first = body_json(res).await;
    assert_eq!(first["deduplicated"], json!(false));
    assert_eq!(first["conversations"], json!(1));
    assert_eq!(first["messages"], json!(2));

    let res = app.clone().oneshot(req(&export)).await.unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let second = body_json(res).await;
    assert_eq!(second["deduplicated"], json!(true));
    assert_eq!(second["artifact_id"], first["artifact_id"]);

    // the artifact is queryable with its conversations attached
    let res = app
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/v1/artifacts/{}",
                first["artifact_id"].as_str().unwrap()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let artifact = body_json(res).await;
    assert_eq!(artifact["kind"], json!("chat_export"));
    assert_eq!(artifact["conversations"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn multipart_upload_segments_markdown() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state);

    let unique = uuid::Uuid::new_v4();
    let markdown = format!(
        "# Decision Log {unique}\n\nWe chose Hetzner CX22 for backups.\n\n## Budget\n\nThe ceiling is $75 per month.\n"
    );
    let boundary = "gatherboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"decisions.md\"\r\nContent-Type: text/markdown\r\n\r\n{markdown}\r\n--{boundary}--\r\n"
    );

    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/ingest/files")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let out = body_json(res).await;
    let file = &out["files"][0];
    assert_eq!(file["status"], json!("accepted"));
    assert_eq!(file["kind"], json!("document_markdown"));
    assert_eq!(file["segments"], json!(2)); // one per heading

    // full-text search finds the uploaded content
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/search/semantic")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"text": "Hetzner backups", "scope": "document_segments"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let hits = body_json(res).await;
    assert!(
        !hits["hits"].as_array().unwrap().is_empty(),
        "expected at least one full-text hit for the uploaded markdown"
    );
}

#[tokio::test]
async fn export_bundle_has_manifest() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state);

    let res = app
        .oneshot(Request::get("/api/v1/export").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let first_line = std::str::from_utf8(&bytes).unwrap().lines().next().unwrap();
    let manifest: Value = serde_json::from_str(first_line).unwrap();
    assert_eq!(manifest["type"], json!("manifest"));
    assert_eq!(manifest["row"]["format"], json!("gather-bundle-v1"));
}

/// One fixture export per newly added platform adapter, pushed through the
/// real router; asserts platform tagging and normalized message counts.
#[tokio::test]
async fn new_platform_adapters_ingest_through_the_api() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let salt = uuid::Uuid::new_v4().simple().to_string();

    let cases: Vec<(&str, Value, usize, usize)> = vec![
        (
            "gemini",
            json!([{
                "title": format!("Prompted what is plan {salt}"),
                "time": "2026-05-01T08:00:00Z",
                "safeHtmlItem": {"htmlValue": "<p>The plan is <b>local-first</b>.</p>"}
            }]),
            1, // conversations
            2, // messages
        ),
        (
            "grok",
            json!({"conversations": [{
                "conversation_id": format!("grok-{salt}"),
                "title": "vps",
                "create_time": 1767225600000i64,
                "responses": [
                    {"sender": "human", "message": format!("salt {salt}: pick a region")},
                    {"sender": "assistant", "message": "Nuremberg."}
                ]
            }]}),
            1,
            2,
        ),
        (
            "perplexity",
            json!({"threads": [{
                "id": format!("px-{salt}"),
                "title": "research",
                "entries": [{"query": format!("salt {salt}: cheapest EU VPS?"),
                             "answer": "Hetzner CX22.",
                             "timestamp": "2026-05-01T09:00:00Z"}]
            }]}),
            1,
            2,
        ),
        (
            "copilot",
            json!({
                "sessionId": format!("cop-{salt}"),
                "requesterUsername": "joey",
                "requests": [{
                    "message": {"text": format!("salt {salt}: add a readyz probe")},
                    "timestamp": 1767225600000i64,
                    "response": [{"value": "Added /readyz."}]
                }]
            }),
            1,
            2,
        ),
    ];

    for (platform, data, conversations, messages) in cases {
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/ingest/chat-export")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"platform": platform, "data": data}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::ACCEPTED,
            "{platform} ingest failed"
        );
        let out = body_json(res).await;
        assert_eq!(out["deduplicated"], json!(false), "{platform}");
        assert_eq!(out["conversations"], json!(conversations), "{platform}");
        assert_eq!(out["messages"], json!(messages), "{platform}");

        // Artifact carries the platform + format-version tags.
        let res = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/artifacts/{}",
                    out["artifact_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        let artifact = body_json(res).await;
        assert_eq!(artifact["source_platform"], json!(platform));
        assert!(artifact["source_format_version"].as_str().unwrap().len() > 3);
    }
}
