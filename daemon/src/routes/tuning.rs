//! Tuning surface (autonomous pipeline, Phase C): what the thresholds are now,
//! how they got there, and a reset back to the env defaults. The tuner is only
//! trustworthy if every move it made can be inspected and undone. The cores
//! are shared by REST and gRPC.

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::config::Config;
use crate::decide::live::{
    LiveThresholds, ADMIT_HOLD_BOUNDS, KEY_ADMIT_HOLD_BELOW, KEY_MERGE_AGREE,
    KEY_MERGE_AUTO_SINGLE, MERGE_AGREE_BOUNDS, MERGE_AUTO_SINGLE_BOUNDS,
};
use crate::error::ApiError;
use crate::tune::Bounds;
use crate::AppState;

/// Audit rows returned by GET /tuning.
const HISTORY_LIMIT: i64 = 50;

#[derive(Debug, Serialize)]
pub struct BoundsOut {
    pub min: f32,
    pub max: f32,
}

#[derive(Debug, Serialize)]
pub struct ThresholdOut {
    pub key: &'static str,
    pub value: f32,
    pub default: f32,
    pub tuned: bool,
    pub bounds: BoundsOut,
}

#[derive(Debug, Serialize)]
pub struct TuningChange {
    pub key: String,
    pub old_value: Option<f64>,
    pub new_value: Option<f64>,
    pub actor: String,
    pub reason: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct TuningState {
    pub enabled: bool,
    pub target_precision: f32,
    pub min_samples: usize,
    pub thresholds: Vec<ThresholdOut>,
    pub history: Vec<TuningChange>,
}

fn threshold(
    key: &'static str,
    value: f32,
    default: f32,
    bounds: Bounds,
    tuned: bool,
) -> ThresholdOut {
    ThresholdOut {
        key,
        value,
        default,
        tuned,
        bounds: BoundsOut {
            min: bounds.min,
            max: bounds.max,
        },
    }
}

/// Live thresholds, their defaults and bounds, and recent changes.
pub async fn get_tuning_core(pool: &PgPool, config: &Config) -> Result<TuningState, ApiError> {
    let defaults = LiveThresholds::from_config(config);
    let live = LiveThresholds::load(pool, config).await?;
    let tuned_keys: Vec<String> = sqlx::query_scalar("SELECT key FROM decision_tuning")
        .fetch_all(pool)
        .await?;
    let is_tuned = |key: &str| tuned_keys.iter().any(|k| k == key);

    let history = sqlx::query(
        "SELECT key, old_value, new_value, actor, reason, created_at \
         FROM decision_tuning_audit ORDER BY created_at DESC LIMIT $1",
    )
    .bind(HISTORY_LIMIT)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| TuningChange {
        key: r.get("key"),
        old_value: r.get("old_value"),
        new_value: r.get("new_value"),
        actor: r.get("actor"),
        reason: r.get("reason"),
        created_at: r.get("created_at"),
    })
    .collect();

    Ok(TuningState {
        enabled: config.tune_enabled,
        target_precision: config.tune_target_precision,
        min_samples: config.tune_min_samples,
        thresholds: vec![
            threshold(
                KEY_ADMIT_HOLD_BELOW,
                live.admit_hold_below,
                defaults.admit_hold_below,
                ADMIT_HOLD_BOUNDS,
                is_tuned(KEY_ADMIT_HOLD_BELOW),
            ),
            threshold(
                KEY_MERGE_AUTO_SINGLE,
                live.merge.auto_single,
                defaults.merge.auto_single,
                MERGE_AUTO_SINGLE_BOUNDS,
                is_tuned(KEY_MERGE_AUTO_SINGLE),
            ),
            threshold(
                KEY_MERGE_AGREE,
                live.merge.agree_cosine.min(live.merge.agree_text),
                defaults.merge.agree_cosine.min(defaults.merge.agree_text),
                MERGE_AGREE_BOUNDS,
                is_tuned(KEY_MERGE_AGREE),
            ),
        ],
        history,
    })
}

/// GET /tuning
pub async fn get_tuning(State(state): State<AppState>) -> Result<Json<TuningState>, ApiError> {
    Ok(Json(get_tuning_core(&state.pool, &state.config).await?))
}

#[derive(Deserialize, Default)]
pub struct ResetRequest {
    /// One key to reset; all tuned keys when absent.
    pub key: Option<String>,
}

/// Drop learned values (one key, or all) so the env defaults apply again.
/// Returns the keys that were reset. Each reset is audited, and the audit row
/// is the cutoff the tuner uses to ignore pre-reset feedback.
pub async fn reset_tuning_core(pool: &PgPool, key: Option<&str>) -> Result<Vec<String>, ApiError> {
    if let Some(k) = key {
        if ![KEY_ADMIT_HOLD_BELOW, KEY_MERGE_AUTO_SINGLE, KEY_MERGE_AGREE].contains(&k) {
            return Err(ApiError::BadRequest(format!("unknown tuning key '{k}'")));
        }
    }

    let mut tx = pool.begin().await?;
    let removed: Vec<(String, f64)> = sqlx::query_as(
        "DELETE FROM decision_tuning WHERE $1::text IS NULL OR key = $1 RETURNING key, value",
    )
    .bind(key)
    .fetch_all(&mut *tx)
    .await?;
    for (k, old) in &removed {
        sqlx::query(
            "INSERT INTO decision_tuning_audit (key, old_value, new_value, actor, reason) \
             VALUES ($1, $2, NULL, 'local-user', '{\"action\": \"reset\"}'::jsonb)",
        )
        .bind(k)
        .bind(old)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(removed.into_iter().map(|(k, _)| k).collect())
}

/// POST /tuning/reset
pub async fn reset_tuning(
    State(state): State<AppState>,
    body: Option<Json<ResetRequest>>,
) -> Result<Json<Value>, ApiError> {
    let key = body.and_then(|b| b.0.key);
    let keys = reset_tuning_core(&state.pool, key.as_deref()).await?;
    Ok(Json(json!({ "reset": keys })))
}
