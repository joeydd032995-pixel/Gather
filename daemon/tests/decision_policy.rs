//! Decision-policy gate: a deterministic, offline check that the conservative
//! auto-act policy never auto-merges on weak or conflicting evidence and never
//! drops user data by default. No DB, no Ollama — it calls the pure functions
//! in `gather_daemon::decide`, so it rides the ordinary CI `test` job.

use gather_daemon::decide::{admit_unit, merge_decision, Band, MergeSignals, MergeThresholds};

struct MergeCase {
    name: &'static str,
    cosine: Option<f32>,
    text: Option<f32>,
    expect: Band,
}

#[test]
fn conservative_merge_gate_holds_the_line() {
    let t = MergeThresholds::conservative();
    let cases = [
        MergeCase {
            name: "exact name, both signals agree high",
            cosine: Some(0.97),
            text: Some(0.99),
            expect: Band::Auto,
        },
        MergeCase {
            name: "embedding agrees with text at the agreement bar",
            cosine: Some(0.81),
            text: Some(0.80),
            expect: Band::Auto,
        },
        MergeCase {
            name: "near-certain single signal (text only, no Ollama)",
            cosine: None,
            text: Some(0.94),
            expect: Band::Auto,
        },
        MergeCase {
            name: "postgres/postgresql text-only 0.80 -> held, never auto",
            cosine: None,
            text: Some(0.80),
            expect: Band::Hold,
        },
        MergeCase {
            name: "one signal screams, the other disagrees -> held",
            cosine: Some(0.88),
            text: Some(0.61),
            expect: Band::Hold,
        },
        MergeCase {
            name: "weak single signal below review floor -> not surfaced",
            cosine: None,
            text: Some(0.50),
            expect: Band::Drop,
        },
    ];

    let mut failures = Vec::new();
    for c in &cases {
        let got = merge_decision(
            &MergeSignals {
                cosine: c.cosine,
                text: c.text,
            },
            &t,
        );
        if got != c.expect {
            failures.push(format!(
                "  [{}] expected {:?}, got {:?}",
                c.name, c.expect, got
            ));
        }
    }

    // The invariant the gate defends: nothing auto-merges unless two signals
    // agree or a single one is >= auto_single. Assert it independently of the
    // table so a future threshold edit can't quietly weaken it.
    for c in &cases {
        let s = MergeSignals {
            cosine: c.cosine,
            text: c.text,
        };
        if merge_decision(&s, &t) == Band::Auto {
            let agree = matches!((c.cosine, c.text), (Some(x), Some(y)) if x >= t.agree_cosine && y >= t.agree_text);
            let strong = c.cosine.is_some_and(|x| x >= t.auto_single)
                || c.text.is_some_and(|y| y >= t.auto_single);
            assert!(
                agree || strong,
                "[{}] auto-merged without agreement or a near-certain signal",
                c.name
            );
        }
    }

    assert!(
        failures.is_empty(),
        "merge policy drift:\n{}",
        failures.join("\n")
    );
}

#[test]
fn admit_defaults_are_lossless() {
    // Shipped defaults: hold_below 0.5, drop_below 0.0 -> nothing is ever
    // dropped; the low-confidence band is admitted and merely parked.
    for pct in 0..=100 {
        let conf = pct as f32 / 100.0;
        let band = admit_unit(conf, 0.5, 0.0);
        assert_ne!(
            band,
            Band::Drop,
            "confidence {conf} was dropped under defaults"
        );
        if conf >= 0.5 {
            assert_eq!(band, Band::Auto);
        } else {
            assert_eq!(band, Band::Hold);
        }
    }
}
