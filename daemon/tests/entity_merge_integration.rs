//! Entity-resolution tests against a real Postgres (pgvector).
//! Skipped without DATABASE_URL, like the other integration suites.
//!
//! The headline case is the regression at the bottom of `merges_are_visible_to_the_resolver`:
//! before this feature, `entity_aliases` had no writer anywhere in the daemon,
//! so the alias branch of `resolve_or_create_entity` (extract/persist.rs)
//! could never match and every surface form of a name became its own node.

use std::sync::Arc;

use sqlx::Row;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::{db, entities, extract, AppState};

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

/// Unique suffix so parallel tests and repeat runs never collide on the
/// partial unique index over entity names.
fn tag() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

async fn new_entity(pool: &sqlx::PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO entities (name, kind) VALUES ($1, 'other') RETURNING id")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("insert entity")
}

async fn new_edge(pool: &sqlx::PgPool, source: Uuid, target: Uuid, relation: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO relationships (source_entity_id, target_entity_id, relation_type) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(source)
    .bind(target)
    .bind(relation)
    .fetch_one(pool)
    .await
    .expect("insert relationship")
}

async fn entity_count_named(pool: &sqlx::PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM entities WHERE lower(name) = lower($1) \
         AND merged_into_entity_id IS NULL",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("count")
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn merges_are_visible_to_the_resolver() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let (winner_name, loser_name) = (format!("Postgres-{t}"), format!("PostgreSQL-{t}"));

    let winner = new_entity(&state.pool, &winner_name).await;
    let loser = new_entity(&state.pool, &loser_name).await;

    // A unit whose subject is the loser, so we can prove it gets repointed.
    let unit: Uuid = sqlx::query_scalar(
        "INSERT INTO atomic_units \
             (kind, statement, statement_hash, subject_entity_id, extraction_method) \
         VALUES ('fact', $1, encode(digest($1,'sha256'),'hex'), $2, 'manual') RETURNING id",
    )
    .bind(format!("statement about {loser_name}"))
    .bind(loser)
    .fetch_one(&state.pool)
    .await
    .expect("insert unit");

    let outcome = entities::merge_entities(&state.pool, winner, loser, None, None)
        .await
        .expect("merge");

    assert_eq!(outcome.units_repointed, 1);
    // The loser's name became an alias of the winner.
    assert!(
        outcome.aliases_added >= 1,
        "expected the loser's name as an alias"
    );

    let subject: Uuid =
        sqlx::query_scalar("SELECT subject_entity_id FROM atomic_units WHERE id = $1")
            .bind(unit)
            .fetch_one(&state.pool)
            .await
            .expect("read unit");
    assert_eq!(subject, winner, "unit should now describe the winner");

    // The loser is soft-deleted, not dropped: provenance survives.
    let merged_into: Option<Uuid> =
        sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
            .bind(loser)
            .fetch_one(&state.pool)
            .await
            .expect("read loser");
    assert_eq!(merged_into, Some(winner));

    // --- the regression this whole feature exists for ----------------------
    // Re-sighting the loser's name must resolve to the winner through the
    // alias, instead of creating a third node. Before this change the alias
    // branch of resolve_or_create_entity was dead code.
    let mut tx = state.pool.begin().await.expect("begin");
    let resolved = extract::persist::resolve_or_create_entity(&mut tx, &loser_name)
        .await
        .expect("resolve");
    tx.commit().await.expect("commit");

    assert_eq!(
        resolved, winner,
        "the merged-away name must resolve to the surviving entity via its alias"
    );
    assert_eq!(
        entity_count_named(&state.pool, &loser_name).await,
        0,
        "resolving the alias must not resurrect the loser as a live entity"
    );
}

#[tokio::test]
async fn merge_drops_edges_that_would_violate_constraints() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let winner = new_entity(&state.pool, &format!("Winner-{t}")).await;
    let loser = new_entity(&state.pool, &format!("Loser-{t}")).await;
    let other = new_entity(&state.pool, &format!("Other-{t}")).await;

    // (a) An edge directly between the pair would become a self-loop once
    //     repointed — relationships_no_self_loop rejects that.
    let self_loop = new_edge(&state.pool, loser, winner, "relates_to").await;
    // (b) Both entities assert the same edge to a third party; repointing
    //     collides under relationships_edge_uq.
    let kept = new_edge(&state.pool, winner, other, "uses").await;
    let duplicate = new_edge(&state.pool, loser, other, "uses").await;
    // (c) A distinct edge that must survive and be repointed.
    let moved = new_edge(&state.pool, loser, other, "depends_on").await;

    let outcome = entities::merge_entities(&state.pool, winner, loser, None, None)
        .await
        .expect("merge must not abort on constraint collisions");

    assert_eq!(outcome.relationships_dropped, 2, "self-loop + duplicate");
    assert_eq!(outcome.relationships_repointed, 1, "the distinct edge");

    for (id, expected, label) in [
        (self_loop, false, "self-loop edge"),
        (duplicate, false, "duplicate edge"),
        (kept, true, "winner's original edge"),
        (moved, true, "distinct edge"),
    ] {
        let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM relationships WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .expect("query edge");
        assert_eq!(exists.is_some(), expected, "{label} presence");
    }

    let source: Uuid =
        sqlx::query_scalar("SELECT source_entity_id FROM relationships WHERE id = $1")
            .bind(moved)
            .fetch_one(&state.pool)
            .await
            .expect("read moved edge");
    assert_eq!(source, winner);
}

