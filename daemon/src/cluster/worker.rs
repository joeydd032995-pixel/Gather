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
use crate::entities::similarity::name_similarity;
use crate::entities::{merge_entities, merge_suggestions};
use crate::scan::score::{all_tokens, jaccard};

/// A stable id for an unordered entity pair, so a held merge review is keyed by
/// the pair (not one endpoint). Without this, two held suggestions sharing an
/// entity collide on `review_queue`'s (target_kind, target_id) unique index and
/// the second is silently dropped.
fn pair_key(a: Uuid, b: Uuid) -> Uuid {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("{lo}:{hi}").as_bytes())
}

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
    let mut kinds: Vec<String> = Vec::new();
    let mut index: HashMap<Uuid, usize> = HashMap::new();
    let mut intern = |id: Uuid,
                      name: &str,
                      kind: &str,
                      ids: &mut Vec<Uuid>,
                      names: &mut Vec<String>,
                      kinds: &mut Vec<String>| {
        *index.entry(id).or_insert_with(|| {
            ids.push(id);
            names.push(name.to_string());
            kinds.push(kind.to_string());
            ids.len() - 1
        })
    };

    let mut auto_edges: Vec<Edge> = Vec::new();
    for s in &suggestions {
        // Supply BOTH signals when they exist. merge_suggestions emits only the
        // embedding row for a pair it scored both ways, so recompute the text
        // similarity here; otherwise the two-signal agreement path can never
        // fire and a pair strong on both is needlessly held.
        let text_sim = name_similarity(&s.a.name, &s.b.name);
        let signals = if s.method == "embedding:cosine" {
            MergeSignals {
                cosine: Some(s.score),
                text: Some(text_sim),
            }
        } else {
            MergeSignals {
                cosine: None,
                text: Some(s.score),
            }
        };
        match merge_decision(&signals, &thresholds) {
            Band::Auto => {
                let ia = intern(
                    s.a.id, &s.a.name, &s.a.kind, &mut ids, &mut names, &mut kinds,
                );
                let ib = intern(
                    s.b.id, &s.b.name, &s.b.kind, &mut ids, &mut names, &mut kinds,
                );
                let (a, b) = if ia < ib { (ia, ib) } else { (ib, ia) };
                auto_edges.push(Edge { a, b, sim: s.score });
            }
            Band::Hold => {
                let parked = sqlx::query(
                    "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
                     VALUES ('entity', $1, 'merge-band', $2) ON CONFLICT DO NOTHING",
                )
                .bind(pair_key(s.a.id, s.b.id))
                .bind(json!({ "a": s.a.id, "b": s.b.id, "score": s.score, "method": s.method }))
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
        // Chaining guard: a component larger than the cap is too diffuse to
        // merge wholesale (a chain of adjacent-Auto pairs can join unrelated
        // endpoints). Park it for review instead of a destructive auto-merge.
        if group.len() > config.cluster_max_component {
            let parked = sqlx::query(
                "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
                 VALUES ('entity', $1, 'oversized-component', $2) ON CONFLICT DO NOTHING",
            )
            .bind(ids[group[0]])
            .bind(json!({ "members": group.iter().map(|&i| ids[i]).collect::<Vec<_>>() }))
            .execute(pool)
            .await?
            .rows_affected();
            stats.entity_pairs_parked += parked as usize;
            continue;
        }
        // Canonical survivor: prefer a specifically-typed entity over an
        // extraction-created 'other' (so a merge never discards the more
        // specific kind), then the longest name, then lowest index.
        let winner_local = *group
            .iter()
            .max_by(|&&x, &&y| {
                let typed_x = (kinds[x] != "other") as u8;
                let typed_y = (kinds[y] != "other") as u8;
                typed_x
                    .cmp(&typed_y)
                    .then(names[x].chars().count().cmp(&names[y].chars().count()))
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
