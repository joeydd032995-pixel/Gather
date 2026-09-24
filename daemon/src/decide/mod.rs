//! Auto-act decision policy (autonomous pipeline, Phase A).
//!
//! Pure functions from a score to a [`Band`] — no I/O, so every rule is
//! unit-testable in isolation, the same shape as `scan::score`. The worker and
//! ingest paths gather the inputs and act on the band:
//!   - [`Band::Auto`] — apply silently (admit the unit / perform the merge).
//!   - [`Band::Hold`] — park in `review_queue` for OPTIONAL review; the item is
//!     still live, parking only flags it.
//!   - [`Band::Drop`] — do not act. For admission that means retract; for a
//!     merge candidate it means "not even worth surfacing".
//!
//! The defaults are deliberately **conservative**: nothing is auto-merged
//! unless two independent signals agree or a single signal is nearly certain,
//! and nothing is dropped on ingest unless explicitly configured. Every Auto is
//! reversible and audited by the caller, so the cost of holding one item too
//! long is a tray entry, while the cost of a wrong Auto is a correction — we
//! bias toward the former.

/// What to do with a scored item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Auto,
    Hold,
    Drop,
}

/// Admission band for a freshly extracted unit, from its final confidence
/// (post source-context adjustment in `extract::persist`).
///
/// `drop_below` defaults to 0.0 (never drop user data) and `hold_below` to 0.5.
/// A unit at or above `hold_below` is admitted silently; the thin band below it
/// is admitted too but parked for optional review; only a unit below
/// `drop_below` — off by default — is retracted.
pub fn admit_unit(confidence: f32, hold_below: f32, drop_below: f32) -> Band {
    if confidence < drop_below {
        Band::Drop
    } else if confidence < hold_below {
        Band::Hold
    } else {
        Band::Auto
    }
}

/// The similarity signals available for a duplicate-entity candidate. Either
/// may be absent: cosine needs Ollama embeddings (opt-in), text is always
/// available. On a stock offline install only `text` is present.
#[derive(Debug, Clone, Copy, Default)]
pub struct MergeSignals {
    pub cosine: Option<f32>,
    pub text: Option<f32>,
}

/// Thresholds for [`merge_decision`]. See [`MergeThresholds::conservative`].
#[derive(Debug, Clone, Copy)]
pub struct MergeThresholds {
    /// A single signal at or above this auto-merges on its own (near-certain).
    pub auto_single: f32,
    /// When BOTH signals are present, each must reach its bar to auto-merge.
    pub agree_cosine: f32,
    pub agree_text: f32,
    /// At or above this (but not Auto) a candidate is held for review; below it
    /// the pair is not surfaced at all.
    pub review_floor: f32,
}

impl MergeThresholds {
    /// The shipped defaults. `auto_single` is high enough that the text-only
    /// regime (trigram/prefix sim) auto-merges only near-exact names — e.g.
    /// "postgres"/"postgresql" scores 0.80 and is therefore HELD, not merged,
    /// without a corroborating embedding. With both signals present, agreement
    /// at 0.80 each is enough.
    pub const fn conservative() -> Self {
        Self {
            auto_single: 0.92,
            agree_cosine: 0.80,
            agree_text: 0.80,
            review_floor: 0.60,
        }
    }
}

impl Default for MergeThresholds {
    fn default() -> Self {
        Self::conservative()
    }
}

/// Decide what to do with a duplicate-entity candidate under the conservative
/// gate: Auto only on agreement or a near-certain single signal.
pub fn merge_decision(signals: &MergeSignals, t: &MergeThresholds) -> Band {
    let agree = matches!(
        (signals.cosine, signals.text),
        (Some(c), Some(x)) if c >= t.agree_cosine && x >= t.agree_text
    );
    let strong_single = signals.cosine.is_some_and(|c| c >= t.auto_single)
        || signals.text.is_some_and(|x| x >= t.auto_single);
    if agree || strong_single {
        return Band::Auto;
    }
    match best_score(signals) {
        Some(best) if best >= t.review_floor => Band::Hold,
        _ => Band::Drop,
    }
}

/// The stronger of the two signals, if any is present.
pub fn best_score(signals: &MergeSignals) -> Option<f32> {
    match (signals.cosine, signals.text) {
        (Some(c), Some(x)) => Some(c.max(x)),
        (Some(c), None) => Some(c),
        (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Concordance of two independent similarity signals, in [0,1]: 1.0 when they
/// agree exactly, lower as they diverge. A single signal is neutral (0.5) —
/// there is nothing to corroborate it. Feeds confidence and gates Auto.
pub fn signal_agreement(cosine: Option<f32>, text: Option<f32>) -> f32 {
    match (cosine, text) {
        (Some(c), Some(x)) => (1.0 - (c - x).abs()).clamp(0.0, 1.0),
        _ => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admit_defaults_never_drop_but_hold_low_confidence() {
        // hold_below 0.5, drop_below 0.0 (shipped defaults)
        assert_eq!(admit_unit(0.6, 0.5, 0.0), Band::Auto);
        assert_eq!(admit_unit(0.4, 0.5, 0.0), Band::Hold);
        // 0.0 is not < drop_below (0.0) so it is never dropped, but it is
        // < hold_below (0.5) so it is admitted-and-parked, not silent-auto.
        assert_eq!(admit_unit(0.0, 0.5, 0.0), Band::Hold);
    }

    #[test]
    fn admit_drops_only_when_a_floor_is_set() {
        assert_eq!(admit_unit(0.1, 0.5, 0.2), Band::Drop);
        assert_eq!(admit_unit(0.3, 0.5, 0.2), Band::Hold);
    }

    #[test]
    fn merge_text_only_holds_the_postgres_case_rather_than_merging() {
        let t = MergeThresholds::conservative();
        // "postgres"/"postgresql" ~ 0.80 text, no embedding -> Hold, not Auto.
        let s = MergeSignals {
            cosine: None,
            text: Some(0.80),
        };
        assert_eq!(merge_decision(&s, &t), Band::Hold);
    }

    #[test]
    fn merge_autos_on_near_certain_single_signal() {
        let t = MergeThresholds::conservative();
        let s = MergeSignals {
            cosine: None,
            text: Some(0.95),
        };
        assert_eq!(merge_decision(&s, &t), Band::Auto);
    }

    #[test]
    fn merge_autos_on_agreement_but_not_on_one_high_one_low() {
        let t = MergeThresholds::conservative();
        let agree = MergeSignals {
            cosine: Some(0.85),
            text: Some(0.82),
        };
        assert_eq!(merge_decision(&agree, &t), Band::Auto);

        // Cosine screams duplicate, text disagrees -> not auto; held.
        let disagree = MergeSignals {
            cosine: Some(0.85),
            text: Some(0.62),
        };
        assert_eq!(merge_decision(&disagree, &t), Band::Hold);
    }

    #[test]
    fn merge_drops_below_review_floor() {
        let t = MergeThresholds::conservative();
        let s = MergeSignals {
            cosine: None,
            text: Some(0.55),
        };
        assert_eq!(merge_decision(&s, &t), Band::Drop);
    }

    #[test]
    fn agreement_is_high_when_signals_align_neutral_when_alone() {
        assert!(signal_agreement(Some(0.8), Some(0.8)) > 0.99);
        assert!(signal_agreement(Some(0.9), Some(0.5)) < 0.65);
        assert_eq!(signal_agreement(None, Some(0.8)), 0.5);
    }
}
