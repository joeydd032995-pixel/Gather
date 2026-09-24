//! End-to-end tests for the clustering worker (autonomous pipeline, Phase B):
//! conservative entity auto-merge and topic grouping. Skipped without
//! DATABASE_URL, like the other integration suites.

use std::sync::Arc;

use uuid::Uuid;

use gather_daemon::cluster::worker::run_one_pass;
use gather_daemon::config::Config;
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

async fn seed_entity(state: &AppState, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO entities (name, kind) VALUES ($1, 'other') RETURNING id")
        .bind(name)
        .fetch_one(&state.pool)
        .await
        .expect("seed entity")
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
