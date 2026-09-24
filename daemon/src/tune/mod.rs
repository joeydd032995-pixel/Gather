//! Active learning + threshold auto-tuning (autonomous pipeline, Phase C).
//!
//! Pure functions, no I/O (same shape as `decide` and `cluster`); the I/O lives
//! in [`worker`]. Two jobs:
//!
//! 1. **Tray ordering.** [`info_gain`] ranks a held item by how close it sits to
//!    the Auto boundary times how much of the graph it touches, so the few
//!    answers a user gives settle the most cases. The *upper* edge of the hold
//!    band is the one that matters: an answer there decides whether the Auto
//!    bar can come down, which is what shrinks the tray.
//!
//! 2. **Threshold tuning.** [`propose_threshold`] moves one threshold from the
//!    user's verdicts. Labels are biased: users mostly reject the wrong things
//!    they happen to notice among auto-accepted items, and confirm from the
//!    tray. That makes measured Auto precision read LOW, which pushes toward
//!    raising — the safe direction. The rule is asymmetric to match:
//!    raise on a point estimate, lower only when a 95% Wilson lower bound
//!    clears the target, each move capped at `max_step` and clamped to hard
//!    bounds. Hysteresis between the two rules prevents oscillation.

pub mod worker;

/// A user's verdict on one item and the score the item carried when judged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Label {
    pub score: f32,
    pub keep: bool,
}

/// Hard limits a tuned threshold can never leave, whatever the evidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: f32,
    pub max: f32,
}

/// Tuning policy knobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TuneParams {
    /// Precision the Auto band must hold (e.g. 0.90).
    pub target_precision: f64,
    /// Labels at/above a threshold needed before it may move at all.
    pub min_samples: usize,
    /// Largest change per tuning run.
    pub max_step: f32,
}

/// Which rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Raise,
    Lower,
    Stay,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Raise => "raise",
            Direction::Lower => "lower",
            Direction::Stay => "stay",
        }
    }
}

/// The tuner's decision plus the evidence behind it (written to the audit log).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Proposal {
    pub value: f32,
    pub direction: Direction,
    /// Point precision of the labels at/above the CURRENT threshold.
    pub precision: Option<f64>,
    /// Number of labels at/above the current threshold.
    pub support: usize,
}

/// 95% two-sided z.
const Z_95: f64 = 1.96;
/// Moves smaller than this are treated as no move (float noise).
const EPSILON: f32 = 1e-4;

/// Wilson score lower bound for a binomial proportion `keeps / n`. Unlike the
/// raw ratio it is pessimistic on small samples: 3/3 kept is ~0.44, not 1.0,
/// so a handful of confirms can never justify loosening a threshold.
pub fn wilson_lower_bound(keeps: usize, n: usize, z: f64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let n = n as f64;
    let p = keeps as f64 / n;
    let z2 = z * z;
    let centre = p + z2 / (2.0 * n);
    let margin = z * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt();
    ((centre - margin) / (1.0 + z2 / n)).clamp(0.0, 1.0)
}

