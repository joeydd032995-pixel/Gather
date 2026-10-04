//! End-to-end tests for the clustering worker (autonomous pipeline, Phase B):
//! conservative entity auto-merge and topic grouping. Skipped without
//! DATABASE_URL, like the other integration suites.

use std::sync::Arc;

use uuid::Uuid;

use gather_daemon::cluster::pair_key;
use gather_daemon::cluster::worker::run_one_pass;
use gather_daemon::config::Config;
use gather_daemon::entities::similarity::name_similarity;
use gather_daemon::{db, AppState};

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
        rate_limiter: None,
    })
}

/// An entity mentioned by a unit in a real (seeded) source: automatic merges
/// need at least one source artifact behind the names they join.
async fn seed_entity(state: &AppState, name: &str) -> Uuid {
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO entities (name, kind) VALUES ($1, 'other') RETURNING id")
            .bind(name)
            .fetch_one(&state.pool)
            .await
            .expect("seed entity");
    let text = format!("A note about {name} {}", Uuid::new_v4());
    let artifact: Uuid = sqlx::query_scalar(
        "INSERT INTO artifacts (kind, byte_size, content_hash, raw_content) \
         VALUES ('document_text', 1, encode(digest($1, 'sha256'), 'hex'), $2) RETURNING id",
    )
    .bind(&text)
    .bind(text.as_bytes())
    .fetch_one(&state.pool)
    .await
    .expect("seed artifact");
    let document: Uuid =
        sqlx::query_scalar("INSERT INTO documents (artifact_id) VALUES ($1) RETURNING id")
            .bind(artifact)
            .fetch_one(&state.pool)
            .await
            .expect("seed document");
    let segment: Uuid = sqlx::query_scalar(
        "INSERT INTO document_segments (document_id, seq, content, content_hash, units_extracted_at) \
         VALUES ($1, 0, $2, encode(digest($2, 'sha256'), 'hex'), now()) RETURNING id",
    )
    .bind(document)
    .bind(&text)
    .fetch_one(&state.pool)
    .await
    .expect("seed segment");
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
    .expect("seed unit");
    sqlx::query(
        "INSERT INTO atomic_unit_provenance (atomic_unit_id, artifact_id, document_segment_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(unit)
    .bind(artifact)
    .bind(segment)
    .execute(&state.pool)
    .await
    .expect("seed provenance");
    id
}

async fn seed_unit(state: &AppState, statement: &str) -> Uuid {
    let hash = format!("clust-{}", Uuid::new_v4());
    sqlx::query_scalar(
        "INSERT INTO atomic_units (kind, statement, statement_hash, confidence, extraction_method) \
         VALUES ('fact', $1, $2, 0.6, 'rule_based') RETURNING id",
    )
    .bind(statement)
    .bind(&hash)
    .fetch_one(&state.pool)
    .await
    .expect("seed unit")
}

async fn merged_into(state: &AppState, id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn worker_auto_merges_duplicates_and_groups_topics() {
    let Some(state) = test_state().await else {
        return;
    };
    // Entity resolution and topic grouping are asserted in ONE test with ONE
    // pass: cargo runs test fns in parallel, and two concurrent worker passes
    // over the same shared DB would split units across their FOR UPDATE SKIP
    // LOCKED batches. One pass, no race.

    // Two names that differ only by a trailing dot -> text similarity >= 0.92,
    // which clears the conservative single-signal Auto bar. Unique per run so
    // the name-uniqueness index doesn't collide across runs.
    let base = format!("Acme Holdings {}", Uuid::new_v4());
    let short = seed_entity(&state, &base).await;
    let long = seed_entity(&state, &format!("{base}.")).await;

    // A chain: A~B and B~C each clear the Auto bar, A~C does not. Joined only
    // through B, they must not all fold into one entity.
    let token = &Uuid::new_v4().simple().to_string()[..8];
    let chain_a_name = format!("Kestrel {token} Orchard Growers Guild");
    let chain_b_name = format!("{chain_a_name}s");
    let chain_c_name = format!("{chain_b_name} Co");
    // Pin the fixture's shape, so a change to the scorer fails loudly here
    // rather than silently testing something else.
    assert!(name_similarity(&chain_a_name, &chain_b_name) >= 0.92);
    assert!(name_similarity(&chain_b_name, &chain_c_name) >= 0.92);
    assert!(name_similarity(&chain_a_name, &chain_c_name) < 0.92);
    let chain_a = seed_entity(&state, &chain_a_name).await;
    let chain_b = seed_entity(&state, &chain_b_name).await;
    let chain_c = seed_entity(&state, &chain_c_name).await;

    let tag = Uuid::new_v4().simple().to_string();
    let ids = [
        seed_unit(&state, &format!("backup target hetzner cx22 {tag}")).await,
        seed_unit(&state, &format!("backup target hetzner chosen {tag}")).await,
        seed_unit(&state, &format!("backup target hetzner selected {tag}")).await,
    ];

    run_one_pass(&state.pool, &state.config)
        .await
        .expect("cluster pass");

    // The shorter name folds into the longer (more information) survivor.
    assert_eq!(merged_into(&state, short).await, Some(long));
    assert_eq!(merged_into(&state, long).await, None);

    // The chain was not merged; each of its Auto pairs waits in the tray.
    for id in [chain_a, chain_b, chain_c] {
        assert_eq!(
            merged_into(&state, id).await,
            None,
            "a chained entity was merged"
        );
    }
    for (x, y) in [(chain_a, chain_b), (chain_b, chain_c)] {
        let parked: Option<String> = sqlx::query_scalar(
            "SELECT reason FROM review_queue \
             WHERE target_kind = 'entity' AND target_id = $1 AND state = 'open'",
        )
        .bind(pair_key(x, y))
        .fetch_optional(&state.pool)
        .await
        .unwrap();
        assert_eq!(
            parked.as_deref(),
            Some("merge-band"),
            "chained pair not parked"
        );
    }

    // An entity cluster was recorded.
    let entity_clusters: i64 =
        sqlx::query_scalar("SELECT count(*) FROM clusters WHERE kind = 'entity'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(entity_clusters >= 1);

    // All three were claimed (cursor stamped) and share one topic cluster.
    let clusters: Vec<Option<Uuid>> = {
        let mut out = Vec::new();
        for id in ids {
            let (stamped, cluster): (Option<chrono::DateTime<chrono::Utc>>, Option<Uuid>) =
                sqlx::query_as(
                    "SELECT clustered_at, topic_cluster_id FROM atomic_units WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
            assert!(stamped.is_some(), "unit should be marked clustered");
            out.push(cluster);
        }
        out
    };
    assert!(
        clusters[0].is_some() && clusters.iter().all(|c| *c == clusters[0]),
        "the three overlapping units should share one topic cluster, got {clusters:?}"
    );

    let topic = clusters[0].unwrap();
    let label: String = sqlx::query_scalar("SELECT label FROM clusters WHERE id = $1")
        .bind(topic)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(!label.is_empty(), "topic cluster should have a label");

    // Cross-batch attach: a unit arriving in a LATER pass, overlapping the same
    // tokens, joins the existing cluster via the context window instead of
    // stranding as a singleton.
    let late = seed_unit(&state, &format!("backup target hetzner later {tag}")).await;
    run_one_pass(&state.pool, &state.config)
        .await
        .expect("second cluster pass");
    let late_cluster: Option<Uuid> =
        sqlx::query_scalar("SELECT topic_cluster_id FROM atomic_units WHERE id = $1")
            .bind(late)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        late_cluster,
        Some(topic),
        "a later overlapping unit should attach to the existing topic cluster"
    );
}

/// A pair parked in the tray before names were checked ("Zero" / "Non-zero")
/// is closed by the next pass rather than left for a person to dismiss.
#[tokio::test]
async fn tray_items_about_opposites_are_closed() {
    let Some(state) = test_state().await else {
        return;
    };
    let salt = Uuid::new_v4().simple().to_string()[..8].to_string();
    let a = seed_entity(&state, &format!("zero{salt}")).await;
    let b = seed_entity(&state, &format!("non-zero{salt}")).await;
    let item: Uuid = sqlx::query_scalar(
        "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
         VALUES ('entity', $1, 'merge-band', $2) RETURNING id",
    )
    .bind(pair_key(a, b))
    .bind(serde_json::json!({ "a": a, "b": b, "score": 0.67 }))
    .fetch_one(&state.pool)
    .await
    .expect("seed tray item");

    run_one_pass(&state.pool, &state.config)
        .await
        .expect("cluster pass");

    let tray: String = sqlx::query_scalar("SELECT state FROM review_queue WHERE id = $1")
        .bind(item)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(tray, "dismissed", "opposites are not worth a question");
    assert_eq!(merged_into(&state, a).await, None);
    assert_eq!(merged_into(&state, b).await, None);
}
