//! Tuning surface (autonomous pipeline, Phase C): what the thresholds are now,
//! how they got there, and a reset back to the env defaults. The tuner is only
//! trustworthy if every move it made can be inspected and undone.

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::decide::live::{
    LiveThresholds, ADMIT_HOLD_BOUNDS, KEY_ADMIT_HOLD_BELOW, KEY_MERGE_AUTO_SINGLE,
    MERGE_AUTO_SINGLE_BOUNDS,
};
use crate::error::ApiError;
use crate::tune::Bounds;
use crate::AppState;

/// Audit rows returned by GET /tuning.
const HISTORY_LIMIT: i64 = 50;

fn threshold_json(key: &str, value: f32, default: f32, bounds: Bounds, tuned: bool) -> Value {
    json!({
        "key": key,
        "value": value,
        "default": default,
        "tuned": tuned,
        "bounds": { "min": bounds.min, "max": bounds.max },
    })
}

/// GET /tuning — live thresholds, their defaults and bounds, and recent changes.
pub async fn get_tuning(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = &state.config;
    let defaults = LiveThresholds::from_config(config);
    let live = LiveThresholds::load(&state.pool, config).await?;
    let tuned_keys: Vec<String> = sqlx::query_scalar("SELECT key FROM decision_tuning")
        .fetch_all(&state.pool)
        .await?;
    let is_tuned = |key: &str| tuned_keys.iter().any(|k| k == key);

    let history: Vec<Value> = sqlx::query(
        "SELECT key, old_value, new_value, actor, reason, created_at \
         FROM decision_tuning_audit ORDER BY created_at DESC LIMIT $1",
    )
    .bind(HISTORY_LIMIT)
    .fetch_all(&state.pool)
    .await?
    .iter()
    .map(|r| {
        json!({
            "key": r.get::<String, _>("key"),
            "old_value": r.get::<Option<f64>, _>("old_value"),
            "new_value": r.get::<Option<f64>, _>("new_value"),
            "actor": r.get::<String, _>("actor"),
            "reason": r.get::<Value, _>("reason"),
            "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        })
    })
    .collect();

    Ok(Json(json!({
        "enabled": config.tune_enabled,
        "target_precision": config.tune_target_precision,
        "min_samples": config.tune_min_samples,
        "thresholds": [
            threshold_json(
                KEY_ADMIT_HOLD_BELOW,
                live.admit_hold_below,
                defaults.admit_hold_below,
                ADMIT_HOLD_BOUNDS,
                is_tuned(KEY_ADMIT_HOLD_BELOW),
            ),
            threshold_json(
                KEY_MERGE_AUTO_SINGLE,
                live.merge.auto_single,
                defaults.merge.auto_single,
                MERGE_AUTO_SINGLE_BOUNDS,
                is_tuned(KEY_MERGE_AUTO_SINGLE),
            ),
        ],
        "history": history,
    })))
}

#[derive(Deserialize, Default)]
pub struct ResetRequest {
    /// One key to reset; all tuned keys when absent.
    pub key: Option<String>,
}

/// POST /tuning/reset — drop learned values so the env defaults apply again.
/// Each reset is audited like any other change.
pub async fn reset_tuning(
    State(state): State<AppState>,
    body: Option<Json<ResetRequest>>,
) -> Result<Json<Value>, ApiError> {
    let key = body.and_then(|b| b.0.key);
    if let Some(k) = &key {
        if k != KEY_ADMIT_HOLD_BELOW && k != KEY_MERGE_AUTO_SINGLE {
            return Err(ApiError::BadRequest(format!("unknown tuning key '{k}'")));
        }
    }

    let mut tx = state.pool.begin().await?;
    let removed: Vec<(String, f64)> = sqlx::query_as(
        "DELETE FROM decision_tuning WHERE $1::text IS NULL OR key = $1 RETURNING key, value",
    )
    .bind(&key)
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

    let keys: Vec<&String> = removed.iter().map(|(k, _)| k).collect();
    Ok(Json(json!({ "reset": keys })))
}
