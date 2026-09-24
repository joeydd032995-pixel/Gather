//! Offline clustering eval: drives the mutual-kNN + connected-components
//! primitive with the REAL entity name-similarity function over a labelled
//! fixture, so a regression in either the graph code or the similarity scorer
//! shows up as a wrong grouping. No DB, no Ollama — rides the CI `test` job.

use gather_daemon::cluster::{components, grouped, mutual_knn};
use gather_daemon::entities::similarity::name_similarity;

/// Names that should group vs. names that must stay apart.
const NAMES: &[&str] = &[
    // 0,1,2 — the same thing spelled three ways (should cluster together).
    "PostgreSQL",
    "Postgres",
    "postgres",
    // 3,4 — a different thing, two spellings (its own cluster).
    "New York",
    "New York City",
    // 5 — unrelated singleton.
    "Tesseract",
];

#[test]
fn name_similarity_groups_duplicates_and_separates_distinct_things() {
    let n = NAMES.len();
    let edges = mutual_knn(n, 4, 0.6, |i, j| name_similarity(NAMES[i], NAMES[j]));
    let groups = grouped(&components(n, &edges));

    // Find the component containing a given index.
    let comp_of = |idx: usize| {
        groups
            .iter()
            .find(|g| g.contains(&idx))
            .expect("every node is in some component")
            .clone()
    };

    // The three Postgres spellings land together...
    assert_eq!(comp_of(0), vec![0, 1, 2]);
    // ...the two New York forms land together, separate from Postgres...
    assert_eq!(comp_of(3), vec![3, 4]);
    // ...and the unrelated name is its own singleton.
    assert_eq!(comp_of(5), vec![5]);

    // Independent invariant: nothing bridges the two real clusters.
    assert!(
        !groups.iter().any(|g| g.contains(&0) && g.contains(&3)),
        "Postgres and New York must never share a component"
    );
}
