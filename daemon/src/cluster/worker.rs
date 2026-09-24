//! Clustering worker (autonomous pipeline, Phase B).
//!
//! Interval-driven, same shape as `scan::worker_loop`. Two passes per tick:
//!
//! 1. **Entity resolution** — reuses `entities::merge_suggestions` (the existing
//!    two-pass scorer) as the candidate edges, applies the conservative
//!    `decide::merge_decision` gate to each, auto-merges the components whose
//!    edges all clear the Auto bar, and parks the rest in `review_queue`. This
//!    is one decision per component, not per pair.
//!
//! 2. **Topic grouping** — claims a batch of unclustered active units and groups
//!    them with the mutual-kNN + connected-components primitive over statement
//!    token similarity (offline, deterministic; embedding-based topics are a
//!    follow-up). Assignment is a reversible `topic_cluster_id` tag.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{cohesion, components, grouped, label_from_texts, mutual_knn, Edge};
use crate::config::Config;
use crate::decide::{merge_decision, Band, MergeSignals, MergeThresholds};
use crate::entities::{merge_entities, merge_suggestions};
use crate::scan::score::{all_tokens, jaccard};

#[derive(Debug, Default)]
pub struct ClusterStats {
    pub entities_merged: usize,
    pub entity_pairs_parked: usize,
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
    entity_resolution_pass(pool, config, &mut stats).await?;
    topic_clustering_pass(pool, config, &mut stats).await?;
    Ok(stats)
}

async fn entity_resolution_pass(
    pool: &PgPool,
    config: &Config,
    stats: &mut ClusterStats,
) -> anyhow::Result<()> {
    let suggestions = merge_suggestions(pool, config.cluster_threshold, 1_000).await?;
    if suggestions.is_empty() {
        return Ok(());
    }
    let thresholds = MergeThresholds::conservative();

    // Index entities appearing in suggestions; keep their names for canonical
    // selection.
    let mut ids: Vec<Uuid> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut index: HashMap<Uuid, usize> = HashMap::new();
    let mut intern = |id: Uuid, name: &str, ids: &mut Vec<Uuid>, names: &mut Vec<String>| {
        *index.entry(id).or_insert_with(|| {
            ids.push(id);
            names.push(name.to_string());
            ids.len() - 1
        })
    };

    let mut auto_edges: Vec<Edge> = Vec::new();
    for s in &suggestions {
        let signals = if s.method == "embedding:cosine" {
            MergeSignals {
                cosine: Some(s.score),
                text: None,
            }
        } else {
            MergeSignals {
                cosine: None,
                text: Some(s.score),
            }
        };
        match merge_decision(&signals, &thresholds) {
            Band::Auto => {
                let ia = intern(s.a.id, &s.a.name, &mut ids, &mut names);
                let ib = intern(s.b.id, &s.b.name, &mut ids, &mut names);
                let (a, b) = if ia < ib { (ia, ib) } else { (ib, ia) };
                auto_edges.push(Edge { a, b, sim: s.score });
            }
            Band::Hold => {
                let parked = sqlx::query(
                    "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
                     VALUES ('entity', $1, 'merge-band', $2) ON CONFLICT DO NOTHING",
                )
                .bind(s.a.id)
                .bind(json!({ "other": s.b.id, "score": s.score, "method": s.method }))
                .execute(pool)
                .await?
                .rows_affected();
                stats.entity_pairs_parked += parked as usize;
            }
            Band::Drop => {}
        }
    }

    if auto_edges.is_empty() {
        return Ok(());
    }
    let comp = components(ids.len(), &auto_edges);
    for group in grouped(&comp) {
        if group.len() < 2 {
            continue;
        }
        // Canonical survivor: the longest name (most information), tie-broken by
        // lowest index for determinism.
        let winner_local = *group
            .iter()
            .max_by(|&&x, &&y| {
                names[x]
                    .chars()
                    .count()
                    .cmp(&names[y].chars().count())
                    .then(y.cmp(&x))
            })
            .expect("non-empty group");
        let winner_id = ids[winner_local];
        let coh = cohesion(&group, &auto_edges);

        let cluster_id: Uuid = sqlx::query_scalar(
            "INSERT INTO clusters (kind, label, cohesion, size) \
             VALUES ('entity', $1, $2, $3) RETURNING id",
        )
        .bind(&names[winner_local])
        .bind(coh)
        .bind(group.len() as i32)
        .fetch_one(pool)
        .await?;

        for &local in &group {
            let member_id = ids[local];
            sqlx::query(
                "INSERT INTO cluster_members (cluster_id, member_kind, member_id, sim) \
                 VALUES ($1, 'entity', $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(cluster_id)
            .bind(member_id)
            .bind(coh)
            .execute(pool)
            .await?;
            if member_id != winner_id {
                merge_entities(
                    pool,
                    winner_id,
                    member_id,
                    Some("auto-clustered duplicate".to_string()),
                    Some("auto".to_string()),
                )
                .await?;
                stats.entities_merged += 1;
            }
        }
    }
    Ok(())
}

async fn topic_clustering_pass(
    pool: &PgPool,
    config: &Config,
    stats: &mut ClusterStats,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    // Claim a batch: FOR UPDATE SKIP LOCKED so a second worker never processes
    // the same units, and the cursor stamp below lands in the same transaction.
    let rows = sqlx::query(
        "SELECT id, statement FROM atomic_units \
         WHERE clustered_at IS NULL AND status = 'active' \
         ORDER BY created_at LIMIT $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(config.cluster_batch)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        tx.rollback().await?;
        return Ok(());
    }

    let ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
    let statements: Vec<String> = rows.iter().map(|r| r.get("statement")).collect();
    let tokens: Vec<Vec<String>> = statements.iter().map(|s| all_tokens(s)).collect();

    let edges = mutual_knn(
        ids.len(),
        config.cluster_k,
        config.cluster_threshold,
        |i, j| jaccard(&tokens[i], &tokens[j]),
    );
    for group in grouped(&components(ids.len(), &edges)) {
        // Skip singletons and blobs too diffuse to auto-label (chaining guard).
        if group.len() < 2 || group.len() > config.cluster_max_component {
            continue;
        }
        let texts: Vec<&str> = group.iter().map(|&i| statements[i].as_str()).collect();
        let label = label_from_texts(&texts);
        let coh = cohesion(&group, &edges);

        let cluster_id: Uuid = sqlx::query_scalar(
            "INSERT INTO clusters (kind, label, cohesion, size) \
             VALUES ('topic', $1, $2, $3) RETURNING id",
        )
        .bind(&label)
        .bind(coh)
        .bind(group.len() as i32)
        .fetch_one(&mut *tx)
        .await?;

        for &i in &group {
            sqlx::query(
                "INSERT INTO cluster_members (cluster_id, member_kind, member_id, sim) \
                 VALUES ($1, 'unit', $2, $3)",
            )
            .bind(cluster_id)
            .bind(ids[i])
            .bind(coh)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE atomic_units SET topic_cluster_id = $2 WHERE id = $1")
                .bind(ids[i])
                .bind(cluster_id)
                .execute(&mut *tx)
                .await?;
        }
        stats.topics_created += 1;
    }

    // Stamp every claimed unit — including singletons and oversized components —
    // so the cursor advances and they are not reprocessed each pass.
    sqlx::query("UPDATE atomic_units SET clustered_at = now() WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    stats.units_clustered += ids.len();
    Ok(())
}
