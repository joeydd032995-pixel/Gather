//! Active-learning / tuning worker (autonomous pipeline, Phase C).
//!
//! Interval-driven, same shape as `scan::worker_loop`. Each pass:
//!
//! 1. **Tune** (when `GATHER_TUNE_ENABLED`) — reads the user's latest verdict per
//!    target from `unit_feedback`, asks [`propose_threshold`] for a move, and
//!    writes any change to `decision_tuning` plus a `decision_tuning_audit` row
//!    in one transaction. Lowering the admission bar also drains tray entries
//!    that now clear it.
//! 2. **Re-rank the tray** — recomputes `review_queue.info_gain` for open items
//!    against the thresholds now in force.

use std::time::Duration;

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{
    boundary_uncertainty, info_gain, propose_threshold, Bounds, Direction, Label, TuneParams,
};
use crate::config::Config;
use crate::decide::live::{
    LiveThresholds, ADMIT_HOLD_BOUNDS, KEY_ADMIT_HOLD_BELOW, KEY_MERGE_AUTO_SINGLE,
    MERGE_AUTO_SINGLE_BOUNDS,
};

/// Largest threshold change per pass.
const MAX_STEP: f32 = 0.05;
/// Open tray items re-ranked per query page.
const RESCORE_PAGE: i64 = 5_000;
/// Uncertainty for held items with no single score (oversized components).
const NEUTRAL_UNCERTAINTY: f32 = 0.5;

/// One applied threshold change.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub key: &'static str,
    pub from: f32,
    pub to: f32,
    pub direction: Direction,
}

#[derive(Debug, Default)]
pub struct TuneStats {
    pub rescored: usize,
    pub drained: usize,
    pub changes: Vec<Change>,
}

/// Long-running entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    let mut interval = tokio::time::interval(Duration::from_secs(config.tune_interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match run_one_pass(&pool, &config).await {
            Ok(stats) if !stats.changes.is_empty() || stats.drained > 0 => {
                for c in &stats.changes {
                    tracing::info!(
                        key = c.key,
                        from = c.from,
                        to = c.to,
                        direction = c.direction.as_str(),
                        "decision threshold tuned from feedback"
                    );
                }
                tracing::info!(
                    rescored = stats.rescored,
                    drained = stats.drained,
                    "tuning pass complete"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "tuning pass failed"),
        }
    }
}

/// One pass (tune, then re-rank). Public for tests.
pub async fn run_one_pass(pool: &PgPool, config: &Config) -> anyhow::Result<TuneStats> {
    let mut live = LiveThresholds::load(pool, config).await?;
    let mut stats = TuneStats::default();

    if config.tune_enabled {
        let params = TuneParams {
            target_precision: f64::from(config.tune_target_precision),
            min_samples: config.tune_min_samples,
            max_step: MAX_STEP,
        };

        // The hold bar may never sink under the drop floor (that would erase
        // the hold band); min() keeps the bounds ordered if the floor is high.
        let admit_bounds = Bounds {
            min: ADMIT_HOLD_BOUNDS
                .min
                .max(live.admit_drop_below)
                .min(ADMIT_HOLD_BOUNDS.max),
            max: ADMIT_HOLD_BOUNDS.max,
        };
        let unit_labels = load_labels(pool, "unit", KEY_ADMIT_HOLD_BELOW).await?;
        if let Some(change) = tune_key(
            pool,
            KEY_ADMIT_HOLD_BELOW,
            live.admit_hold_below,
            admit_bounds,
            &unit_labels,
            &params,
        )
        .await?
        {
            live.admit_hold_below = change.to;
            if change.direction == Direction::Lower {
                stats.drained += drain_cleared_units(pool, change.to).await?;
            }
            stats.changes.push(change);
        }

        let merge_labels = load_labels(pool, "merge", KEY_MERGE_AUTO_SINGLE).await?;
        if let Some(change) = tune_key(
            pool,
            KEY_MERGE_AUTO_SINGLE,
            live.merge.auto_single,
            MERGE_AUTO_SINGLE_BOUNDS,
            &merge_labels,
            &params,
        )
        .await?
        {
            live.merge.auto_single = change.to;
            stats.changes.push(change);
        }
    }

    stats.rescored = rescore_review_queue(pool, &live).await?;
    metrics::gauge!("gather_decision_threshold", "key" => KEY_ADMIT_HOLD_BELOW)
        .set(f64::from(live.admit_hold_below));
    metrics::gauge!("gather_decision_threshold", "key" => KEY_MERGE_AUTO_SINGLE)
        .set(f64::from(live.merge.auto_single));
    Ok(stats)
}