/// How close `score` sits to the Auto boundary `hi`, within the hold band
/// `[lo, hi]`: 1.0 right at the boundary, 0.0 at the floor. A degenerate band
/// (hi <= lo) is maximally uncertain.
pub fn boundary_uncertainty(score: f32, lo: f32, hi: f32) -> f32 {
    if !score.is_finite() {
        return 0.0;
    }
    if hi <= lo {
        return 1.0;
    }
    ((score - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// Tray priority: uncertainty × impact, where impact grows with the number of
/// graph facts the answer would affect (log-damped so a hub doesn't swamp the
/// tray). Always finite and non-negative.
pub fn info_gain(uncertainty: f32, degree: i64) -> f32 {
    let u = if uncertainty.is_finite() {
        uncertainty.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let impact = 1.0 + (1.0 + degree.max(0) as f32).ln();
    u * impact
}

fn keeps_and_total<'a>(labels: impl Iterator<Item = &'a Label>) -> (usize, usize) {
    labels.fold((0, 0), |(k, n), l| (k + l.keep as usize, n + 1))
}

/// Propose a new value for one threshold from the user's verdicts.
///
/// - **Raise** (`+max_step`) when there are at least `min_samples` labels
///   at/above the current threshold and their precision is below target.
/// - **Lower** to the smallest labelled score `t` in `[current - max_step,
///   current)` such that: the labels at/above `t` number at least
///   `min_samples` with a Wilson lower bound ≥ target, AND the newly-admitted
///   region `[t, current)` has its own evidence (≥ `min_region` labels, point
///   precision ≥ target). Only a labelled score can be a candidate, so the
///   threshold never moves into a region nobody has judged.
/// - Otherwise **stay**.
///
/// The result is always within `bounds`.
pub fn propose_threshold(
    labels: &[Label],
    current: f32,
    bounds: Bounds,
    params: &TuneParams,
) -> Proposal {
    let current = current.clamp(bounds.min, bounds.max);
    let labels: Vec<Label> = labels
        .iter()
        .copied()
        .filter(|l| l.score.is_finite())
        .collect();

    let (keeps, support) = keeps_and_total(labels.iter().filter(|l| l.score >= current));
    let precision = (support > 0).then(|| keeps as f64 / support as f64);
    let stay = Proposal {
        value: current,
        direction: Direction::Stay,
        precision,
        support,
    };

    if let Some(p) = precision {
        if support >= params.min_samples && p < params.target_precision {
            let value = (current + params.max_step).min(bounds.max);
            if value - current > EPSILON {
                return Proposal {
                    value,
                    direction: Direction::Raise,
                    ..stay
                };
            }
            return stay;
        }
    }

    let floor = (current - params.max_step).max(bounds.min);
    let min_region = (params.min_samples / 4).max(3);
    let mut candidates: Vec<f32> = labels
        .iter()
        .map(|l| l.score)
        .filter(|&s| s >= floor && s < current)
        .collect();
    candidates.sort_by(f32::total_cmp);
    candidates.dedup_by(|a, b| (*a - *b).abs() < EPSILON);

    for t in candidates {
        let (keeps_t, n_t) = keeps_and_total(labels.iter().filter(|l| l.score >= t));
        let (keeps_r, n_r) =
            keeps_and_total(labels.iter().filter(|l| l.score >= t && l.score < current));
        let region_ok =
            n_r >= min_region && (keeps_r as f64 / n_r as f64) >= params.target_precision;
        let overall_ok = n_t >= params.min_samples
            && wilson_lower_bound(keeps_t, n_t, Z_95) >= params.target_precision;
        if region_ok && overall_ok && current - t > EPSILON {
            return Proposal {
                value: t,
                direction: Direction::Lower,
                ..stay
            };
        }
    }
    stay
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: TuneParams = TuneParams {
        target_precision: 0.90,
        min_samples: 20,
        max_step: 0.05,
    };
    const BOUNDS: Bounds = Bounds { min: 0.3, max: 0.9 };

    fn labels(score: f32, keeps: usize, rejects: usize) -> Vec<Label> {
        let mut out = vec![Label { score, keep: true }; keeps];
        out.extend(vec![Label { score, keep: false }; rejects]);
        out
    }

    #[test]
    fn wilson_is_pessimistic_on_small_samples() {
        assert!(wilson_lower_bound(3, 3, Z_95) < 0.5);
        assert!(wilson_lower_bound(100, 100, Z_95) > 0.95);
        assert_eq!(wilson_lower_bound(0, 0, Z_95), 0.0);
    }

    #[test]
    fn uncertainty_peaks_at_the_auto_boundary() {
        assert_eq!(boundary_uncertainty(0.5, 0.0, 0.5), 1.0);
        assert_eq!(boundary_uncertainty(0.0, 0.0, 0.5), 0.0);
        assert!((boundary_uncertainty(0.25, 0.0, 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(boundary_uncertainty(f32::NAN, 0.0, 0.5), 0.0);
    }

    #[test]
    fn info_gain_prefers_uncertain_high_impact_items() {
        assert!(info_gain(0.9, 10) > info_gain(0.9, 0));
        assert!(info_gain(0.9, 0) > info_gain(0.2, 0));
        assert_eq!(info_gain(0.0, 1000), 0.0);
        assert!(info_gain(f32::NAN, 5).is_finite());
    }

    #[test]
    fn raises_when_auto_band_precision_is_below_target() {
        let l = labels(0.6, 15, 10);
        let p = propose_threshold(&l, 0.5, BOUNDS, &PARAMS);
        assert_eq!(p.direction, Direction::Raise);
        assert!((p.value - 0.55).abs() < 1e-6);
    }

    #[test]
    fn stays_put_without_enough_samples() {
        let l = labels(0.6, 2, 8);
        let p = propose_threshold(&l, 0.5, BOUNDS, &PARAMS);
        assert_eq!(p.direction, Direction::Stay);
        assert_eq!(p.value, 0.5);
    }

    #[test]
    fn lowers_only_with_strong_evidence_in_the_new_region() {
        let mut l = labels(0.6, 60, 0);
        l.extend(labels(0.46, 10, 0));
        let p = propose_threshold(&l, 0.5, BOUNDS, &PARAMS);
        assert_eq!(p.direction, Direction::Lower);
        assert!((p.value - 0.46).abs() < 1e-6);
    }

    #[test]
    fn never_lowers_into_an_unlabelled_or_bad_region() {
        let mut clean = labels(0.6, 60, 0);
        assert_eq!(
            propose_threshold(&clean, 0.5, BOUNDS, &PARAMS).direction,
            Direction::Stay
        );
        clean.extend(labels(0.46, 5, 5));
        assert_eq!(
            propose_threshold(&clean, 0.5, BOUNDS, &PARAMS).direction,
            Direction::Stay
        );
    }

    #[test]
    fn respects_hard_bounds() {
        let l = labels(0.95, 0, 40);
        let p = propose_threshold(&l, 0.9, BOUNDS, &PARAMS);
        assert!(p.value <= BOUNDS.max);
        let l = labels(0.31, 100, 0);
        let p = propose_threshold(&l, 0.3, BOUNDS, &PARAMS);
        assert!(p.value >= BOUNDS.min);
    }
}
