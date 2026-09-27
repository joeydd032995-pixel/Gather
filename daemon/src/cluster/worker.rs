//! Clustering worker (autonomous pipeline, Phase B).
//!
//! Interval-driven, same shape as `scan::worker_loop`. Two passes per tick:
//!
//! 1. **Entity resolution** — `cluster::resolve`: plans merges with the
//!    `safety::identity` rule (every pair in a merged group needs its own
//!    qualifying evidence) over live and merged-away records, applies the
//!    plan, withdraws automatic merges later evidence turned into chains, and
//!    parks the rest in `review_queue`, each with a certificate.
//!
//! 2. **Topic grouping** — claims a batch of unclustered active units and groups
//!    them with the mutual-kNN + connected-components primitive over statement
//!    token similarity (offline, deterministic; embedding-based topics are a
//!    follow-up). Assignment is a reversible `topic_cluster_id` tag.

use std::time::Duration;

use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{cohesion, components, grouped, label_from_texts, mutual_knn};
use crate::config::Config;
use crate::scan::score::{all_tokens, jaccard};

#[derive(Debug, Default)]
pub struct ClusterStats {
    pub entities_merged: usize,
    pub entity_pairs_parked: usize,
    /// Automatic merges withdrawn because later evidence made them a chain.
    pub entities_withdrawn: usize,
    pub topics_created: usize,
    pub units_clustered: usize,
}

/// Long-running clustering entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    let mut interval = tokio::time::interval(Duration::from_secs(config.cluster_interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match run_one_pass(&pool, &config).await {
            Ok(stats)
                if stats.entities_merged + stats.units_clustered + stats.entity_pairs_parked
                    > 0 =>
            {
                tracing::info!(
                    merged = stats.entities_merged,
                    parked = stats.entity_pairs_parked,
                    topics = stats.topics_created,
                    units = stats.units_clustered,
                    "clustering pass complete"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "clustering pass failed"),
        }
    }
}

/// One full pass (entity resolution + topic grouping). Public for tests.
pub async fn run_one_pass(pool: &PgPool, config: &Config) -> anyhow::Result<ClusterStats> {
    let mut stats = ClusterStats::default();
    let resolved = super::resolve::entity_resolution_pass(pool, config).await?;
    stats.entities_merged = resolved.merged;
    stats.entity_pairs_parked = resolved.parked;
    stats.entities_withdrawn = resolved.withdrawn;
    topic_clustering_pass(pool, config, &mut stats).await?;
    Ok(stats)
}

async fn topic_clustering_pass(
    pool: &PgPool,
    config: &Config,
    stats: &mut ClusterStats,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    // Claim the unclustered batch: FOR UPDATE SKIP LOCKED so a second worker
    // never processes the same units, and the stamp below lands in the same tx.
    let new_rows = sqlx::query(
        "SELECT id, statement FROM atomic_units \
         WHERE clustered_at IS NULL AND status = 'active' \
         ORDER BY created_at LIMIT $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(config.cluster_batch)
    .fetch_all(&mut *tx)
    .await?;
    if new_rows.is_empty() {
        tx.rollback().await?;
        return Ok(());
    }

    // Context window: a bounded set of already-clustered units. Including them
    // in the graph lets a new unit attach to a cluster formed in an earlier
    // pass, so related units that fell on opposite sides of a batch boundary
    // still end up together instead of stranded as singletons. Only the NEW
    // units are ever assigned or stamped, so progress always advances.
    let ctx_rows = sqlx::query(
        "SELECT id, statement, topic_cluster_id FROM atomic_units \
         WHERE topic_cluster_id IS NOT NULL AND status = 'active' \
         ORDER BY created_at DESC LIMIT $1",
    )
    .bind(config.cluster_batch)
    .fetch_all(&mut *tx)
    .await?;

    let n = new_rows.len();
    let new_ids: Vec<Uuid> = new_rows.iter().map(|r| r.get("id")).collect();
    // Combined node list: new units first (0..n), then context units (n..).
    let mut statements: Vec<String> = new_rows.iter().map(|r| r.get("statement")).collect();
    // Per node: the existing cluster it already belongs to (None for new units).
    let mut ctx_cluster: Vec<Option<Uuid>> = vec![None; n];
    for r in &ctx_rows {
        statements.push(r.get("statement"));
        ctx_cluster.push(r.get("topic_cluster_id"));
    }
    let tokens: Vec<Vec<String>> = statements.iter().map(|s| all_tokens(s)).collect();
    let total = statements.len();

    let edges = mutual_knn(total, config.cluster_k, config.cluster_threshold, |i, j| {
        jaccard(&tokens[i], &tokens[j])
    });

    let mut clustered_new: Vec<Uuid> = Vec::new();
    for group in grouped(&components(total, &edges)) {
        // Only new units get assigned; a component with none is pure context.
        let new_in_group: Vec<usize> = group.iter().copied().filter(|&i| i < n).collect();
        if new_in_group.is_empty() {
            continue;
        }
        let coh = cohesion(&group, &edges);

        // Attach to an existing cluster if the component reaches one; otherwise
        // form a fresh topic cluster (needs >= 2 new units and must not be a
        // diffuse blob).
        let existing = group.iter().find_map(|&i| ctx_cluster[i]);
        let cluster_id = match existing {
            Some(cid) => cid,
            None => {
                if new_in_group.len() < 2 || group.len() > config.cluster_max_component {
                    continue;
                }
                let texts: Vec<&str> = new_in_group
                    .iter()
                    .map(|&i| statements[i].as_str())
                    .collect();
                let label = label_from_texts(&texts);
                let id: Uuid = sqlx::query_scalar(
                    "INSERT INTO clusters (kind, label, cohesion, size) \
                     VALUES ('topic', $1, $2, $3) RETURNING id",
                )
                .bind(&label)
                .bind(coh)
                .bind(new_in_group.len() as i32)
                .fetch_one(&mut *tx)
                .await?;
                stats.topics_created += 1;
                id
            }
        };

        for &i in &new_in_group {
            sqlx::query(
                "INSERT INTO cluster_members (cluster_id, member_kind, member_id, sim) \
                 VALUES ($1, 'unit', $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(cluster_id)
            .bind(new_ids[i])
            .bind(coh)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE atomic_units SET topic_cluster_id = $2 WHERE id = $1")
                .bind(new_ids[i])
                .bind(cluster_id)
                .execute(&mut *tx)
                .await?;
            clustered_new.push(new_ids[i]);
        }
    }

    // Stamp every claimed new unit — clustered or not — so the cursor always
    // advances (a permanent singleton must not be re-claimed forever). Context
    // units are already stamped and are never re-stamped here.
    sqlx::query("UPDATE atomic_units SET clustered_at = now() WHERE id = ANY($1)")
        .bind(&new_ids)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    stats.units_clustered += clustered_new.len();
    Ok(())
}
