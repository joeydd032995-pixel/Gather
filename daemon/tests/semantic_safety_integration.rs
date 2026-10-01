//! End-to-end semantic safety against pgvector: certificates written by the
//! real workers, withdrawal on new evidence, user decisions as evidence,
//! source retraction, supersession, provenance independence, extractor
//! revisions, the REST/gRPC query surface and the reversible migration.
//! Skipped without DATABASE_URL. Tests share one database and the workers
//! are global, so every test holds one lock.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::entities::similarity::name_similarity;
use gather_daemon::grpc::{self, pb};
use gather_daemon::{cluster, db, extract, photo, routes, scan, AppState};

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

async fn call(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"));
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn ingest(app: &axum::Router, conversation: &str, content: &str, at: &str) {
    let export = json!({
        "platform": "generic",
        "data": {"schema": "gather-generic-v1", "conversations": [{
            "id": conversation,
            "messages": [{"role": "user", "content": content, "created_at": at}]
        }]}
    });
    let (status, _) = call(app, Method::POST, "/ingest/chat-export", Some(export)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

async fn drain(state: &AppState) {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .expect("extraction pass");
        scan::run_one_scan(&state.pool, &state.config, None)
            .await
            .expect("scan pass");
        let busy: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM messages WHERE units_extracted_at IS NULL) \
                  + (SELECT count(*) FROM atomic_units \
                     WHERE contradiction_scanned_at IS NULL AND status = 'active')",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if busy == 0 {
            return;
        }
    }
    panic!("queues did not drain");
}

async fn unit_like(state: &AppState, needle: &str) -> Uuid {
    sqlx::query_scalar(
        "SELECT id FROM atomic_units WHERE statement LIKE $1 ORDER BY created_at LIMIT 1",
    )
    .bind(format!("%{needle}%"))
    .fetch_one(&state.pool)
    .await
    .unwrap_or_else(|_| panic!("no unit like {needle}"))
}

async fn artifact_of(state: &AppState, conversation: &str) -> Uuid {
    sqlx::query_scalar("SELECT artifact_id FROM conversations WHERE external_id = $1")
        .bind(conversation)
        .fetch_one(&state.pool)
        .await
        .expect("artifact for conversation")
}

/// An entity mentioned by a unit in a seeded source.
async fn seed_entity(state: &AppState, name: &str) -> Uuid {
    seed_entity_with_source(state, name).await.0
}

