//! Product-level MVP acceptance gauntlet.
//!
//! This deliberately overlaps several narrower integration suites. The point is
//! to prove that their hand-offs work together on one corpus:
//!
//! ChatGPT export + agent JSONL + manual document
//!   -> extraction + provenance
//!   -> entity graph + search
//!   -> cross-source contradiction detection + resolution
//!   -> portable export + idempotent re-import.
//!
//! The existing CI restore drill remains the destructive export -> wipe ->
//! restore/import fidelity proof. This test is non-destructive so it can run in
//! parallel with the rest of `cargo test --all-targets` against CI's pgvector
//! service.
//!
//! CI sets DATABASE_URL. Without it this test skips, matching the other
//! database-backed integration suites.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::{db, extract, routes, scan, AppState};

async fn test_state() -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping MVP acceptance: DATABASE_URL not set");
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

async fn body_bytes(response: axum::response::Response) -> bytes::Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}

async fn body_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

fn json_request(method: &str, path: impl AsRef<str>, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path.as_ref())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn multipart_markdown(filename: &str, markdown: &str) -> Request<Body> {
    let boundary = "gathermvpacceptanceboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{filename}\"\r\nContent-Type: text/markdown\r\n\r\n\
         {markdown}\r\n--{boundary}--\r\n"
    );
    Request::post("/api/v1/ingest/files")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn drive_workers_until(
    state: &AppState,
    subject: &str,
    tool: &str,
    choice: &str,
) -> Uuid {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .expect("extraction pass");
        scan::run_one_scan(&state.pool, &state.config, None)
            .await
            .expect("scan pass");

        let units_ready: i64 = sqlx::query_scalar(
            r#"SELECT count(*) FROM atomic_units
               WHERE statement LIKE '%' || $1 || '%'
                  OR statement LIKE '%' || $2 || '%'
                  OR statement LIKE '%' || $3 || '%'"#,
        )
        .bind(subject)
        .bind(tool)
        .bind(choice)
        .fetch_one(&state.pool)
        .await
        .unwrap();

        let contradiction: Option<Uuid> = sqlx::query_scalar(
            r#"SELECT c.id
               FROM contradictions c
               JOIN atomic_units a ON a.id = c.unit_a_id
               JOIN atomic_units b ON b.id = c.unit_b_id
               WHERE a.statement LIKE '%' || $1 || '%'
                 AND b.statement LIKE '%' || $1 || '%'
                 AND c.status = 'open'
               LIMIT 1"#,
        )
        .bind(subject)
        .fetch_optional(&state.pool)
        .await
        .unwrap();

        if units_ready >= 4 {
            if let Some(id) = contradiction {
                return id;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("MVP acceptance corpus did not fully extract/scan before deadline");
}

#[tokio::test]
async fn full_mvp_acceptance_gauntlet() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());

    // ---------------------------------------------------------------- health
    let health = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let ready = app
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);

    let marker = Uuid::new_v4().simple().to_string();
    let short = &marker[..10];
    let tool = format!("VectorStore{short}");
    let choice = format!("AgentPlan{short}");
    let subject = format!("Zeta{short} budget");

    // ----------------------------------------------------------- ChatGPT tree
    let chat = json!({
        "platform": "chatgpt",
        "filename": "conversations.json",
        "data": [{
            "conversation_id": format!("chatgpt-{short}"),
            "title": "MVP acceptance",
            "create_time": 1767225600.0,
            "update_time": 1767225660.0,
            "current_node": "n2",
            "mapping": {
                "root": {
                    "id": "root", "message": null, "parent": null,
                    "children": ["n1"]
                },
                "n1": {
                    "id": "n1", "parent": "root", "children": ["n2"],
                    "message": {
                        "id": format!("m1-{short}"),
                        "author": {"role": "user"},
                        "create_time": 1767225601.0,
                        "content": {
                            "content_type": "text",
                            "parts": [format!("I use {tool}.")]
                        }
                    }
                },
                "n2": {
                    "id": "n2", "parent": "n1", "children": [],
                    "message": {
                        "id": format!("m2-{short}"),
                        "author": {"role": "assistant"},
                        "create_time": 1767225602.0,
                        "metadata": {"model_slug": "gpt-5"},
                        "content": {
                            "content_type": "text",
                            "parts": ["Recorded."]
                        }
                    }
                }
            }
        }]
    });

    let ingest_chat = || {
        json_request(
            "POST",
            "/api/v1/ingest/chat-export",
            chat.clone(),
        )
    };

    let first = app.clone().oneshot(ingest_chat()).await.unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first = body_json(first).await;
    assert_eq!(first["deduplicated"], json!(false));
    assert_eq!(first["conversations"], json!(1));
    assert_eq!(first["messages"], json!(2));

    let second = app.clone().oneshot(ingest_chat()).await.unwrap();
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let second = body_json(second).await;
    assert_eq!(second["deduplicated"], json!(true));
    assert_eq!(second["artifact_id"], first["artifact_id"]);
    assert_eq!(second["conversations"], json!(0));
    assert_eq!(second["messages"], json!(0));

    // ------------------------------------------------------------ agent JSONL
    let jsonl = [
        json!({
            "role": "user",
            "content": format!("We decided on {choice}."),
            "timestamp": "2026-01-01T00:02:00Z"
        })
        .to_string(),
        json!({
            "role": "user",
            "content": format!("My {subject} is $75 per month."),
            "timestamp": "2026-01-01T00:03:00Z"
        })
        .to_string(),
        json!({
            "role": "assistant",
            "content": "Acknowledged.",
            "timestamp": "2026-01-01T00:04:00Z"
        })
        .to_string(),
    ]
    .join("\n");

    let agent = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/ingest/agent-log",
            json!({
                "platform": "claude_code",
                "jsonl": jsonl,
                "session_id": format!("session-{short}"),
                "title": "MVP acceptance agent log"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(agent.status(), StatusCode::ACCEPTED);
    let agent = body_json(agent).await;
    assert_eq!(agent["conversations"], json!(1));
    assert_eq!(agent["messages"], json!(3));

    // ------------------------------------------------------------- manual doc
    let markdown = format!(
        "# MVP Acceptance {short}\n\nI prefer local-first testing.\n\nMy {subject} is $50 per month.\n"
    );
    let document = app
        .clone()
        .oneshot(multipart_markdown(
            &format!("mvp-{short}.md"),
            &markdown,
        ))
        .await
        .unwrap();
    assert_eq!(document.status(), StatusCode::ACCEPTED);
    let document = body_json(document).await;
    assert_eq!(document["files"][0]["kind"], json!("document_markdown"));

    // Drive the real extraction + contradiction scanner over this one corpus.
    let contradiction_id =
        drive_workers_until(&state, &subject, &tool, &choice).await;

    // ---------------------------------------------------------- units/prov
    let units = app
        .clone()
        .oneshot(
            Request::get("/api/v1/atomic-units?limit=500")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(units.status(), StatusCode::OK);
    let units = body_json(units).await;
    let items = units["items"].as_array().unwrap();

    assert!(items.iter().any(|u| {
        u["statement"].as_str().unwrap_or("").contains(&tool)
            && u["kind"] == "fact"
            && u["provenance_count"].as_i64().unwrap_or(0) >= 1
    }));
    assert!(items.iter().any(|u| {
        u["statement"].as_str().unwrap_or("").contains(&choice)
            && u["kind"] == "decision"
            && u["provenance_count"].as_i64().unwrap_or(0) >= 1
    }));

    // ---------------------------------------------------------------- search
    let search = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/search/semantic",
            json!({"text": tool, "scope": "atomic_units", "limit": 20}),
        ))
        .await
        .unwrap();
    assert_eq!(search.status(), StatusCode::OK);
    let search = body_json(search).await;
    assert!(search["hits"].as_array().unwrap().iter().any(|h| {
        h["content"]
            .as_str()
            .unwrap_or("")
            .contains(&format!("VectorStore{short}"))
    }));

    // ---------------------------------------------------------- entity graph
    let entities = app
        .clone()
        .oneshot(
            Request::get("/api/v1/entities?q=Me&limit=100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let entities = body_json(entities).await;
    let me = entities["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Me")
        .and_then(|e| e["id"].as_str())
        .expect("Me entity")
        .to_string();

    let graph = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/entities/{me}/graph?depth=2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(graph.status(), StatusCode::OK);
    let graph = body_json(graph).await;
    let node_names: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    assert!(node_names.iter().any(|n| *n == format!("VectorStore{short}")));
    assert!(node_names.iter().any(|n| *n == format!("AgentPlan{short}")));
    assert!(graph["query_ms"].is_number());

    // ------------------------------------------------------- contradiction
    let detail = app
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/v1/contradictions/{contradiction_id}"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::OK);
    let detail = body_json(detail).await;

    let source_a: Vec<&str> = detail["unit_a"]["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["source_platform"].as_str())
        .collect();
    let source_b: Vec<&str> = detail["unit_b"]["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["source_platform"].as_str())
        .collect();
    let mut all_sources = source_a;
    all_sources.extend(source_b);
    assert!(all_sources.contains(&"manual"));
    assert!(all_sources.contains(&"claude_code"));

    let statement_a = detail["unit_a"]["statement"].as_str().unwrap();
    let resolution = if statement_a.contains("$75") {
        "resolved_a"
    } else {
        "resolved_b"
    };

    let resolved = app
        .clone()
        .oneshot(json_request(
            "POST",
            format!("/api/v1/contradictions/{contradiction_id}/resolve"),
            json!({
                "resolution": resolution,
                "note": "MVP acceptance keeps the newer value",
                "actor": "mvp-acceptance"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resolved.status(), StatusCode::OK);
    let resolved = body_json(resolved).await;
    assert_eq!(resolved["status"], json!(resolution));

    let loser_status: String = sqlx::query_scalar(
        r#"SELECT status::text FROM atomic_units
           WHERE statement LIKE '%' || $1 || '%' AND statement LIKE '%$50%'
           ORDER BY created_at DESC LIMIT 1"#,
    )
    .bind(&subject)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(loser_status, "superseded");

    // -------------------------------------------------------- export/import
    // The destructive fidelity proof lives in scripts/ci-restore-drill.sh.
    // Here, prove the public import endpoint can consume a live full bundle
    // idempotently without changing the corpus.
    let export = app
        .clone()
        .oneshot(Request::get("/api/v1/export").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(export.status(), StatusCode::OK);
    let bundle = body_bytes(export).await;
    let manifest: Value = serde_json::from_slice(
        bundle.split(|b| *b == b'\n').next().unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["type"], "manifest");
    assert_eq!(manifest["row"]["format"], "gather-bundle-v1");

    let import = app
        .clone()
        .oneshot(
            Request::post("/api/v1/import")
                .body(Body::from(bundle.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(import.status(), StatusCode::OK);
    let import = body_json(import).await;
    assert_eq!(import["format"], "gather-bundle-v1");

    // Every row already exists; an idempotent import must add nothing.
    for table in import["tables"].as_object().unwrap().values() {
        assert_eq!(table["inserted"].as_u64().unwrap_or(0), 0);
    }

    // Final public search check after import proves the corpus remains usable.
    let after = app
        .oneshot(json_request(
            "POST",
            "/api/v1/search/semantic",
            json!({
                "text": format!("VectorStore{short}"),
                "scope": "atomic_units",
                "limit": 20
            }),
        ))
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::OK);
    let after = body_json(after).await;
    assert!(!after["hits"].as_array().unwrap().is_empty());

    // A concise diagnostic lands in GitHub's test log on success.
    let artifact_count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let unit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM atomic_units")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let contradiction_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM contradictions")
            .fetch_one(&state.pool)
            .await
            .unwrap();

    eprintln!(
        "MVP ACCEPTANCE PASS: artifacts={artifact_count} atomic_units={unit_count} contradictions={contradiction_count}"
    );

    // Sanity check that SQL used by the diagnostics remains live.
    let _ = state.pool.acquire().await.unwrap();
    let _ = Row::get::<i64, _>;
}