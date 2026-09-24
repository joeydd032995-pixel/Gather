//! Live decision thresholds (autonomous pipeline, Phase C).
//!
//! The auto-tuner writes learned values to `decision_tuning`; the workers read
//! them here once per pass. The environment config stays the default: an
//! absent key, or a stored value that is non-finite or out of its hard bounds
//! (e.g. a hand-edited or imported bundle), falls back to it, so a bad row can
//! never disable the conservative gate.

use sqlx::PgPool;

use super::MergeThresholds;
use crate::config::Config;
use crate::tune::Bounds;

/// `decision_tuning` key for the unit admission hold threshold.
pub const KEY_ADMIT_HOLD_BELOW: &str = "admit.hold_below";
/// `decision_tuning` key for the single-signal auto-merge threshold.
pub const KEY_MERGE_AUTO_SINGLE: &str = "merge.auto_single";

/// Hard limits for the tuned admission threshold.
pub const ADMIT_HOLD_BOUNDS: Bounds = Bounds { min: 0.3, max: 0.9 };
/// Hard limits for the tuned merge threshold. The floor keeps text-only
/// auto-merges near-exact however much evidence accumulates.
pub const MERGE_AUTO_SINGLE_BOUNDS: Bounds = Bounds {
    min: 0.85,
    max: 0.99,
};

/// Thresholds in force for one worker pass.
#[derive(Debug, Clone, Copy)]
pub struct LiveThresholds {
    pub admit_hold_below: f32,
    pub admit_drop_below: f32,
    pub merge: MergeThresholds,
}

impl LiveThresholds {
    /// The env-only values, as if nothing had been tuned.
    pub fn from_config(config: &Config) -> Self {
        Self {
            admit_hold_below: config.admit_hold_below,
            admit_drop_below: config.admit_drop_below,
            merge: MergeThresholds::conservative(),
        }
    }

    /// Env defaults overlaid with any valid tuned values.
    pub async fn load(pool: &PgPool, config: &Config) -> Result<Self, sqlx::Error> {
        let mut live = Self::from_config(config);
        let rows: Vec<(String, f64)> =
            sqlx::query_as("SELECT key, value FROM decision_tuning WHERE key = ANY($1)")
                .bind([KEY_ADMIT_HOLD_BELOW, KEY_MERGE_AUTO_SINGLE])
                .fetch_all(pool)
                .await?;
        for (key, value) in rows {
            let value = value as f32;
            match key.as_str() {
                KEY_ADMIT_HOLD_BELOW if within(value, ADMIT_HOLD_BOUNDS) => {
                    // Never let tuning put the hold bar under the drop floor:
                    // that would erase the hold band.
                    live.admit_hold_below = value.max(live.admit_drop_below);
                }
                KEY_MERGE_AUTO_SINGLE if within(value, MERGE_AUTO_SINGLE_BOUNDS) => {
                    live.merge.auto_single = value;
                }
                _ => tracing::warn!(key, value, "ignoring invalid tuned threshold"),
            }
        }
        Ok(live)
    }
}

fn within(value: f32, bounds: Bounds) -> bool {
    value.is_finite() && value >= bounds.min && value <= bounds.max
}
