//! Duplicate-entity scoring.
//!
//! Follows the same split `scan::score::score_pair` uses for atomic units:
//! pgvector cosine when both embeddings exist, a deterministic text fallback
//! otherwise. That keeps suggestions working on a stock offline install —
//! entity embeddings come from Ollama, which is opt-in (§5.3), and until this
//! release nothing populated `entities.embedding` at all.
//!
//! Entity names are short, so token overlap alone is not enough: "Postgres"
//! and "PostgreSQL" share no whole token. Character trigrams and prefix
//! containment cover that case, while still separating genuinely different
//! names that happen to share a stem ("Java" vs "JavaScript").

use std::collections::HashSet;

use crate::scan::score::{all_tokens, jaccard};

/// Pairs at or above this score are surfaced for review. Chosen so
/// "postgres"/"postgresql" (0.80 via prefix) and "new york"/"new york city"
/// (0.67 via tokens) qualify, while "java"/"javascript" (0.40) does not.
pub const DEFAULT_THRESHOLD: f32 = 0.6;

/// Lowercase, collapse internal whitespace, trim. Applied before every signal
/// so they all see the same string.
fn normalize_name(name: &str) -> String {
    name.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Character trigrams of a space-padded string, the same shape pg_trgm uses.
/// Padding makes the first and last characters count, so "abc"/"xbc" score
/// lower than their raw-trigram overlap alone would suggest.
fn trigrams(s: &str) -> HashSet<String> {
    let padded: Vec<char> = format!("  {s} ").chars().collect();
    if padded.len() < 3 {
        return HashSet::new();
    }
    padded.windows(3).map(|w| w.iter().collect()).collect()
}

fn trigram_jaccard(a: &str, b: &str) -> f32 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.intersection(&tb).count() as f32;
    let union = ta.union(&tb).count() as f32;
    intersection / union
}

/// Length ratio when one name is a prefix of the other, else 0. Catches the
/// abbreviation case ("postgres" ⊂ "postgresql" → 0.80) without rewarding a
/// one-character prefix of a long name ("a" ⊂ "abcdefgh" → 0.13).
fn prefix_ratio(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.starts_with(short) {
        short.chars().count() as f32 / long.chars().count() as f32
    } else {
        0.0
    }
}

/// Text-only similarity of two entity names, in [0,1]. Used when embeddings
/// are unavailable, which on a stock install is always.
pub fn name_similarity(a: &str, b: &str) -> f32 {
    let (na, nb) = (normalize_name(a), normalize_name(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    // Each signal catches a different duplicate shape; the strongest wins
    // rather than being averaged away by the ones that do not apply.
    let token = jaccard(&all_tokens(&na), &all_tokens(&nb));
    let trigram = trigram_jaccard(&na, &nb);
    token.max(trigram).max(prefix_ratio(&na, &nb))
}

/// Score a candidate pair. `cosine_sim` is the pgvector cosine similarity when
/// both entities have embeddings; it supersedes the text score, mirroring
/// `scan::score::score_pair`'s treatment of unit similarity.
pub fn pair_score(a_name: &str, b_name: &str, cosine_sim: Option<f32>) -> f32 {
    cosine_sim.unwrap_or_else(|| name_similarity(a_name, b_name))
}

/// Which signal produced a score, for display in the review UI.
pub fn pair_method(cosine_sim: Option<f32>) -> &'static str {
    if cosine_sim.is_some() {
        "embedding:cosine"
    } else {
        "rule:name-similarity"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_names_score_one() {
        assert_eq!(name_similarity("Postgres", "postgres"), 1.0);
        assert_eq!(name_similarity("New  York", "new york"), 1.0);
    }

    #[test]
    fn abbreviation_prefix_scores_above_threshold() {
        // The motivating case: these are one node in reality, but token
        // overlap alone is 0.0 because neither name shares a whole token.
        let score = name_similarity("Postgres", "PostgreSQL");
        assert!(score >= DEFAULT_THRESHOLD, "postgres/postgresql = {score}");
    }

    #[test]
    fn shared_token_scores_above_threshold() {
        let score = name_similarity("New York", "New York City");
        assert!(score >= DEFAULT_THRESHOLD, "new york = {score}");
    }

    #[test]
    fn shared_stem_but_different_thing_stays_below_threshold() {
        // Guards against the obvious false positive of a prefix-only rule.
        let score = name_similarity("Java", "JavaScript");
        assert!(score < DEFAULT_THRESHOLD, "java/javascript = {score}");
    }

    #[test]
    fn unrelated_names_score_low() {
        assert!(name_similarity("Postgres", "Kubernetes") < DEFAULT_THRESHOLD);
        assert!(name_similarity("Alice", "Bob") < DEFAULT_THRESHOLD);
    }

    #[test]
    fn short_prefix_of_long_name_is_not_a_match() {
        assert!(name_similarity("a", "abcdefgh") < DEFAULT_THRESHOLD);
    }

    #[test]
    fn empty_names_never_match() {
        assert_eq!(name_similarity("", "anything"), 0.0);
        assert_eq!(name_similarity("", ""), 0.0);
    }

    #[test]
    fn cosine_supersedes_text_score() {
        // Text says "unrelated", the embedding says "same" — embedding wins,
        // matching how scan::score::score_pair treats cosine_sim.
        assert_eq!(pair_score("Postgres", "Kubernetes", Some(0.95)), 0.95);
        assert_eq!(pair_method(Some(0.95)), "embedding:cosine");
        assert_eq!(pair_method(None), "rule:name-similarity");
    }
}