/// The latest keep/reject verdict per target, with the score the item carried
/// when judged. A reject later undone by a restore counts as a keep.
///
/// Only verdicts given after the last `POST /tuning/reset` of `key` count, so
/// a reset is durable: the same historical labels can't re-create the value
/// on the next pass. New feedback after the reset tunes it again.
async fn load_labels(
    pool: &PgPool,
    target_kind: &str,
    key: &str,
) -> Result<Vec<Label>, sqlx::Error> {
    let rows: Vec<(String, Option<f32>)> = sqlx::query_as(
        "WITH cutoff AS ( \
           SELECT max(created_at) AS at FROM decision_tuning_audit \
           WHERE key = $2 AND reason->>'action' = 'reset' \
         ) \
         SELECT DISTINCT ON (f.target_id) f.action, COALESCE(f.score, u.confidence) \
         FROM unit_feedback f \
         LEFT JOIN atomic_units u ON f.target_kind = 'unit' AND u.id = f.target_id \
         CROSS JOIN cutoff \
         WHERE f.target_kind = $1 AND f.action IN ('confirm', 'reject') \
           AND (cutoff.at IS NULL OR f.created_at > cutoff.at) \
         ORDER BY f.target_id, f.created_at DESC, f.id DESC",
    )
    .bind(target_kind)
    .bind(key)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(action, score)| {
            score.map(|score| Label {
                score,
                keep: action == "confirm",
            })
        })
        .collect())
}

/// Propose and, on a move, persist a new value for `key` with its evidence.
async fn tune_key(
    pool: &PgPool,
    key: &'static str,
    current: f32,
    bounds: Bounds,
    labels: &[Label],
    params: &TuneParams,
) -> Result<Option<Change>, sqlx::Error> {
    let proposal = propose_threshold(labels, current, bounds, params);
    if proposal.direction == Direction::Stay {
        return Ok(None);
    }
    let reason = json!({
        "direction": proposal.direction.as_str(),
        // At/above the OLD threshold.
        "precision": proposal.precision,
        "support": proposal.support,
        // At the NEW threshold, when lowering.
        "lowering": proposal.lowering.map(|e| json!({
            "support": e.support,
            "wilson_lower_bound": e.lower_bound,
            "region_support": e.region_support,
            "region_precision": e.region_precision,
        })),
        "labels": labels.len(),
        "target_precision": params.target_precision,
        "min_samples": params.min_samples,
    });

    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO decision_tuning (key, value, updated_at) VALUES ($1, $2, now()) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(f64::from(proposal.value))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO decision_tuning_audit (key, old_value, new_value, actor, reason) \
         VALUES ($1, $2, $3, 'auto-tuner', $4)",
    )
    .bind(key)
    .bind(f64::from(current))
    .bind(f64::from(proposal.value))
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    metrics::counter!(
        "gather_tuning_changes_total",
        "key" => key, "direction" => proposal.direction.as_str()
    )
    .increment(1);
    Ok(Some(Change {
        key,
        from: current,
        to: proposal.value,
        direction: proposal.direction,
    }))
}

/// After the admission bar comes down, low-confidence tray entries that now
/// clear it are no longer ambiguous: dismiss them so the tray drains itself.
async fn drain_cleared_units(pool: &PgPool, hold_below: f32) -> Result<usize, sqlx::Error> {
    let drained = sqlx::query(
        "UPDATE review_queue SET state = 'dismissed' \
         WHERE state = 'open' AND target_kind = 'unit' AND reason = 'low-confidence' \
           AND (signals->>'confidence')::real >= $1",
    )
    .bind(hold_below)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(drained as usize)
}

