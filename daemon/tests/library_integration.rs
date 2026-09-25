//! Browsing endpoints: artifact progress, units by artifact, artifact content,
//! and the graph overview, over REST and gRPC. Runs when DATABASE_URL is set.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tonic::transport::Channel;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::grpc::{self, pb};
use gather_daemon::{db, extract, routes, AppState};

use pb::query_service_client::QueryServiceClient;

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

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn upload(filename: &str, text: &str) -> Request<Body> {
    let boundary = "gatherlibraryboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{filename}\"\r\nContent-Type: text/markdown\r\n\r\n{text}\r\n--{boundary}--\r\n"
    );
    Request::post("/api/v1/ingest/files")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn artifact_status(app: &axum::Router, id: &str) -> Value {
    let (status, body) = get_json(app, &format!("/api/v1/artifacts/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn library_views_follow_an_upload_through_extraction() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let salt = Uuid::new_v4().simple().to_string();
    let tool = format!("Zanzibar{}", &salt[..8]);
    let host = format!("Quokka{}", &salt[..8]);
    let filename = format!("library-{salt}.md");
    let text = format!(
        "# Notes {salt}\n\nI prefer {tool}.\n\nWe decided to use {host}.\n\n\
         Some ordinary prose that matches no extraction rule at all.\n"
    );

    let res = app.clone().oneshot(upload(&filename, &text)).await.unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["files"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Segmented but not yet mined for units: still processing, nothing found.
    let before = artifact_status(&app, &id).await;
    assert_eq!(before["status"], "processing");
    assert_eq!(before["unit_count"], 0);

    // The list carries the same progress fields.
    let (_, list) = get_json(&app, "/api/v1/artifacts?limit=500").await;
    let listed = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id.as_str())
        .expect("uploaded artifact is listed");
    assert_eq!(listed["status"], "processing");

    // Run extraction until this artifact is done (other tests share the queue).
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .expect("extraction pass");
        if artifact_status(&app, &id).await["status"] == "done" {
            break;
        }
    }
    let after = artifact_status(&app, &id).await;
    assert_eq!(after["status"], "done", "{after}");
    assert!(after["unit_count"].as_i64().unwrap() >= 2, "{after}");

    // Units filtered to this artifact are exactly this file's statements.
    let (_, units) = get_json(&app, &format!("/api/v1/atomic-units?artifact_id={id}")).await;
    let statements: Vec<&str> = units["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["statement"].as_str().unwrap())
        .collect();
    assert_eq!(
        statements.len() as i64,
        after["unit_count"].as_i64().unwrap()
    );
    assert!(
        statements.iter().any(|s| s.contains(&tool)),
        "{statements:?}"
    );
    assert!(
        statements.iter().any(|s| s.contains(&host)),
        "{statements:?}"
    );

    // Content: the document's segments, in order, paged.
    let (status, content) = get_json(&app, &format!("/api/v1/artifacts/{id}/content")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content["source"], "document");
    let total = content["total"].as_i64().unwrap();
    assert!(total >= 1);
    let joined: String = content["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap())
        .collect();
    assert!(joined.contains("ordinary prose"), "{content}");
    let (_, past_end) = get_json(
        &app,
        &format!("/api/v1/artifacts/{id}/content?offset={total}"),
    )
    .await;
    assert_eq!(past_end["items"].as_array().unwrap().len(), 0);
    assert_eq!(past_end["total"], total);

    let (status, _) = get_json(
        &app,
        &format!("/api/v1/artifacts/{}/content", Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Graph: the new entities, their relationships, and the file they came from.
    let (status, graph) = get_json(&app, "/api/v1/graph?max_entities=1000&max_files=1000").await;
    assert_eq!(status, StatusCode::OK);
    let entity_id = |name: &str| {
        graph["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            .map(|e| e["id"].as_str().unwrap().to_string())
    };
    let tool_id = entity_id(&tool).expect("preferred tool is an entity");
    let host_id = entity_id(&host).expect("chosen host is an entity");
    let me_id = entity_id("Me").expect("first-person statements hang off Me");
    let has_relation = |to: &str| {
        graph["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["source"] == me_id.as_str() && r["target"] == to)
    };
    assert!(has_relation(&tool_id), "{graph}");
    assert!(has_relation(&host_id), "{graph}");
    assert!(graph["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["id"] == id.as_str()));
    let mentions_file = |entity: &str| {
        graph["mentions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["file_id"] == id.as_str() && m["entity_id"] == entity)
    };
    assert!(
        mentions_file(&tool_id) && mentions_file(&host_id),
        "{graph}"
    );

    // max_files=0 leaves files out; relations stay.
    let (_, no_files) = get_json(&app, "/api/v1/graph?max_entities=1000&max_files=0").await;
    assert!(no_files["files"].as_array().unwrap().is_empty());
    assert!(no_files["mentions"].as_array().unwrap().is_empty());
    assert!(!no_files["relations"].as_array().unwrap().is_empty());

    // gRPC returns the same views.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(grpc::serve_on(state.clone(), listener));
    let endpoint = Channel::from_shared(format!("http://{addr}")).unwrap();
    let mut channel = None;
    for _ in 0..50 {
        if let Ok(c) = endpoint.connect().await {
            channel = Some(c);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut query = QueryServiceClient::new(channel.expect("gRPC server is up"));

    let artifact = query
        .get_artifact(pb::GetArtifactRequest { id: id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(artifact.status, "done");
    assert_eq!(artifact.unit_count, after["unit_count"].as_i64().unwrap());

    let units = query
        .list_atomic_units(pb::ListAtomicUnitsRequest {
            artifact_id: id.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(units.items.len(), statements.len());

    let content = query
        .get_artifact_content(pb::GetArtifactContentRequest {
            id: id.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(content.source, "document");
    assert_eq!(content.total, total);

    let overview = query
        .get_graph_overview(pb::GetGraphOverviewRequest {
            max_entities: 1000,
            max_files: 1000,
            exclude_files: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(overview.entities.iter().any(|e| e.id == tool_id));
    assert!(overview.files.iter().any(|f| f.id == id));
    let without_files = query
        .get_graph_overview(pb::GetGraphOverviewRequest {
            max_entities: 1000,
            exclude_files: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(without_files.files.is_empty());
}