#[tokio::test]
async fn merge_moves_aliases_and_tolerates_collisions() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let winner = new_entity(&state.pool, &format!("Winner-{t}")).await;
    let loser = new_entity(&state.pool, &format!("Loser-{t}")).await;

    let shared = format!("Shared-{t}");
    let only_loser = format!("OnlyLoser-{t}");
    for (entity, alias) in [(winner, &shared), (loser, &shared), (loser, &only_loser)] {
        sqlx::query("INSERT INTO entity_aliases (entity_id, alias) VALUES ($1, $2)")
            .bind(entity)
            .bind(alias)
            .execute(&state.pool)
            .await
            .expect("insert alias");
    }

    entities::merge_entities(&state.pool, winner, loser, None, None)
        .await
        .expect("alias collision must not abort the merge");

    let aliases: Vec<String> =
        sqlx::query_scalar("SELECT alias FROM entity_aliases WHERE entity_id = $1 ORDER BY alias")
            .bind(winner)
            .fetch_all(&state.pool)
            .await
            .expect("read aliases");

    assert!(
        aliases.contains(&shared),
        "shared alias retained exactly once"
    );
    assert_eq!(
        aliases.iter().filter(|a| **a == shared).count(),
        1,
        "entity_aliases_uq must not be violated or duplicated"
    );
    assert!(
        aliases.contains(&only_loser),
        "loser's unique alias moved over"
    );

    let leftover: i64 =
        sqlx::query_scalar("SELECT count(*) FROM entity_aliases WHERE entity_id = $1")
            .bind(loser)
            .fetch_one(&state.pool)
            .await
            .expect("count loser aliases");
    assert_eq!(leftover, 0, "loser's alias rows are cleaned up");
}

#[tokio::test]
async fn neighborhood_unions_both_sides_after_merge() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let winner = new_entity(&state.pool, &format!("Winner-{t}")).await;
    let loser = new_entity(&state.pool, &format!("Loser-{t}")).await;
    let winner_side = new_entity(&state.pool, &format!("WinnerSide-{t}")).await;
    let loser_side = new_entity(&state.pool, &format!("LoserSide-{t}")).await;

    new_edge(&state.pool, winner, winner_side, "uses").await;
    new_edge(&state.pool, loser, loser_side, "uses").await;

    entities::merge_entities(&state.pool, winner, loser, None, None)
        .await
        .expect("merge");

    let rows =
        sqlx::query("SELECT source_entity_id, target_entity_id FROM entity_neighborhood($1, 2)")
            .bind(winner)
            .fetch_all(&state.pool)
            .await
            .expect("neighborhood");

    let reached: Vec<Uuid> = rows
        .iter()
        .flat_map(|r| {
            [
                r.get::<Uuid, _>("source_entity_id"),
                r.get::<Uuid, _>("target_entity_id"),
            ]
        })
        .collect();

    assert!(reached.contains(&winner_side), "winner's own neighbour");
    assert!(
        reached.contains(&loser_side),
        "the merged-in entity's neighbour must now be reachable from the winner"
    );

    // A stale link to the merged id still resolves to the survivor.
    assert_eq!(
        entities::resolve_head(&state.pool, loser)
            .await
            .expect("head"),
        winner
    );
}

#[tokio::test]
async fn suggestions_rank_duplicates_without_ollama() {
    let Some(state) = test_state().await else {
        return;
    };
    // state.ollama is None here, so entities.embedding stays NULL and only the
    // offline text pass can contribute — which is the point of this test.
    let t = tag();
    let a = new_entity(&state.pool, &format!("Kubernetes{t}")).await;
    let b = new_entity(&state.pool, &format!("Kubernetes{t}Cluster")).await;
    let unrelated = new_entity(&state.pool, &format!("Zzzmongo{t}")).await;

    let suggestions = entities::merge_suggestions(&state.pool, 0.6, 500)
        .await
        .expect("suggestions");

    let pair_found = suggestions
        .iter()
        .any(|s| (s.a.id == a && s.b.id == b) || (s.a.id == b && s.b.id == a));
    assert!(pair_found, "the near-duplicate pair should be suggested");

    let unrelated_paired = suggestions
        .iter()
        .any(|s| s.a.id == unrelated || s.b.id == unrelated);
    assert!(
        !unrelated_paired,
        "unrelated entity should not be suggested"
    );

    for s in &suggestions {
        assert_eq!(
            s.method, "rule:name-similarity",
            "with no embeddings only the offline pass can fire"
        );
    }

    // Dismissing the pair removes it from later suggestions.
    entities::dismiss_suggestion(&state.pool, a, b, None, None)
        .await
        .expect("dismiss");
    let after = entities::merge_suggestions(&state.pool, 0.6, 500)
        .await
        .expect("suggestions after dismiss");
    assert!(
        !after
            .iter()
            .any(|s| (s.a.id == a && s.b.id == b) || (s.a.id == b && s.b.id == a)),
        "a dismissed pair must stay dismissed in both orderings"
    );
}

#[tokio::test]
async fn merge_guards_reject_bad_input() {
    let Some(state) = test_state().await else {
        return;
    };
    let t = tag();
    let a = new_entity(&state.pool, &format!("A-{t}")).await;
    let b = new_entity(&state.pool, &format!("B-{t}")).await;
    let c = new_entity(&state.pool, &format!("C-{t}")).await;

    assert!(
        entities::merge_entities(&state.pool, a, a, None, None)
            .await
            .is_err(),
        "self-merge must be rejected"
    );

    assert!(
        entities::merge_entities(&state.pool, a, Uuid::new_v4(), None, None)
            .await
            .is_err(),
        "unknown entity must 404"
    );

    entities::merge_entities(&state.pool, a, b, None, None)
        .await
        .expect("first merge");

    // b is already merged away; chaining would force every reader to walk a
    // chain, so it is rejected rather than silently re-targeted.
    assert!(
        entities::merge_entities(&state.pool, c, b, None, None)
            .await
            .is_err(),
        "merging an already-merged entity must be rejected"
    );
}
