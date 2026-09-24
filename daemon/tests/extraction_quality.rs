//! Extraction-quality gate: a deterministic, offline evaluation of the
//! always-on rule-based extractor against a hand-labelled golden corpus
//! (tests/fixtures/extraction_golden.json).
//!
//! This is the automatable half of the write-up's Phase-1 "Go" gate
//! ("≥70% of sampled units judged usable"). A human still owns judging real
//! ingested data — scripts/unit-quality-sample.sh — but this makes the
//! precision of the rule extractor a repeatable, CI-enforced number so quality
//! can't silently regress. It calls the pure extract_units() function, so it
//! needs no database and no Ollama.

use gather_daemon::extract::rules::extract_units;
use serde::Deserialize;

#[derive(Deserialize)]
struct Golden {
    threshold_precision: f64,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    input: String,
    expected: Vec<ExpectedUnit>,
}

#[derive(Deserialize, Clone)]
struct ExpectedUnit {
    kind: String,
    statement: String,
    #[allow(dead_code)]
    subject: Option<String>,
}

/// Case-insensitive, whitespace-collapsed, trailing-punctuation-stripped form
/// so a golden statement only has to agree on wording, not formatting.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches(['.', '!', '?'])
        .to_string()
}

fn load_golden() -> Golden {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/extraction_golden.json"
    );
    let raw = std::fs::read_to_string(path).expect("read golden corpus");
    serde_json::from_str(&raw).expect("parse golden corpus")
}

#[test]
fn rule_extractor_meets_precision_gate() {
    let golden = load_golden();

    let mut produced_total = 0usize; // every unit the extractor emitted
    let mut usable_produced = 0usize; // …that matched a labelled usable unit (true positive)
    let mut expected_total = 0usize; // every labelled usable unit
    let mut matched_expected = 0usize; // …the extractor found (for recall)
    let mut failures: Vec<String> = Vec::new();

    for case in &golden.cases {
        let produced = extract_units(&case.input);

        // Match on (kind, normalized statement); each expected consumed once.
        let mut remaining: Vec<ExpectedUnit> = case.expected.clone();
        expected_total += case.expected.len();
        produced_total += produced.len();

        for unit in &produced {
            let stmt = normalize(&unit.statement);
            if let Some(pos) = remaining
                .iter()
                .position(|e| e.kind == unit.kind && normalize(&e.statement) == stmt)
            {
                usable_produced += 1;
                matched_expected += 1;
                remaining.remove(pos);
            } else {
                // Extractor emitted a unit no reviewer labelled usable: a false
                // positive that (correctly) costs precision.
                failures.push(format!(
                    "  [{}] spurious {} unit: {:?}",
                    case.name, unit.kind, unit.statement
                ));
            }
        }
        for miss in remaining {
            // A usable fact the rules did not catch: costs recall, not precision.
            failures.push(format!(
                "  [{}] missed {} unit: {:?}",
                case.name, miss.kind, miss.statement
            ));
        }
    }

    let precision = if produced_total == 0 {
        1.0
    } else {
        usable_produced as f64 / produced_total as f64
    };
    let recall = if expected_total == 0 {
        1.0
    } else {
        matched_expected as f64 / expected_total as f64
    };

    eprintln!("── extraction quality (rule-based, offline) ──");
    eprintln!("cases:       {}", golden.cases.len());
    eprintln!(
        "produced:    {produced_total}  (usable {usable_produced}, spurious {})",
        produced_total - usable_produced
    );
    eprintln!("labelled:    {expected_total}  (found {matched_expected})");
    eprintln!(
        "precision:   {:.1}%  (gate ≥ {:.0}%)",
        precision * 100.0,
        golden.threshold_precision * 100.0
    );
    eprintln!(
        "recall:      {:.1}%  (informational — rules are high-precision/low-recall)",
        recall * 100.0
    );
    if !failures.is_empty() {
        eprintln!("misses & spurious units:");
        for f in &failures {
            eprintln!("{f}");
        }
    }

    assert!(
        precision >= golden.threshold_precision,
        "extraction precision {:.1}% is below the {:.0}% gate",
        precision * 100.0,
        golden.threshold_precision * 100.0
    );
}
