//! Bounded graph traversal (`entity_neighborhood`, migration 0005).
//! Skipped without DATABASE_URL, like the other integration suites.
//!
//! The headline test is `matches_the_original_recursive_cte_exactly`: it
//! recreates 0001's path-enumerating implementation as a scratch function and
//! diffs `(relationship_id, depth)` against the current one for every root at
//! every depth. A faster traversal that returns a different graph is not a fix,
//! so that equivalence is checked in CI rather than proven once by hand.

use std::sync::Arc;

use sqlx::Row;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::{db, AppState};

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
    })
}

fn tag() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// A deliberately awkward fixture: a chain, a cycle, a hub, a disconnected
/// node, and an inactive edge that must stay invisible.
async fn seed_graph(pool: &sqlx::PgPool, t: &str) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for _ in 0..20 {
        // Independently random names, NOT a shared prefix plus an index: the
        // suggestion suite shares this database, and near-identical fixture
        // names would rank as duplicates and crowd its expected pair out of
        // the top-scoring results.
        let id: Uuid =
            sqlx::query_scalar("INSERT INTO entities (name, kind) VALUES ($1,'other') RETURNING id")
                .bind(Uuid::new_v4().simple().to_string())
                .fetch_one(pool)
                .await
                .expect("insert entity");
        ids.push(id);
    }

    // chain 0-1-2-3-4, cycle 1-2-5-1, back-edge 4-0; hub at 6 reaching 7..=15,
    // attached to the chain at 3; 16-17 a separate component; 0-18 inactive so
    // it must stay invisible; 19 isolated.
    let mut edges: Vec<(usize, usize, &str)> = vec![
        (0, 1, "active"),
        (1, 2, "active"),
        (2, 3, "active"),
        (3, 4, "active"),
        (2, 5, "active"),
        (5, 1, "active"),
        (4, 0, "active"),
        (3, 6, "active"),
        (16, 17, "active"),
        (0, 18, "superseded"),
    ];
    edges.extend((7..=15).map(|b| (6usize, b, "active")));

    for (a, b, status) in edges {
        sqlx::query(
            "INSERT INTO relationships (source_entity_id, target_entity_id, relation_type, status) \
             VALUES ($1,$2,$3,$4::unit_status)",
        )
        .bind(ids[a])
        .bind(ids[b])
        .bind(format!("gt{t}_{a}_{b}"))
        .bind(status)
        .execute(pool)
        .await
        .expect("insert relationship");
    }

    ids
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn matches_the_original_recursive_cte_exactly() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    // 0001's implementation verbatim, under a scratch name. Recreating it here
    // rather than shipping it in the migration keeps the old exploding walk out
    // of the deployed schema while still guarding the contract on every run.
    let scratch = format!("entity_neighborhood_ref_{t}");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        CREATE FUNCTION {scratch}(root uuid, max_depth integer DEFAULT 2)
        RETURNS TABLE (depth integer, relationship_id uuid, source_entity_id uuid,
                       target_entity_id uuid, relation_type text, confidence real)
        LANGUAGE sql STABLE AS $fn$
            WITH RECURSIVE walk AS (
                SELECT r.id AS relationship_id, r.source_entity_id, r.target_entity_id,
                       r.relation_type, r.confidence, 1 AS depth,
                       ARRAY[root, CASE WHEN r.source_entity_id = root
                                        THEN r.target_entity_id ELSE r.source_entity_id END] AS visited
                FROM relationships r
                WHERE r.status = 'active'
                  AND (r.source_entity_id = root OR r.target_entity_id = root)
                UNION ALL
                SELECT r.id, r.source_entity_id, r.target_entity_id, r.relation_type,
                       r.confidence, w.depth + 1,
                       w.visited || CASE WHEN r.source_entity_id = w.visited[array_upper(w.visited,1)]
                                         THEN r.target_entity_id ELSE r.source_entity_id END
                FROM relationships r
                JOIN walk w ON (r.source_entity_id = w.visited[array_upper(w.visited,1)]
                             OR r.target_entity_id = w.visited[array_upper(w.visited,1)])
                WHERE r.status = 'active' AND w.depth < max_depth
                  AND NOT (CASE WHEN r.source_entity_id = w.visited[array_upper(w.visited,1)]
                                THEN r.target_entity_id ELSE r.source_entity_id END = ANY (w.visited))
            )
            SELECT DISTINCT ON (relationship_id)
                   depth, relationship_id, source_entity_id, target_entity_id,
                   relation_type, confidence
            FROM walk ORDER BY relationship_id, depth;
        $fn$;
        "#
    )))
    .execute(&state.pool)
    .await
    .expect("create reference function");

    // Budgets high enough that the new walk cannot truncate, so any difference
    // is a semantic divergence rather than a cap.
    let mut checked = 0i64;
    for root in &ids {
        for depth in 1..=4 {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"
                WITH old AS (SELECT relationship_id, depth FROM {scratch}($1, $2)),
                     new AS (SELECT relationship_id, depth
                             FROM entity_neighborhood($1, $2, 100000, 0))
                SELECT (SELECT count(*) FROM (SELECT * FROM old EXCEPT SELECT * FROM new) a)
                     + (SELECT count(*) FROM (SELECT * FROM new EXCEPT SELECT * FROM old) b)
                       AS divergences,
                       (SELECT count(*) FROM old) AS old_rows
                "#
            )))
            .bind(root)
            .bind(depth)
            .fetch_one(&state.pool)
            .await
            .expect("diff");

            let divergences: i64 = row.get("divergences");
            let old_rows: i64 = row.get("old_rows");
            assert_eq!(
                divergences, 0,
                "root {root} depth {depth}: {divergences} (relationship_id, depth) divergences"
            );
            checked += old_rows;
        }
    }

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP FUNCTION {scratch}(uuid, integer)"
    )))
        .execute(&state.pool)
        .await
        .expect("drop reference function");

    assert!(
        checked > 0,
        "fixture produced no edges — the comparison proved nothing"
    );
}