/// Recompute `info_gain` for open tray items against the live thresholds.
/// Degree = graph facts the answer affects: for a unit, the relationships it
/// asserts plus other active units about the same subject; for a merge pair,
/// the active units about either entity; for an oversized component, its size.
///
/// Walks the whole open set in keyset pages, so every item is re-ranked each
/// pass however large the tray grows.
async fn rescore_review_queue(pool: &PgPool, live: &LiveThresholds) -> Result<usize, sqlx::Error> {
    let mut rescored = 0;
    let mut after = Uuid::nil();
    loop {
        let (page, last) = rescore_page(pool, live, after).await?;
        rescored += page;
        match last {
            Some(id) => after = id,
            None => return Ok(rescored),
        }
    }
}

/// Re-rank one page of open items with id > `after`. Returns the number
/// updated and the last id seen (None when the page was empty).
async fn rescore_page(
    pool: &PgPool,
    live: &LiveThresholds,
    after: Uuid,
) -> Result<(usize, Option<Uuid>), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT r.id, r.target_kind, r.reason, r.signals, \
           CASE \
             WHEN r.target_kind = 'unit' THEN \
               (SELECT count(*) FROM relationships rel \
                 WHERE rel.atomic_unit_id = r.target_id AND rel.status = 'active') \
             + (SELECT count(*) FROM atomic_units u \
                 JOIN atomic_units u2 ON u2.subject_entity_id = u.subject_entity_id \
                 WHERE u.id = r.target_id AND u2.id <> u.id AND u2.status = 'active') \
             WHEN r.reason = 'merge-band' AND r.signals ? 'a' AND r.signals ? 'b' THEN \
               (SELECT count(*) FROM atomic_units u \
                 WHERE u.status = 'active' AND u.subject_entity_id IN \
                   ((r.signals->>'a')::uuid, (r.signals->>'b')::uuid)) \
             WHEN r.reason = 'oversized-component' \
               AND jsonb_typeof(r.signals->'members') = 'array' THEN \
               jsonb_array_length(r.signals->'members')::bigint \
             ELSE 0 \
           END AS degree \
         FROM review_queue r WHERE r.state = 'open' AND r.id > $2 \
         ORDER BY r.id LIMIT $1",
    )
    .bind(RESCORE_PAGE)
    .bind(after)
    .fetch_all(pool)
    .await?;
    let Some(last) = rows.last().map(|r| r.get::<Uuid, _>("id")) else {
        return Ok((0, None));
    };

    let mut ids: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut gains: Vec<f32> = Vec::with_capacity(rows.len());
    for r in &rows {
        let target_kind: String = r.get("target_kind");
        let reason: String = r.get("reason");
        let signals: Value = r.get("signals");
        let degree: i64 = r.get("degree");
        let uncertainty = item_uncertainty(&target_kind, &reason, &signals, live);
        ids.push(r.get("id"));
        gains.push(info_gain(uncertainty, degree));
    }

    let updated = sqlx::query(
        "UPDATE review_queue r SET info_gain = v.gain \
         FROM UNNEST($1::uuid[], $2::real[]) AS v(id, gain) \
         WHERE r.id = v.id AND r.state = 'open'",
    )
    .bind(&ids)
    .bind(&gains)
    .execute(pool)
    .await?
    .rows_affected();
    Ok((updated as usize, Some(last)))
}

/// Closeness to the Auto boundary for one held item.
fn item_uncertainty(
    target_kind: &str,
    reason: &str,
    signals: &Value,
    live: &LiveThresholds,
) -> f32 {
    let score = |field: &str| signals.get(field).and_then(Value::as_f64).map(|v| v as f32);
    match (target_kind, reason) {
        ("unit", _) => score("confidence")
            .map(|c| boundary_uncertainty(c, live.admit_drop_below, live.admit_hold_below))
            .unwrap_or(NEUTRAL_UNCERTAINTY),
        (_, "merge-band") => score("score")
            .map(|s| boundary_uncertainty(s, live.merge.review_floor, live.merge.auto_single))
            .unwrap_or(NEUTRAL_UNCERTAINTY),
        _ => NEUTRAL_UNCERTAINTY,
    }
}
