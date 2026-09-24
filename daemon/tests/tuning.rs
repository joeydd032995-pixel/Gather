//! Offline eval for active learning + threshold auto-tuning (Phase C).
//!
//! Deterministic, no DB: synthetic label populations with a KNOWN quality
//! boundary, run through the tuner pass after pass. Guards the properties that
//! make auto-tuning safe to leave on: it converges, it never leaves its bounds,
//! it never moves on thin evidence, it never oscillates, and biased
//! reject-only feedback can only make it more cautious.

use gather_daemon::tune::{
    info_gain, propose_threshold, Bounds, Direction, Label, Proposal, TuneParams,
};

const PARAMS: TuneParams = TuneParams {
    target_precision: 0.90,
    min_samples: 20,
    max_step: 0.05,
};
const BOUNDS: Bounds = Bounds { min: 0.3, max: 0.9 };

/// `per_score` labels at each score in [0.30, 0.90] (step 0.02). Above
/// `boundary` 19 of every 20 are keeps (95%); below it, half are.
fn population(boundary: f32, per_score: usize) -> Vec<Label> {
    let mut out = Vec::new();
    for step in 0..=30 {
        let score = 0.30 + step as f32 * 0.02;
        for i in 0..per_score {
            let keep = if score >= boundary {
                i % 20 != 0
            } else {
                i % 2 == 0
            };
            out.push(Label { score, keep });
        }
    }
    out
}

/// Run the tuner `passes` times from `start`, returning every proposal.
fn run(labels: &[Label], start: f32, passes: usize) -> Vec<Proposal> {
    let mut current = start;
    (0..passes)
        .map(|_| {
            let p = propose_threshold(labels, current, BOUNDS, &PARAMS);
            current = p.value;
            p
        })
        .collect()
}

fn precision_at(labels: &[Label], t: f32) -> f64 {
    let above: Vec<&Label> = labels.iter().filter(|l| l.score >= t).collect();
    above.iter().filter(|l| l.keep).count() as f64 / above.len() as f64
}

#[test]
fn a_too_loose_threshold_climbs_until_the_auto_band_meets_target() {
    let labels = population(0.6, 20);
    let trace = run(&labels, 0.4, 40);
    let last = trace.last().unwrap().value;
    assert!(
        (0.5..=0.65).contains(&last),
        "converged to {last}, expected near the 0.6 quality boundary"
    );
    assert!(precision_at(&labels, last) >= 0.88);
    // Settled: the final ten passes don't move.
    assert!(trace[30..].iter().all(|p| p.direction == Direction::Stay));
}

#[test]
fn a_too_strict_threshold_comes_down_only_as_far_as_evidence_supports() {
    // Everything labelled from 0.30 up is good: the tuner may loosen, one
    // bounded step at a time, but never below the lowest labelled score.
    let labels: Vec<Label> = population(0.0, 20);
    let trace = run(&labels, 0.8, 60);
    let mut prev = 0.8f32;
    for p in &trace {
        assert!(prev - p.value <= PARAMS.max_step + 1e-6, "step too large");
        prev = p.value;
    }
    let last = trace.last().unwrap().value;
    assert!((BOUNDS.min..0.8).contains(&last), "ended at {last}");
}

#[test]
fn never_oscillates_on_stable_feedback() {
    for (boundary, start) in [(0.6, 0.4), (0.6, 0.85), (0.45, 0.7), (0.75, 0.3)] {
        let labels = population(boundary, 20);
        let trace = run(&labels, start, 80);
        let moves: Vec<Direction> = trace
            .iter()
            .map(|p| p.direction)
            .filter(|d| *d != Direction::Stay)
            .collect();
        assert!(
            moves.windows(2).all(|w| w[0] == w[1]),
            "direction reversed for boundary {boundary} from {start}: {moves:?}"
        );
    }
}

#[test]
fn never_leaves_hard_bounds() {
    let all_bad: Vec<Label> = (0..200)
        .map(|i| Label {
            score: 0.3 + (i % 60) as f32 * 0.01,
            keep: false,
        })
        .collect();
    let trace = run(&all_bad, 0.5, 50);
    assert!(trace
        .iter()
        .all(|p| p.value >= BOUNDS.min && p.value <= BOUNDS.max));
    assert!(
        trace.last().unwrap().value > 0.5,
        "should tighten on all-bad labels"
    );

    // Plenty of bad labels right at the ceiling: clamped, never past it.
    let at_ceiling: Vec<Label> = (0..40)
        .map(|_| Label {
            score: 0.95,
            keep: false,
        })
        .collect();
    let p = propose_threshold(&at_ceiling, 0.88, BOUNDS, &PARAMS);
    assert_eq!(p.value, BOUNDS.max);
}

#[test]
fn thin_evidence_never_moves_a_threshold() {
    let few: Vec<Label> = population(0.6, 20).into_iter().step_by(40).collect();
    assert!(few.len() < PARAMS.min_samples);
    let trace = run(&few, 0.5, 20);
    assert!(trace.iter().all(|p| p.direction == Direction::Stay));
}

#[test]
fn biased_reject_only_feedback_can_only_tighten() {
    // Users mostly reject the wrong things they notice among auto-accepted
    // items. Feedback like that must never loosen a threshold.
    let rejects: Vec<Label> = (0..100)
        .map(|i| Label {
            score: 0.3 + (i % 60) as f32 * 0.01,
            keep: false,
        })
        .collect();
    let trace = run(&rejects, 0.5, 30);
    assert!(trace.iter().all(|p| p.direction != Direction::Lower));
}

#[test]
fn tray_ranks_boundary_hubs_first() {
    // (label, uncertainty, degree)
    let items = [
        ("far-from-boundary leaf", 0.1, 0),
        ("at-boundary leaf", 1.0, 0),
        ("at-boundary hub", 1.0, 40),
        ("mid-band hub", 0.5, 40),
        ("far-from-boundary hub", 0.1, 40),
    ];
    let mut ranked: Vec<(&str, f32)> = items
        .iter()
        .map(|(name, u, d)| (*name, info_gain(*u, *d)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let order: Vec<&str> = ranked.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        order,
        [
            "at-boundary hub",
            "mid-band hub",
            "at-boundary leaf",
            "far-from-boundary hub",
            "far-from-boundary leaf",
        ]
    );
}