#[tokio::test]
async fn inactive_edges_are_never_traversed() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    // ids[18] is only reachable through the 'superseded' edge from ids[0].
    let reached: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entity_neighborhood($1, 5, 100000, 0) \
         WHERE source_entity_id = $2 OR target_entity_id = $2",
    )
    .bind(ids[0])
    .bind(ids[18])
    .fetch_one(&state.pool)
    .await
    .expect("query");
    assert_eq!(reached, 0, "a superseded edge must be invisible to traversal");
}

#[tokio::test]
async fn cycles_terminate_and_do_not_duplicate_edges() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    // ids[0..6] contain two cycles; a path-based walk revisits their edges
    // many times. Each edge must still appear exactly once, at its min depth.
    let dupes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
           SELECT relationship_id FROM entity_neighborhood($1, 5, 100000, 0)
           GROUP BY relationship_id HAVING count(*) > 1
         ) d",
    )
    .bind(ids[0])
    .fetch_one(&state.pool)
    .await
    .expect("query");
    assert_eq!(dupes, 0, "each edge must appear exactly once");
}

#[tokio::test]
async fn a_small_neighbourhood_is_not_reported_as_truncated() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    let truncated: Vec<bool> =
        sqlx::query_scalar("SELECT truncated FROM entity_neighborhood($1, 1, 5000, 1000)")
            .bind(ids[0])
            .fetch_all(&state.pool)
            .await
            .expect("query");
    assert!(!truncated.is_empty(), "expected some edges");
    assert!(
        truncated.iter().all(|t| !t),
        "a neighbourhood well inside both budgets must not claim truncation"
    );
}

#[tokio::test]
async fn the_node_budget_bounds_the_walk_and_reports_it() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    // ids[6] is the hub. A budget of 3 nodes cannot cover its 9 neighbours.
    let truncated: Vec<bool> =
        sqlx::query_scalar("SELECT truncated FROM entity_neighborhood($1, 2, 3, 0)")
            .bind(ids[6])
            .fetch_all(&state.pool)
            .await
            .expect("query");
    assert!(!truncated.is_empty(), "expected some edges");
    assert!(
        truncated.iter().all(|t| *t),
        "exceeding the node budget must be reported on every row"
    );
}

#[tokio::test]
async fn the_edge_cap_keeps_the_closest_edges() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    let rows = sqlx::query(
        "SELECT depth, truncated FROM entity_neighborhood($1, 3, 100000, 4) ORDER BY depth",
    )
    .bind(ids[6])
    .fetch_all(&state.pool)
    .await
    .expect("query");

    assert_eq!(rows.len(), 4, "the cap must be respected exactly");
    assert!(
        rows.iter().all(|r| r.get::<bool, _>("truncated")),
        "a capped result must say so"
    );
    // Closest-first: a truncated answer should keep the immediate neighbourhood
    // rather than an arbitrary slice of a deeper one.
    assert_eq!(
        rows[0].get::<i32, _>("depth"),
        1,
        "the cap must retain the nearest edges"
    );
}

#[tokio::test]
async fn degenerate_inputs_return_empty_rather_than_erroring() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let ids = seed_graph(&state.pool, &t).await;

    for (label, root, depth) in [
        ("depth 0", ids[0], 0),
        ("negative depth", ids[0], -1),
        ("isolated node", ids[19], 3),
        ("unknown root", Uuid::new_v4(), 3),
    ] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM entity_neighborhood($1, $2, 5000, 0)",
        )
        .bind(root)
        .bind(depth)
        .fetch_one(&state.pool)
        .await
        .unwrap_or_else(|e| panic!("{label} should not error: {e}"));
        assert_eq!(n, 0, "{label} should return no rows");
    }
}
