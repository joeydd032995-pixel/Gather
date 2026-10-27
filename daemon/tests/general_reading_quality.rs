//! Quality of the no-model reader (daemon/src/extract/general.rs) on ordinary
//! notes and documents, scored the way `extraction_quality.rs` scores the
//! rules: against the sentences a person would pick out of each passage.
//!
//! The rules alone find a few percent of these (they only take a handful of
//! fixed sentence shapes); the reader is for the rest. The floors sit below
//! what it scores, so a change that costs it real coverage or lets small talk
//! through fails here.

use std::collections::HashSet;

use gather_daemon::extract::{general, rules};
use serde::Deserialize;

#[derive(Deserialize)]
struct Set {
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    name: String,
    input: String,
    expected: Vec<Expected>,
}
#[derive(Deserialize)]
struct Expected {
    statement: String,
}

fn words(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// The unit says what was expected, allowing for the bits of wording a
/// sentence carries: three quarters of the shorter one's words are in the other.
fn same(a: &str, b: &str) -> bool {
    let (x, y) = (words(a), words(b));
    !x.is_empty()
        && !y.is_empty()
        && x.intersection(&y).count() as f64 / x.len().min(y.len()) as f64 >= 0.75
}

fn read(text: &str) -> Vec<String> {
    let mut units = rules::extract_units(text);
    let taken: Vec<(usize, usize)> = units.iter().map(|u| (u.char_start, u.char_end)).collect();
    units.extend(general::extract_units(text, &taken));
    units.into_iter().map(|u| u.statement).collect()
}

#[test]
fn ordinary_notes_are_read_with_good_coverage_and_few_false_statements() {
    let set: Set =
        serde_json::from_str(include_str!("fixtures/ordinary_notes.json")).expect("fixture");
    let (mut expected, mut found, mut produced, mut right) = (0usize, 0usize, 0usize, 0usize);
    let mut chatter_with_output = Vec::new();
    for case in &set.cases {
        let statements = read(&case.input);
        if case.expected.is_empty() && !statements.is_empty() {
            chatter_with_output.push(format!("{}: {statements:?}", case.name));
        }
        expected += case.expected.len();
        produced += statements.len();
        found += case
            .expected
            .iter()
            .filter(|e| statements.iter().any(|s| same(s, &e.statement)))
            .count();
        right += statements
            .iter()
            .filter(|s| case.expected.iter().any(|e| same(s, &e.statement)))
            .count();
    }
    let recall = found as f64 / expected as f64;
    let precision = right as f64 / produced as f64;
    eprintln!(
        "ordinary notes: recall {found}/{expected} = {:.0}%, precision {right}/{produced} = {:.0}%",
        recall * 100.0,
        precision * 100.0
    );
    assert!(recall >= 0.78, "recall {recall:.2}");
    assert!(precision >= 0.92, "precision {precision:.2}");
    // Passages with nothing to find stay empty, bar the odd borderline
    // sentence (a plan with a figure in it).
    assert!(
        chatter_with_output.len() <= 1,
        "small talk, slogans or boilerplate kept: {chatter_with_output:#?}"
    );
}

#[test]
fn the_rules_alone_find_little_of_it() {
    // The reason the reader exists: if the rules ever cover this, it isn't needed.
    let set: Set =
        serde_json::from_str(include_str!("fixtures/ordinary_notes.json")).expect("fixture");
    let (mut expected, mut found) = (0usize, 0usize);
    for case in &set.cases {
        let statements: Vec<String> = rules::extract_units(&case.input)
            .into_iter()
            .map(|u| u.statement)
            .collect();
        expected += case.expected.len();
        found += case
            .expected
            .iter()
            .filter(|e| statements.iter().any(|s| same(s, &e.statement)))
            .count();
    }
    assert!(
        (found as f64) < 0.15 * expected as f64,
        "the rules found {found}/{expected}"
    );
}