/// [`seed_entity`], also returning the artifact that mentions it.
async fn seed_entity_with_source(state: &AppState, name: &str) -> (Uuid, Uuid) {
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO entities (name, kind) VALUES ($1, 'other') RETURNING id")
            .bind(name)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let text = format!("A note about {name} {}", Uuid::new_v4());
    let artifact: Uuid = sqlx::query_scalar(
        "INSERT INTO artifacts (kind, byte_size, content_hash, raw_content) \
         VALUES ('document_text', 1, encode(digest($1, 'sha256'), 'hex'), $2) RETURNING id",
    )
    .bind(&text)
    .bind(text.as_bytes())
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let document: Uuid =
        sqlx::query_scalar("INSERT INTO documents (artifact_id) VALUES ($1) RETURNING id")
            .bind(artifact)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let segment: Uuid = sqlx::query_scalar(
        "INSERT INTO document_segments (document_id, seq, content, content_hash, units_extracted_at) \
         VALUES ($1, 0, $2, encode(digest($2, 'sha256'), 'hex'), now()) RETURNING id",
    )
    .bind(document)
    .bind(&text)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let unit: Uuid = sqlx::query_scalar(
        "INSERT INTO atomic_units (kind, statement, statement_hash, subject_entity_id, \
           confidence, extraction_method, clustered_at, contradiction_scanned_at) \
         VALUES ('fact', $1, encode(digest($1, 'sha256'), 'hex'), $2, 0.6, 'rule_based', \
                 now(), now()) RETURNING id",
    )
    .bind(&text)
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO atomic_unit_provenance (atomic_unit_id, artifact_id, document_segment_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(unit)
    .bind(artifact)
    .bind(segment)
    .execute(&state.pool)
    .await
    .unwrap();
    (id, artifact)
}

async fn head(state: &AppState, id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

async fn certificates(app: &axum::Router, query: &str) -> Vec<Value> {
    let (status, body) = call(app, Method::GET, &format!("/certificates?{query}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["items"].as_array().cloned().unwrap_or_default()
}

fn has_reason(c: &Value, code: &str) -> bool {
    c["reason_codes"]
        .as_array()
        .is_some_and(|a| a.iter().any(|r| r == code))
}

#[tokio::test]
async fn a_merge_is_withdrawn_when_later_evidence_makes_it_a_chain() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let token = &Uuid::new_v4().simple().to_string()[..8];
    let a_name = format!("Heron {token} Valley Beekeepers Union");
    let b_name = format!("{a_name}s");
    let c_name = format!("{b_name} Co");
    assert!(name_similarity(&a_name, &b_name) >= 0.92);
    assert!(name_similarity(&b_name, &c_name) >= 0.92);
    assert!(name_similarity(&a_name, &c_name) < 0.92);

    // Incremental: A and B arrive first and merge (a clean pair).
    let a = seed_entity(&state, &a_name).await;
    let b = seed_entity(&state, &b_name).await;
    cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    assert_eq!(
        head(&state, a).await,
        Some(b),
        "the pair merges into the longer name"
    );
    let merge_cert = certificates(
        &app,
        &format!("subject_id={a}&kind=entity_merge&outcome=auto_applied"),
    )
    .await;
    assert_eq!(merge_cert.len(), 1, "the merge has a certificate");
    assert!(!merge_cert[0]["source_artifact_ids"]
        .as_array()
        .unwrap()
        .is_empty());

    // C arrives: A~B~C is a chain. The earlier merge no longer holds.
    let c = seed_entity(&state, &c_name).await;
    cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    for id in [a, b, c] {
        assert_eq!(
            head(&state, id).await,
            None,
            "no chained entity stays merged"
        );
    }
    let old = certificates(
        &app,
        &format!("subject_id={a}&kind=entity_merge&outcome=superseded"),
    )
    .await;
    assert!(
        old.iter().any(|x| x["id"] == merge_cert[0]["id"]),
        "the merge certificate is superseded"
    );
    let review = certificates(
        &app,
        &format!("subject_id={b}&outcome=needs_review&reason=CHAINED_SIMILARITY"),
    )
    .await;
    assert!(!review.is_empty(), "the chain is explained");
    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM review_queue WHERE state = 'open' AND reason = 'merge-band' \
         AND target_id IN ($1, $2)",
    )
    .bind(cluster::pair_key(a, b))
    .bind(cluster::pair_key(b, c))
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(parked, 2, "both chained pairs wait in the tray");
    // The unmerge was not a user decision: the pair is not dismissed.
    let dismissed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entity_merge_audit WHERE action = 'dismiss' \
         AND ((winner_entity_id = $1 AND loser_entity_id = $2) OR (winner_entity_id = $2 AND loser_entity_id = $1))",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(dismissed, 0);

    // A second pass is a no-op (idempotent).
    let stats = cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    assert_eq!(stats.entities_withdrawn, 0);
    for id in [a, b, c] {
        assert_eq!(head(&state, id).await, None);
    }
}

#[tokio::test]
async fn a_user_split_is_evidence_and_blocks_an_automatic_remerge() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let token = &Uuid::new_v4().simple().to_string()[..8];
    let x = seed_entity(&state, &format!("Osprey {token} Freight")).await;
    let y = seed_entity(&state, &format!("Osprey {token} Freight.")).await;
    cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    assert_eq!(head(&state, x).await, Some(y));
    let merge = certificates(
        &app,
        &format!("subject_id={x}&kind=entity_merge&outcome=auto_applied"),
    )
    .await;
    assert_eq!(merge.len(), 1);

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/entities/{x}/unmerge"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let retracted = certificates(
        &app,
        &format!("subject_id={x}&kind=entity_merge&outcome=retracted"),
    )
    .await;
    assert_eq!(retracted.len(), 1, "the merge certificate is retracted");
    let event = retracted[0]["caused_by"]
        .as_str()
        .expect("caused by the split");
    let (_, split) = call(&app, Method::GET, &format!("/certificates/{event}"), None).await;
    assert_eq!(split["decision"], "user_decision");
    assert_eq!(split["rule_id"], "user.entity_split");
    let (_, affected) = call(
        &app,
        Method::GET,
        &format!("/certificates/{event}/affected"),
        None,
    )
    .await;
    assert!(affected["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == merge[0]["id"]));

    cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    assert_eq!(
        head(&state, x).await,
        None,
        "an explicit split is never undone automatically"
    );
    let blocked = certificates(
        &app,
        &format!("subject_id={x}&outcome=blocked&reason=USER_REJECTION_EXISTS"),
    )
    .await;
    assert!(!blocked.is_empty(), "the blocked re-merge is explained");
}

#[tokio::test]
async fn retracting_a_source_withdraws_what_rested_on_it() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let m = &Uuid::new_v4().simple().to_string()[..8];
    let (ca, cb) = (format!("ret-{m}-a"), format!("ret-{m}-b"));
    ingest(
        &app,
        &ca,
        &format!("My Kappa{m} budget is $50 per month."),
        "2026-04-01T10:00:00Z",
    )
    .await;
    ingest(
        &app,
        &cb,
        &format!("My Kappa{m} budget is $75 per month."),
        "2026-04-01T10:00:00Z",
    )
    .await;
    drain(&state).await;

    let (id, status, certificate, alignment): (Uuid, String, Option<Uuid>, Option<Value>) =
        sqlx::query_as(
            "SELECT c.id, c.status::text, c.certificate_id, c.alignment FROM contradictions c \
         JOIN atomic_units a ON a.id = c.unit_a_id WHERE a.statement LIKE $1",
        )
        .bind(format!("%Kappa{m}%"))
        .fetch_one(&state.pool)
        .await
        .expect("the aligned conflict is reported");
    assert_eq!(status, "open");
    let certificate = certificate.expect("contradiction has a certificate");
    let alignment = alignment.expect("alignment stored");
    for d in [
        "subject",
        "predicate",
        "unit",
        "value",
        "scope",
        "granularity",
        "time",
    ] {
        assert!(alignment.get(d).is_some(), "alignment lacks {d}");
    }
    let (_, cert) = call(
        &app,
        Method::GET,
        &format!("/certificates/{certificate}"),
        None,
    )
    .await;
    assert_eq!(cert["conclusion_id"], json!(id));
    assert_eq!(cert["decision"], "auto_applied");

    let art_b = artifact_of(&state, &cb).await;
    let (_, before) = call(
        &app,
        Method::GET,
        &format!("/artifacts/{art_b}/conclusions"),
        None,
    )
    .await;
    assert!(before["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == json!(certificate)));

    let (s, report) = call(
        &app,
        Method::POST,
        &format!("/artifacts/{art_b}/retract"),
        Some(json!({"reason": "wrong file"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{report}");
    assert_eq!(report["units_retracted"].as_array().unwrap().len(), 1);
    assert!(report["contradictions_withdrawn"].as_u64().unwrap() >= 1);
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM contradictions WHERE id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(status, "dismissed", "the contradiction is withdrawn");
    let (_, cert) = call(
        &app,
        Method::GET,
        &format!("/certificates/{certificate}"),
        None,
    )
    .await;
    assert_eq!(cert["outcome"], "retracted");
    assert_eq!(cert["caused_by"], report["event_certificate"]);

    // Deleting the other source removes the artifact itself.
    let art_a = artifact_of(&state, &ca).await;
    let (s, report) = call(&app, Method::DELETE, &format!("/artifacts/{art_a}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(report["deleted"], true);
    let gone: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE id = $1")
        .bind(art_a)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(gone, 0);
    // The event keeps the history even though the file is gone.
    let events = certificates(&app, &format!("subject_id={art_a}&kind=user_decision")).await;
    assert_eq!(events[0]["rule_id"], "user.retract_source");
}

#[tokio::test]
async fn a_later_current_state_supersedes_and_rejecting_it_restores_the_old_one() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let m = &Uuid::new_v4().simple().to_string()[..8];
    ingest(
        &app,
        &format!("sup-{m}-a"),
        &format!("My Lambda{m} rent is $1200 per month."),
        "2023-01-01T10:00:00Z",
    )
    .await;
    ingest(
        &app,
        &format!("sup-{m}-b"),
        &format!("My Lambda{m} rent is $1500 per month."),
        "2026-01-01T10:00:00Z",
    )
    .await;
    drain(&state).await;

    let older = unit_like(&state, &format!("Lambda{m} rent is $1200")).await;
    let newer = unit_like(&state, &format!("Lambda{m} rent is $1500")).await;
    let (status, by): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status::text, superseded_by_unit_id FROM atomic_units WHERE id = $1",
    )
    .bind(older)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), by), ("superseded", Some(newer)));
    let contradictions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contradictions WHERE unit_a_id IN ($1, $2) AND unit_b_id IN ($1, $2)",
    )
    .bind(older)
    .bind(newer)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        contradictions, 0,
        "a change of state is not a contradiction"
    );
    let blocked = certificates(&app, &format!("subject_id={older}&kind=contradiction")).await;
    assert!(blocked.iter().any(|c| has_reason(c, "TEMPORAL_SUCCESSION")));
    let sup = certificates(&app, &format!("subject_id={older}&kind=fact_supersession")).await;
    assert_eq!(sup.len(), 1);

    let (s, _) = call(
        &app,
        Method::POST,
        &format!("/units/{newer}/reject"),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let status: String = sqlx::query_scalar("SELECT status::text FROM atomic_units WHERE id = $1")
        .bind(older)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(status, "active", "the older state is current again");
    let sup = certificates(&app, &format!("subject_id={older}&kind=fact_supersession")).await;
    assert_eq!(sup[0]["outcome"], "retracted");
}

#[tokio::test]
async fn copies_do_not_corroborate_and_independent_sources_do() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let m = &Uuid::new_v4().simple().to_string()[..8];
    let text = format!("I use Zorbix{m} for search.");
    ingest(&app, &format!("cor-{m}-a"), &text, "2026-02-01T10:00:00Z").await;
    ingest(&app, &format!("cor-{m}-b"), &text, "2026-02-02T10:00:00Z").await;
    drain(&state).await;
    let unit = unit_like(&state, &format!("Zorbix{m}")).await;
    let (_, support) = call(&app, Method::GET, &format!("/units/{unit}/support"), None).await;
    assert_eq!(support["support"]["artifacts"], 2);
    assert_eq!(
        support["support"]["independent_sources"], 1,
        "an identical copy counts once"
    );
    let blocked = certificates(&app, &format!("subject_id={unit}&rule=claim.corroboration")).await;
    assert!(blocked
        .iter()
        .any(|c| c["decision"] == "blocked" && has_reason(c, "SOURCE_NOT_INDEPENDENT")));

    // A differently worded source that says the same thing is independent...
    ingest(
        &app,
        &format!("cor-{m}-c"),
        &format!("{text} Also the index rebuilds nightly."),
        "2026-02-03T10:00:00Z",
    )
    .await;
    drain(&state).await;
    let (_, support) = call(&app, Method::GET, &format!("/units/{unit}/support"), None).await;
    assert_eq!(support["support"]["independent_sources"], 2);
    // ...until it is declared a summary of the first.
    let (child, parent) = (
        artifact_of(&state, &format!("cor-{m}-c")).await,
        artifact_of(&state, &format!("cor-{m}-a")).await,
    );
    let (s, body) = call(
        &app,
        Method::POST,
        &format!("/artifacts/{child}/derivations"),
        Some(json!({"parent_id": parent, "kind": "summary"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(!body["certificates_withdrawn"]
        .as_array()
        .unwrap()
        .is_empty());
    let (_, support) = call(&app, Method::GET, &format!("/units/{unit}/support"), None).await;
    assert_eq!(support["support"]["independent_sources"], 1);

    // Re-ingesting the same export is a no-op: no new certificates.
    let before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inference_certificates WHERE $1 = ANY(subject_ids)",
    )
    .bind(unit)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    ingest(&app, &format!("cor-{m}-a"), &text, "2026-02-01T10:00:00Z").await;
    drain(&state).await;
    let after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inference_certificates WHERE $1 = ANY(subject_ids)",
    )
    .bind(unit)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(before, after);
}

async fn seed_photo(state: &AppState, phash: u64) -> Uuid {
    let bytes = Uuid::new_v4().as_bytes().to_vec();
    let artifact: Uuid = sqlx::query_scalar(
        "INSERT INTO artifacts (kind, byte_size, content_hash, raw_content, original_filename) \
         VALUES ('image_photo', 16, encode(digest($1, 'sha256'), 'hex'), $1, 'p.jpg') RETURNING id",
    )
    .bind(&bytes)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO images (artifact_id, phash, ocr_status, units_extracted_at, photo_prepared_at, \
           captioned_at) VALUES ($1, $2, 'skipped', now(), now(), now()) RETURNING id",
    )
    .bind(artifact)
    .bind(phash as i64)
    .fetch_one(&state.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_not_duplicate_decision_splits_the_group_for_good() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let base = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap());
    let p1 = seed_photo(&state, base).await;
    let p2 = seed_photo(&state, base ^ 1).await;
    photo::worker::run_one_pass(&state.pool, &state.config, None)
        .await
        .unwrap();
    let cluster_of = |id: Uuid| {
        let pool = state.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>("SELECT dup_cluster_id FROM images WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let group = cluster_of(p1).await;
    assert!(
        group.is_some() && group == cluster_of(p2).await,
        "near copies are grouped"
    );
    let certs = certificates(
        &app,
        &format!("subject_id={p1}&kind=photo_duplicate_group&outcome=auto_applied"),
    )
    .await;
    assert_eq!(certs.len(), 1);
    assert_eq!(certs[0]["conclusion_id"], json!(group.unwrap()));

    let (s, body) = call(
        &app,
        Method::POST,
        &format!("/images/{p1}/not-duplicate"),
        Some(json!({"other_id": p2})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    photo::worker::run_one_pass(&state.pool, &state.config, None)
        .await
        .unwrap();
    assert!(cluster_of(p1).await.is_none() || cluster_of(p1).await != cluster_of(p2).await);
    let (_, affected) = call(
        &app,
        Method::GET,
        &format!(
            "/certificates/{}/affected",
            body["certificate"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert!(affected["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == certs[0]["id"]));
    let blocked = certificates(
        &app,
        &format!("subject_id={p1}&reason=USER_REJECTION_EXISTS"),
    )
    .await;
    assert!(!blocked.is_empty());
}

#[tokio::test]
async fn an_extractor_revision_that_disagrees_is_reviewed_not_applied() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let m = &Uuid::new_v4().simple().to_string()[..8];
    ingest(
        &app,
        &format!("rev-{m}"),
        &format!("I use Quibble{m} daily."),
        "2026-03-01T10:00:00Z",
    )
    .await;
    drain(&state).await;
    let unit = unit_like(&state, &format!("Quibble{m}")).await;
    let before: String = sqlx::query_scalar("SELECT statement FROM atomic_units WHERE id = $1")
        .bind(unit)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let (s, body) = call(
        &app,
        Method::POST,
        &format!("/units/{unit}/revisions"),
        Some(json!({"statement": format!("I do not use Quibble{m} daily"), "model_version": "ollama:test@2"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["disagreement"], true);
    let after: String = sqlx::query_scalar("SELECT statement FROM atomic_units WHERE id = $1")
        .bind(unit)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(before, after, "history is not rewritten");
    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM review_queue WHERE target_id = $1 AND reason = 'model-disagreement' AND state = 'open'",
    )
    .bind(unit)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(parked, 1);
    let certs = certificates(
        &app,
        &format!("conclusion_id={unit}&reason=MODEL_DISAGREEMENT"),
    )
    .await;
    assert_eq!(certs[0]["model_version"], "ollama:test@2");
    assert_eq!(certs[0]["scope"]["old_model"], "rule_based");
}

#[tokio::test]
async fn grpc_serves_certificates() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(grpc::serve_on(state.clone(), listener));
    let endpoint = tonic::transport::Channel::from_shared(format!("http://{addr}")).unwrap();
    let mut channel = None;
    for _ in 0..50 {
        if let Ok(c) = endpoint.connect().await {
            channel = Some(c);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut client = pb::safety_service_client::SafetyServiceClient::new(channel.expect("connect"));
    let photo = seed_photo(
        &state,
        0x0123_4567_89ab_cdef ^ u64::from(Uuid::new_v4().as_u128() as u16),
    )
    .await;
    let artifact: Uuid = sqlx::query_scalar("SELECT artifact_id FROM images WHERE id = $1")
        .bind(photo)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let report = client
        .retract_artifact(pb::RetractArtifactRequest {
            artifact_id: artifact.to_string(),
            reason: "test".into(),
            delete: false,
        })
        .await
        .unwrap()
        .into_inner();
    let listed = client
        .list_certificates(pb::ListCertificatesRequest {
            artifact_id: artifact.to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(listed
        .items
        .iter()
        .any(|c| c.id == report.event_certificate));
    let one = client
        .get_certificate(pb::GetCertificateRequest {
            id: report.event_certificate.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(one.decision, "user_decision");
    let summary = client
        .get_safety_summary(pb::GetSafetySummaryRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(summary.by_outcome.is_some());
    let bad = client
        .list_certificates(pb::ListCertificatesRequest {
            reason: "NOT_A_CODE".into(),
            ..Default::default()
        })
        .await;
    assert_eq!(bad.unwrap_err().code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn the_migration_is_reversible() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let name = format!("gather_mig_{}", &Uuid::new_v4().simple().to_string()[..12]);
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&state.pool)
        .await
        .unwrap();
    let url = state.config.database_url.clone();
    let base = url.rsplit_once('/').map(|(b, _)| b.to_string()).unwrap();
    let query = url
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    let pool = db::connect(&format!("{base}/{name}{query}")).await.unwrap();
    // Test this historical migration at its own schema version. Later
    // migrations deliberately depend on the safety columns being present.
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations
        .iter()
        .filter(|migration| migration.version <= 15)
    {
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
    }
    let exists = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'inference_certificates')",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(exists(pool.clone()).await);
    sqlx::raw_sql(include_str!(
        "../migrations-down/0015_explained_away.down.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations-down/0014_semantic_safety.down.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert!(!exists(pool.clone()).await, "down removes it");
    let column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name = 'atomic_units' AND column_name = 'asserted_at')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!column);
    sqlx::raw_sql(include_str!("../migrations/0014_semantic_safety.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0015_explained_away.sql"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(exists(pool.clone()).await, "and up applies again");
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&state.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn removing_the_last_source_of_an_automatic_merge_undoes_it() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let token = &Uuid::new_v4().simple().to_string()[..8];
    let (x, art_x) = seed_entity_with_source(&state, &format!("Plover {token} Maritime")).await;
    let (y, art_y) = seed_entity_with_source(&state, &format!("Plover {token} Maritime.")).await;
    cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    assert_eq!(head(&state, x).await, Some(y));

    // One source left: the merge still has support.
    let (_, first) = call(
        &app,
        Method::POST,
        &format!("/artifacts/{art_x}/retract"),
        Some(json!({})),
    )
    .await;
    assert_eq!(first["merges_withdrawn"], 0);
    assert_eq!(head(&state, x).await, Some(y));
    // No source left: the merge is undone at once.
    let (_, second) = call(
        &app,
        Method::POST,
        &format!("/artifacts/{art_y}/retract"),
        Some(json!({})),
    )
    .await;
    assert_eq!(second["merges_withdrawn"], 1, "{second}");
    assert_eq!(head(&state, x).await, None);
    // And no later pass re-merges it without a source.
    cluster::worker::run_one_pass(&state.pool, &state.config)
        .await
        .unwrap();
    assert_eq!(head(&state, x).await, None);
}

async fn seed_artifact(state: &AppState, supersedes: Option<Uuid>) -> Uuid {
    let text = Uuid::new_v4().to_string();
    sqlx::query_scalar(
        "INSERT INTO artifacts (kind, byte_size, content_hash, raw_content, supersedes_artifact_id) \
         VALUES ('document_text', 1, encode(digest($1, 'sha256'), 'hex'), $2, $3) RETURNING id",
    )
    .bind(&text)
    .bind(text.as_bytes())
    .bind(supersedes)
    .fetch_one(&state.pool)
    .await
    .unwrap()
}

fn families_of(links: &[gather_daemon::safety::provenance::Derivation], ids: &[Uuid]) -> usize {
    use gather_daemon::safety::provenance::{families, SourceRef};
    let sources: Vec<SourceRef> = ids
        .iter()
        .map(|&artifact| SourceRef {
            artifact,
            fingerprint: None,
        })
        .collect();
    families(&sources, links).len()
}

#[tokio::test]
async fn source_families_survive_long_version_chains_and_deleted_parents() {
    use gather_daemon::safety::service::{add_derivation, derivations_for, retract_artifact};
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    // A <- B <- C <- D as versions: the ends are one lineage.
    let a = seed_artifact(&state, None).await;
    let b = seed_artifact(&state, Some(a)).await;
    let c = seed_artifact(&state, Some(b)).await;
    let d = seed_artifact(&state, Some(c)).await;
    let mut conn = state.pool.acquire().await.unwrap();
    let links = derivations_for(&mut conn, &[a, d]).await.unwrap();
    assert_eq!(families_of(&links, &[a, d]), 1);

    // Two summaries of one parent stay related after the parent is deleted.
    let parent = seed_artifact(&state, None).await;
    let s1 = seed_artifact(&state, None).await;
    let s2 = seed_artifact(&state, None).await;
    add_derivation(&state.pool, s1, parent, "summary")
        .await
        .unwrap();
    add_derivation(&state.pool, s2, parent, "summary")
        .await
        .unwrap();
    retract_artifact(&state.pool, parent, None, true, None)
        .await
        .unwrap();
    let links = derivations_for(&mut conn, &[s1, s2]).await.unwrap();
    assert_eq!(
        families_of(&links, &[s1, s2]),
        1,
        "siblings are still one family"
    );
}

#[tokio::test]
async fn restoring_a_rejected_claim_brings_its_contradiction_back() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let m = &Uuid::new_v4().simple().to_string()[..8];
    ingest(
        &app,
        &format!("rst-{m}-a"),
        &format!("My Sigma{m} budget is $50 per month."),
        "2026-04-01T10:00:00Z",
    )
    .await;
    ingest(
        &app,
        &format!("rst-{m}-b"),
        &format!("My Sigma{m} budget is $75 per month."),
        "2026-04-01T10:00:00Z",
    )
    .await;
    drain(&state).await;
    let b = unit_like(&state, &format!("Sigma{m} budget is $75")).await;
    let status = |state: AppState| async move {
        sqlx::query_scalar::<_, String>(
            "SELECT status::text FROM contradictions WHERE unit_a_id = $1 OR unit_b_id = $1",
        )
        .bind(b)
        .fetch_one(&state.pool)
        .await
        .unwrap()
    };
    assert_eq!(status(state.clone()).await, "open");
    call(
        &app,
        Method::POST,
        &format!("/units/{b}/reject"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status(state.clone()).await, "dismissed");
    call(
        &app,
        Method::POST,
        &format!("/units/{b}/restore"),
        Some(json!({})),
    )
    .await;
    drain(&state).await;
    assert_eq!(
        status(state.clone()).await,
        "open",
        "the conflict holds again"
    );
}

/// Two statements of the same rent three years apart: Gather reads a change
/// of state, not a contradiction. Returns (older, newer).
async fn explained_pair(state: &AppState, app: &axum::Router, tag: &str) -> (Uuid, Uuid) {
    let m = &Uuid::new_v4().simple().to_string()[..8];
    ingest(
        app,
        &format!("{tag}-{m}-a"),
        &format!("My {tag}{m} rent is $1200 per month."),
        "2023-01-01T10:00:00Z",
    )
    .await;
    ingest(
        app,
        &format!("{tag}-{m}-b"),
        &format!("My {tag}{m} rent is $1500 per month."),
        "2026-01-01T10:00:00Z",
    )
    .await;
    drain(state).await;
    (
        unit_like(state, &format!("{tag}{m} rent is $1200")).await,
        unit_like(state, &format!("{tag}{m} rent is $1500")).await,
    )
}

async fn explained_away(app: &axum::Router, unit: Uuid) -> Option<Value> {
    let (status, body) = call(
        app,
        Method::GET,
        "/contradictions/explained-away?limit=200",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["total"].as_i64().unwrap() >= body["items"].as_array().unwrap().len() as i64);
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["unit_a"]["id"] == json!(unit) || i["unit_b"]["id"] == json!(unit))
        .cloned()
}

async fn rescan(state: &AppState, units: &[Uuid]) {
    sqlx::query("UPDATE atomic_units SET contradiction_scanned_at = NULL WHERE id = ANY($1)")
        .bind(units)
        .execute(&state.pool)
        .await
        .unwrap();
    drain(state).await;
}

async fn unit_status(state: &AppState, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM atomic_units WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn confirming_an_explained_away_pair_reports_it_for_good() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let (older, newer) = explained_pair(&state, &app, "Upsilon").await;
    assert_eq!(unit_status(&state, older).await, "superseded");

    let item = explained_away(&app, older)
        .await
        .expect("listed for review");
    assert!(item["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["code"] == "TEMPORAL_SUCCESSION" && !r["text"].as_str().unwrap().is_empty()));
    assert_eq!(item["detection_method"], "rule:numeric-mismatch");
    let cert = item["certificate_id"].as_str().unwrap().to_string();

    let (s, body) = call(
        &app,
        Method::POST,
        &format!("/contradictions/explained-away/{cert}/confirm"),
        Some(json!({"note": "both are this year's rent"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["supersessions_reverted"], 1);
    let id = body["contradiction_id"].as_str().unwrap().to_string();
    let (s, detail) = call(&app, Method::GET, &format!("/contradictions/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(detail["status"], "open");
    assert_eq!(
        unit_status(&state, older).await,
        "active",
        "a real conflict means the older claim was never replaced"
    );
    let sup = certificates(&app, &format!("subject_id={older}&kind=fact_supersession")).await;
    assert_eq!(sup[0]["outcome"], "retracted");
    let blocked = certificates(&app, &format!("subject_id={older}&kind=contradiction")).await;
    assert!(blocked
        .iter()
        .all(|c| c["outcome"] != "blocked" || c["superseded_at"] != Value::Null));
    assert!(explained_away(&app, older).await.is_none(), "reviewed");

    // Once reviewed, the same certificate can't be decided again.
    let (s, _) = call(
        &app,
        Method::POST,
        &format!("/contradictions/explained-away/{cert}/agree"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A later scan of the same pair keeps the person's verdict.
    rescan(&state, &[older, newer]).await;
    let (_, detail) = call(&app, Method::GET, &format!("/contradictions/{id}"), None).await;
    assert_eq!(detail["status"], "open");
    assert_eq!(unit_status(&state, older).await, "active");
    assert!(explained_away(&app, older).await.is_none());
}

#[tokio::test]
async fn agreeing_with_an_explanation_takes_the_pair_off_the_list() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let (older, newer) = explained_pair(&state, &app, "Phi").await;
    let item = explained_away(&app, newer)
        .await
        .expect("listed for review");
    let cert = item["certificate_id"].as_str().unwrap().to_string();
    // An open contradiction left over from an earlier reading of the pair.
    let stale: Uuid = sqlx::query_scalar(
        "INSERT INTO contradictions (unit_a_id, unit_b_id, score, detection_method, explanation) \
         VALUES (LEAST($1::uuid, $2::uuid), GREATEST($1::uuid, $2::uuid), 0.5, 'rule:stale', \
                 'from an earlier reading') RETURNING id",
    )
    .bind(older)
    .bind(newer)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let (s, body) = call(
        &app,
        Method::POST,
        &format!("/contradictions/explained-away/{cert}/agree"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["contradictions_closed"], 1);
    let (_, detail) = call(&app, Method::GET, &format!("/contradictions/{stale}"), None).await;
    assert_eq!(detail["status"], "both_valid", "agreeing closes it");
    assert!(explained_away(&app, older).await.is_none());
    assert_eq!(
        unit_status(&state, older).await,
        "superseded",
        "history kept"
    );

    rescan(&state, &[older, newer]).await;
    let contradictions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contradictions WHERE unit_a_id IN ($1, $2) \
           AND unit_b_id IN ($1, $2) AND status = 'open'",
    )
    .bind(older)
    .bind(newer)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(contradictions, 0);
    assert!(
        explained_away(&app, older).await.is_none(),
        "stays reviewed"
    );
    let user = certificates(&app, &format!("subject_id={older}&kind=user_decision")).await;
    assert!(user
        .iter()
        .any(|c| c["rule_id"] == "user.contradiction_not_conflict"));

    // The agreed change of state still applies after the newer claim is
    // rejected and restored: the older one is superseded again.
    for action in ["reject", "restore"] {
        let (s, _) = call(
            &app,
            Method::POST,
            &format!("/units/{newer}/{action}"),
            Some(json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
    assert_eq!(unit_status(&state, older).await, "active");
    drain(&state).await;
    assert_eq!(
        unit_status(&state, older).await,
        "superseded",
        "the agreed succession is re-derived"
    );
    assert!(explained_away(&app, older).await.is_none());

    // Changing your mind later is still possible from the certificate the
    // rescan left in force.
    let live = certificates(
        &app,
        &format!("subject_id={older}&kind=contradiction&outcome=blocked&live=true"),
    )
    .await;
    let cert = live[0]["id"].as_str().unwrap();
    let (s, _) = call(
        &app,
        Method::POST,
        &format!("/contradictions/explained-away/{cert}/confirm"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contradictions WHERE unit_a_id IN ($1, $2) \
           AND unit_b_id IN ($1, $2) AND status = 'open'",
    )
    .bind(older)
    .bind(newer)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(open, 1);
    let (_, detail) = call(&app, Method::GET, &format!("/contradictions/{stale}"), None).await;
    assert_eq!(detail["detection_method"], "rule:numeric-mismatch");
    assert!(detail["explanation"]
        .as_str()
        .unwrap()
        .starts_with("You marked"));
    let verdicts = certificates(
        &app,
        &format!("subject_id={older}&kind=user_decision&live=true"),
    )
    .await;
    assert!(
        verdicts
            .iter()
            .all(|c| c["rule_id"] != "user.contradiction_not_conflict"),
        "the earlier verdict is superseded"
    );
}
