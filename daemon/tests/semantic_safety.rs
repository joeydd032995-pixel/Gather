//! The semantic-safety fixture corpus as a test: every scenario passes, no
//! invariant fails, and the outcome digest is deterministic. Offline.

use std::path::PathBuf;

use gather_daemon::safety::eval::{self, load_dir, run};

fn corpus() -> Vec<eval::Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/semantic");
    let c = load_dir(&dir).expect("fixtures load");
    assert!(c.len() >= 10, "the corpus covers every failure family");
    c
}

#[test]
fn corpus_passes_with_no_invariant_failures() {
    let report = run(&corpus());
    assert!(report.ok(), "{}", eval::render(&report));
    assert_eq!(report.false_auto_actions, 0);
    assert_eq!(report.missing_certificates, 0);
    assert_eq!(report.retraction_propagation_failures, 0);
    // Every failure family is exercised.
    let categories: std::collections::BTreeSet<&str> = report
        .scenarios
        .iter()
        .map(|s| s.category.as_str())
        .collect();
    for c in [
        "identity",
        "photo",
        "contradiction",
        "temporal",
        "provenance",
        "modality",
        "retraction",
        "drift",
        "ingestion_order",
    ] {
        assert!(categories.contains(c), "no scenario for {c}");
    }
    for code in [
        "CHAINED_SIMILARITY",
        "PAIRWISE_EVIDENCE_GAP",
        "ENTITY_TYPE_MISMATCH",
        "CONTEXT_SCOPE_MISMATCH",
        "TIME_SCOPE_UNKNOWN",
        "TIME_WINDOWS_NON_OVERLAPPING",
        "SOURCE_NOT_INDEPENDENT",
        "DERIVED_SOURCE_DUPLICATION",
        "MODALITY_MISMATCH",
        "UNIT_NORMALIZATION_REQUIRED",
        "GRANULARITY_MISMATCH",
        "MODEL_DISAGREEMENT",
        "USER_REJECTION_EXISTS",
        "HUB_DEGREE_EXCEEDED",
        "GENERIC_IDENTIFIER",
    ] {
        assert!(
            report.blocked_by_reason.contains_key(code)
                || report.review_by_reason.contains_key(code),
            "reason {code} never exercised"
        );
    }
}

#[test]
fn digest_is_deterministic_and_order_independent() {
    let mut c = corpus();
    let first = run(&c);
    c.reverse();
    let second = run(&c);
    assert_eq!(first.digest, second.digest);
    assert_eq!(
        first
            .scenarios
            .iter()
            .map(|s| &s.digest)
            .collect::<Vec<_>>(),
        second
            .scenarios
            .iter()
            .map(|s| &s.digest)
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_wrong_expectation_is_caught() {
    // The harness must be able to fail: flip one expectation and it does.
    let mut c = corpus();
    let fx = c
        .iter_mut()
        .find(|f| f.id == "identity-bridge-chain")
        .expect("bridge fixture");
    fx.expect["merge_groups"] = serde_json::json!([["A", "B", "C"], ["D", "E"]]);
    let report = run(&c);
    assert!(!report.ok());
    assert!(report
        .scenarios
        .iter()
        .any(|s| s.id == "identity-bridge-chain" && !s.passed));
}
