//! End-to-end gRPC tests against a real Postgres (pgvector), driven through a
//! real tonic client talking to `grpc::serve_on` on an ephemeral loopback
//! port. Skipped without DATABASE_URL, like the other integration suites.

// tonic interceptors must return Result<_, tonic::Status> (~176 bytes).
#![allow(clippy::result_large_err)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request as HttpRequest;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tonic::transport::Channel;
use tonic::{Code, Request};
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::extract::ollama::OllamaClient;
use gather_daemon::grpc::{self, pb};
use gather_daemon::{db, routes, AppState};

use pb::contradiction_service_client::ContradictionServiceClient;
use pb::export_service_client::ExportServiceClient;
use pb::ingest_service_client::IngestServiceClient;
use pb::query_service_client::QueryServiceClient;

async fn test_state(api_token: Option<&str>) -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return None;
    };
    let pool = db::connect(&database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    let mut config = Config::for_tests(database_url);
    config.api_token = api_token.map(str::to_string);
    Some(AppState {
        pool,
        config: Arc::new(config),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        ollama: None,
    })
}

/// Boot the gRPC server on an ephemeral loopback port and return its URL.
async fn spawn_grpc(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(grpc::serve_on(state, listener));
    format!("http://{addr}")
}

async fn connect(url: &str) -> Channel {
    let endpoint = Channel::from_shared(url.to_string()).expect("endpoint");
    for _ in 0..50 {
        if let Ok(channel) = endpoint.connect().await {
            return channel;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("gRPC server did not accept connections at {url}");
}

fn claude_export(salt: Uuid) -> Value {
    json!([{
        "uuid": format!("grpc-{salt}"),
        "name": "grpc integration conversation",
        "created_at": "2026-02-01T00:00:00Z",
        "updated_at": "2026-02-01T00:10:00Z",
        "chat_messages": [
            {"uuid": "m1", "sender": "human",
             "text": format!("gRPC parity fact {salt}"),
             "created_at": "2026-02-01T00:00:01Z"},
            {"uuid": "m2", "sender": "assistant", "text": "Understood.",
             "created_at": "2026-02-01T00:00:02Z"}
        ]
    }])
}

#[tokio::test]
async fn chat_export_roundtrip_matches_rest() {
    let Some(state) = test_state(None).await else {
        return;
    };
    let url = spawn_grpc(state.clone()).await;
    let channel = connect(&url).await;
    let mut ingest = IngestServiceClient::new(channel.clone());

    let salt = Uuid::new_v4();
    let request = pb::IngestChatExportRequest {
        platform: "claude".into(),
        data_json: claude_export(salt).to_string().into_bytes(),
        filename: "conversations.json".into(),
    };
    let first = ingest
        .ingest_chat_export(request.clone())
        .await
        .expect("ingest chat export")
        .into_inner();
    assert!(!first.deduplicated);
    assert_eq!(first.conversations, 1);
    assert_eq!(first.messages, 2);
    assert!(Uuid::parse_str(&first.job_id).is_ok());
    let artifact_id = Uuid::parse_str(&first.artifact_id).expect("artifact uuid");

    // Same payload again → content-hash dedup, same artifact.
    let second = ingest
        .ingest_chat_export(request)
        .await
        .expect("re-ingest")
        .into_inner();
    assert!(second.deduplicated);
    assert_eq!(second.artifact_id, first.artifact_id);

    // GetArtifact reflects what was stored.
    let mut query = QueryServiceClient::new(channel.clone());
    let artifact = query
        .get_artifact(pb::GetArtifactRequest {
            id: first.artifact_id.clone(),
        })
        .await
        .expect("get artifact")
        .into_inner();
    assert_eq!(artifact.kind, pb::ArtifactKind::ChatExport as i32);
    assert_eq!(artifact.source_platform, "claude");
    assert_eq!(artifact.content_hash.len(), 64);
    assert!(artifact.ingested_at.is_some());
    assert_eq!(artifact.original_filename, "conversations.json");

    // ListArtifacts with kind+platform filters includes it (newest first).
    let list = query
        .list_artifacts(pb::ListArtifactsRequest {
            kind: pb::ArtifactKind::ChatExport as i32,
            source_platform: "claude".into(),
            limit: 500,
            offset: 0,
        })
        .await
        .expect("list artifacts")
        .into_inner();
    assert!(list.items.iter().any(|a| a.id == first.artifact_id));

    // Parity: the REST surface serves the same artifact from the same store.
    let app = routes::build_router(state);
    let res = app
        .oneshot(
            HttpRequest::get(format!("/api/v1/artifacts/{artifact_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let rest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rest["id"], json!(first.artifact_id));
    assert_eq!(rest["kind"], json!("chat_export"));
    assert_eq!(rest["source_platform"], json!("claude"));

    // Unknown artifact → NOT_FOUND, garbage id → INVALID_ARGUMENT.
    let missing = query
        .get_artifact(pb::GetArtifactRequest {
            id: Uuid::new_v4().to_string(),
        })
        .await
        .expect_err("missing artifact");
    assert_eq!(missing.code(), Code::NotFound);
    let garbage = query
        .get_artifact(pb::GetArtifactRequest {
            id: "not-a-uuid".into(),
        })
        .await
        .expect_err("bad uuid");
    assert_eq!(garbage.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn ingest_file_streams_markdown_into_segments() {
    let Some(state) = test_state(None).await else {
        return;
    };
    let url = spawn_grpc(state).await;
    let mut ingest = IngestServiceClient::new(connect(&url).await);

    let salt = Uuid::new_v4();
    let markdown = format!(
        "# gRPC streamed upload {salt}\n\n\
         The first paragraph arrives in one data chunk.\n\n\
         ## Details\n\n\
         The second paragraph arrives in another chunk entirely.\n"
    );
    let bytes = markdown.as_bytes();
    let (head, tail) = bytes.split_at(bytes.len() / 2);
    let chunks = vec![
        pb::IngestFileChunk {
            payload: Some(pb::ingest_file_chunk::Payload::Meta(
                pb::ingest_file_chunk::Meta {
                    filename: format!("grpc-{salt}.md"),
                    media_type: "text/markdown".into(),
                    kind_override: pb::ArtifactKind::Unspecified as i32,
                },
            )),
        },
        pb::IngestFileChunk {
            payload: Some(pb::ingest_file_chunk::Payload::Data(head.to_vec())),
        },
        pb::IngestFileChunk {
            payload: Some(pb::ingest_file_chunk::Payload::Data(tail.to_vec())),
        },
    ];

    let res = ingest
        .ingest_file(tokio_stream::iter(chunks))
        .await
        .expect("streaming file ingest")
        .into_inner();
    assert_eq!(res.kind, pb::ArtifactKind::DocumentMarkdown as i32);
    assert!(!res.deduplicated);
    assert!(
        res.segments >= 2,
        "expected >=2 segments, got {}",
        res.segments
    );
    assert!(Uuid::parse_str(&res.artifact_id).is_ok());

    // A stream with data before meta is rejected outright.
    let bad = vec![pb::IngestFileChunk {
        payload: Some(pb::ingest_file_chunk::Payload::Data(b"orphan".to_vec())),
    }];
    let err = ingest
        .ingest_file(tokio_stream::iter(bad))
        .await
        .expect_err("data before meta");
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn entity_graph_traverses_seeded_relationships() {
    let Some(state) = test_state(None).await else {
        return;
    };
    let salt = Uuid::new_v4();

    let mut ids = Vec::new();
    for suffix in ["root", "mid", "leaf"] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO entities (name, kind) VALUES ($1, 'project') RETURNING id",
        )
        .bind(format!("grpc-graph-{suffix}-{salt}"))
        .fetch_one(&state.pool)
        .await
        .expect("seed entity");
        ids.push(id);
    }
    for (source, target) in [(ids[0], ids[1]), (ids[1], ids[2])] {
        sqlx::query(
            r#"
            INSERT INTO relationships (source_entity_id, target_entity_id,
                                       relation_type, confidence)
            VALUES ($1, $2, 'uses', 0.9)
            "#,
        )
        .bind(source)
        .bind(target)
        .execute(&state.pool)
        .await
        .expect("seed relationship");
    }

    let url = spawn_grpc(state).await;
    let mut query = QueryServiceClient::new(connect(&url).await);
    let graph = query
        .get_entity_graph(pb::GetEntityGraphRequest {
            entity_id: ids[0].to_string(),
            depth: 2,
        })
        .await
        .expect("entity graph")
        .into_inner();

    assert_eq!(graph.root.expect("root").id, ids[0].to_string());
    assert_eq!(graph.edges.len(), 2, "expected the 2 seeded edges");
    let depths: Vec<i32> = graph.edges.iter().map(|e| e.depth).collect();
    assert!(depths.contains(&1) && depths.contains(&2));
    assert_eq!(graph.nodes.len(), 3, "root + two hops");

    // depth=1 stops at the first hop.
    let shallow = query
        .get_entity_graph(pb::GetEntityGraphRequest {
            entity_id: ids[0].to_string(),
            depth: 1,
        })
        .await
        .expect("shallow graph")
        .into_inner();
    assert_eq!(shallow.edges.len(), 1);
}

#[tokio::test]
async fn semantic_search_fulltext_branch_finds_seeded_unit() {
    let Some(state) = test_state(None).await else {
        return;
    };
    let token = format!("zq{}", Uuid::new_v4().simple());
    let statement = format!("The {token} launch window opens in March 2027");
    let unit_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO atomic_units (kind, statement, statement_hash, confidence,
                                  extraction_method, status)
        VALUES ('fact', $1, $2, 0.9, 'manual', 'active')
        RETURNING id
        "#,
    )
    .bind(&statement)
    .bind(format!("{:0>64}", Uuid::new_v4().simple()))
    .fetch_one(&state.pool)
    .await
    .expect("seed unit");

    let url = spawn_grpc(state).await;
    let mut query = QueryServiceClient::new(connect(&url).await);
    let res = query
        .semantic_search(pb::SemanticSearchRequest {
            text: token.clone(),
            embedding: vec![],
            scope: "atomic_units".into(),
            limit: 10,
        })
        .await
        .expect("full-text search")
        .into_inner();
    assert_eq!(
        res.hits.len(),
        1,
        "unique token should match exactly one unit"
    );
    assert_eq!(res.hits[0].id, unit_id.to_string());
    assert_eq!(res.hits[0].scope, "atomic_units");
    assert!(res.hits[0].content.contains(&token));

    // Neither text nor embedding → INVALID_ARGUMENT.
    let err = query
        .semantic_search(pb::SemanticSearchRequest {
            text: String::new(),
            embedding: vec![],
            scope: "atomic_units".into(),
            limit: 10,
        })
        .await
        .expect_err("empty query");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// Minimal Ollama-compatible /api/embed stub returning a fixed vector.
async fn spawn_embed_stub(vector: Vec<f32>, calls: Arc<AtomicUsize>) -> String {
    let app = axum::Router::new().route(
        "/api/embed",
        axum::routing::post(move || {
            let vector = vector.clone();
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({ "embeddings": [vector] }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("embed stub");
    });
    format!("http://{addr}")
}

/// Deterministic pseudo-random 768-dim vector, unique per test run so
/// accumulated rows from earlier runs rank strictly lower.
fn random_unit_vector() -> Vec<f32> {
    let mut seed = Uuid::new_v4().as_u128() | 1;
    let mut v: Vec<f32> = (0..768)
        .map(|_| {
            // xorshift over the uuid seed
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed % 2000) as f32 / 1000.0) - 1.0
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
        *x /= norm;
    }
    v
}

#[tokio::test]
async fn semantic_search_embeds_query_server_side() {
    let Some(base) = test_state(None).await else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let vector = random_unit_vector();
    let stub_url = spawn_embed_stub(vector.clone(), calls.clone()).await;

    // State with Ollama pointed at the stub: search must embed server-side.
    let mut config = Config::for_tests(base.config.database_url.clone());
    config.ollama_url = Some(stub_url);
    let ollama = OllamaClient::from_config(&config)
        .expect("ollama client")
        .map(Arc::new);
    assert!(ollama.is_some(), "stub URL should configure the client");
    let state = AppState {
        config: Arc::new(config),
        ollama,
        ..base
    };

    // Seed a unit whose embedding equals exactly what the stub returns.
    let token = format!("zv{}", Uuid::new_v4().simple());
    let unit_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO atomic_units (kind, statement, statement_hash, confidence,
                                  extraction_method, status, embedding)
        VALUES ('fact', $1, $2, 0.9, 'manual', 'active', $3)
        RETURNING id
        "#,
    )
    .bind(format!("The {token} reactor runs at 42 percent efficiency"))
    .bind(format!("{:0>64}", Uuid::new_v4().simple()))
    .bind(pgvector::Vector::from(vector))
    .fetch_one(&state.pool)
    .await
    .expect("seed embedded unit");

    let url = spawn_grpc(state).await;
    let mut query = QueryServiceClient::new(connect(&url).await);
    let res = query
        .semantic_search(pb::SemanticSearchRequest {
            text: "what efficiency does the reactor run at".into(),
            embedding: vec![],
            scope: "atomic_units".into(),
            limit: 5,
        })
        .await
        .expect("vector search")
        .into_inner();

    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "server should have called the embed stub"
    );
    assert!(!res.hits.is_empty());
    // Cosine similarity of identical vectors is 1.0: the seeded unit wins.
    assert_eq!(res.hits[0].id, unit_id.to_string());
    assert!(
        res.hits[0].score > 0.99,
        "expected ~1.0 cosine score, got {}",
        res.hits[0].score
    );
}

#[tokio::test]
async fn resolve_contradiction_supersedes_loser_and_audits() {
    let Some(state) = test_state(None).await else {
        return;
    };
    let salt = Uuid::new_v4();
    let mut unit_ids = Vec::new();
    for (n, price) in [("a", 50), ("b", 75)] {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO atomic_units (kind, statement, statement_hash, confidence,
                                      extraction_method, status)
            VALUES ('fact', $1, $2, 0.8, 'manual', 'active')
            RETURNING id
            "#,
        )
        .bind(format!(
            "grpc-resolve-{salt} plan {n} costs ${price} monthly"
        ))
        .bind(format!("{:0>64}", Uuid::new_v4().simple()))
        .fetch_one(&state.pool)
        .await
        .expect("seed unit");
        unit_ids.push(id);
    }
    // The schema enforces unit_a_id < unit_b_id.
    unit_ids.sort();
    let (unit_a, unit_b) = (unit_ids[0], unit_ids[1]);
    let contradiction_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO contradictions (unit_a_id, unit_b_id, score,
                                    detection_method, explanation)
        VALUES ($1, $2, 0.9, 'rule:numeric', 'seeded by grpc test')
        RETURNING id
        "#,
    )
    .bind(unit_a)
    .bind(unit_b)
    .fetch_one(&state.pool)
    .await
    .expect("seed contradiction");

    let url = spawn_grpc(state.clone()).await;
    let channel = connect(&url).await;
    let mut contra = ContradictionServiceClient::new(channel.clone());

    // It shows up in the open list with both units attached.
    let listed = contra
        .list_contradictions(pb::ListContradictionsRequest {
            status: pb::ContradictionStatus::Open as i32,
            limit: 500,
            offset: 0,
        })
        .await
        .expect("list contradictions")
        .into_inner();
    let found = listed
        .items
        .iter()
        .find(|c| c.id == contradiction_id.to_string())
        .expect("seeded contradiction listed");
    assert_eq!(found.unit_a.as_ref().unwrap().id, unit_a.to_string());
    assert_eq!(found.unit_b.as_ref().unwrap().id, unit_b.to_string());

    // Resolve keeping A; B must come back superseded by A.
    let resolved = contra
        .resolve_contradiction(pb::ResolveContradictionRequest {
            id: contradiction_id.to_string(),
            resolution: pb::ContradictionStatus::ResolvedA as i32,
            note: "keep the $50 figure".into(),
            actor: "grpc-reviewer".into(),
        })
        .await
        .expect("resolve")
        .into_inner();
    assert_eq!(resolved.status, pb::ContradictionStatus::ResolvedA as i32);
    assert_eq!(resolved.resolved_by, "grpc-reviewer");
    assert_eq!(resolved.resolution_note, "keep the $50 figure");
    assert!(resolved.resolved_at.is_some());
    let loser = resolved.unit_b.expect("unit_b present");
    assert_eq!(loser.status, pb::UnitStatus::Superseded as i32);
    assert_eq!(loser.superseded_by_unit_id, unit_a.to_string());
    assert_eq!(
        resolved.unit_a.expect("unit_a present").status,
        pb::UnitStatus::Active as i32
    );

    // The audit trail recorded the transition.
    let (audit_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM contradiction_audit WHERE contradiction_id = $1 AND action = 'resolve'",
    )
    .bind(contradiction_id)
    .fetch_one(&state.pool)
    .await
    .expect("audit count");
    assert_eq!(audit_count, 1);

    // Double-resolve is rejected; OPEN is not a valid resolution either.
    let again = contra
        .resolve_contradiction(pb::ResolveContradictionRequest {
            id: contradiction_id.to_string(),
            resolution: pb::ContradictionStatus::Dismissed as i32,
            note: String::new(),
            actor: String::new(),
        })
        .await
        .expect_err("already resolved");
    assert_eq!(again.code(), Code::InvalidArgument);
    let open = contra
        .resolve_contradiction(pb::ResolveContradictionRequest {
            id: contradiction_id.to_string(),
            resolution: pb::ContradictionStatus::Open as i32,
            note: String::new(),
            actor: String::new(),
        })
        .await
        .expect_err("open is not a resolution");
    assert_eq!(open.code(), Code::InvalidArgument);

    // Annotate leaves another audit row.
    let annotated = contra
        .annotate_contradiction(pb::AnnotateContradictionRequest {
            id: contradiction_id.to_string(),
            note: "checked against the invoice".into(),
            actor: "grpc-reviewer".into(),
        })
        .await
        .expect("annotate")
        .into_inner();
    assert!(Uuid::parse_str(&annotated.audit_id).is_ok());
}

#[tokio::test]
async fn export_bundle_streams_and_reimports() {
    let Some(state) = test_state(None).await else {
        return;
    };
    let url = spawn_grpc(state.clone()).await;
    let channel = connect(&url).await;

    // Guarantee at least one artifact exists before exporting.
    let mut ingest = IngestServiceClient::new(channel.clone());
    ingest
        .ingest_chat_export(pb::IngestChatExportRequest {
            platform: "claude".into(),
            data_json: claude_export(Uuid::new_v4()).to_string().into_bytes(),
            filename: "export-seed.json".into(),
        })
        .await
        .expect("seed artifact");

    let mut export = ExportServiceClient::new(channel.clone());
    let mut stream = export
        .export_bundle(pb::ExportBundleRequest {})
        .await
        .expect("export bundle")
        .into_inner();
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk_count = 0usize;
    while let Some(chunk) = stream.message().await.expect("bundle chunk") {
        bytes.extend_from_slice(&chunk.data);
        chunk_count += 1;
    }
    assert!(chunk_count >= 1);
    let text = String::from_utf8(bytes.clone()).expect("bundle is UTF-8");

    // First line is the gather-bundle-v1 manifest; every line is valid NDJSON.
    let mut lines = text.lines();
    let manifest: Value =
        serde_json::from_str(lines.next().expect("manifest line")).expect("manifest json");
    assert_eq!(manifest["type"], json!("manifest"));
    assert_eq!(manifest["row"]["format"], json!("gather-bundle-v1"));
    assert!(manifest["row"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t == "artifacts"));
    for line in lines {
        let row: Value = serde_json::from_str(line).expect("NDJSON row");
        assert!(row.get("type").is_some());
    }

    // Re-import the same bundle through the client-streaming RPC: every row
    // already exists, so nothing should error and counts must be sane.
    let chunks: Vec<pb::BundleChunk> = bytes
        .chunks(64 * 1024)
        .map(|c| pb::BundleChunk { data: c.to_vec() })
        .collect();
    let imported = export
        .import_bundle(tokio_stream::iter(chunks))
        .await
        .expect("import bundle")
        .into_inner();
    assert!(!imported.tables.is_empty());
    let artifacts = imported
        .tables
        .iter()
        .find(|t| t.table == "artifacts")
        .expect("artifacts table counted");
    assert!(artifacts.in_bundle >= 1);
    assert!(artifacts.inserted <= artifacts.in_bundle);
}

#[tokio::test]
async fn auth_interceptor_enforces_bearer_token() {
    let Some(state) = test_state(Some("grpc-secret-token")).await else {
        return;
    };
    let url = spawn_grpc(state).await;
    let channel = connect(&url).await;

    // No token → UNAUTHENTICATED.
    let mut bare = QueryServiceClient::new(channel.clone());
    let err = bare
        .list_artifacts(pb::ListArtifactsRequest::default())
        .await
        .expect_err("missing token");
    assert_eq!(err.code(), Code::Unauthenticated);

    // Wrong token → UNAUTHENTICATED.
    let mut wrong = QueryServiceClient::with_interceptor(channel.clone(), |mut req: Request<()>| {
        req.metadata_mut()
            .insert("authorization", "Bearer wrong-token".parse().unwrap());
        Ok(req)
    });
    let err = wrong
        .list_artifacts(pb::ListArtifactsRequest::default())
        .await
        .expect_err("wrong token");
    assert_eq!(err.code(), Code::Unauthenticated);

    // Correct token → the call goes through on every service.
    let with_token = |mut req: Request<()>| {
        req.metadata_mut()
            .insert("authorization", "Bearer grpc-secret-token".parse().unwrap());
        Ok(req)
    };
    let mut authed = QueryServiceClient::with_interceptor(channel.clone(), with_token);
    authed
        .list_artifacts(pb::ListArtifactsRequest::default())
        .await
        .expect("authorized query");
    let mut contra = ContradictionServiceClient::with_interceptor(channel, with_token);
    contra
        .list_contradictions(pb::ListContradictionsRequest::default())
        .await
        .expect("authorized contradiction list");
}
