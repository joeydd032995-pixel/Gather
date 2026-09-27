//! Modality and negation: keep "did", "will", "might", "won't" and "if" apart.
//!
//! A completed event, a plan, a possibility, a rejection, a condition and a
//! mere consideration are different propositions even when they share every
//! content word. Canonicalization keys on (content, modality, polarity), so
//! "not" is never dropped and a hypothetical never becomes a present fact.
//! When the markers conflict, the classification is `ambiguous` and the
//! claim goes to review instead of being read one way silently.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Happened / holds ("I moved to Chicago", "I use Postgres").
    Actual,
    /// Intended ("I plan to move", "I will use").
    Planned,
    /// Might happen ("I might move", "maybe").
    Possible,
    /// Explicitly decided against ("I decided not to move").
    Rejected,
    /// Only under a condition ("If I move, ...").
    Conditional,
    /// Thought about, no commitment ("I considered moving").
    Considered,
}

impl Modality {
    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Actual => "actual",
            Modality::Planned => "planned",
            Modality::Possible => "possible",
            Modality::Rejected => "rejected",
            Modality::Conditional => "conditional",
            Modality::Considered => "considered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalReading {
    pub modality: Modality,
    pub negated: bool,
    /// Conflicting markers: the reading is a best guess and must be reviewed.
    pub ambiguous: bool,
}

impl ModalReading {
    /// Only an actual, non-negated claim may assert a positive graph edge.
    pub fn asserts_positive_fact(&self) -> bool {
        self.modality == Modality::Actual && !self.negated && !self.ambiguous
    }
}

const REJECT: &[&str] = &[
    "decided not to",
    "decided against",
    "chose not to",
    "ruled out",
    "won't",
    "will not",
    "rejected",
    "gave up on",
];
const CONDITIONAL_START: &[&str] = &["if ", "unless ", "in case ", "should i ", "should we "];
const CONDITIONAL_ANY: &[&str] = &[" if ", " unless ", " would "];
const CONSIDERED: &[&str] = &[
    "considered",
    "considering",
    "thought about",
    "thinking about",
    "thinking of",
    "was thinking",
    "toying with",
];
const POSSIBLE: &[&str] = &[
    "might", "may ", "could ", "maybe", "perhaps", "possibly", "probably", "not sure",
];
const PLANNED: &[&str] = &[
    "plan to",
    "planning to",
    "plans to",
    "going to",
    "intend to",
    "intends to",
    "will ",
    "'ll ",
    "want to",
    "hope to",
    "about to",
];
const NEGATORS: &[&str] = &[
    " not ",
    " never ",
    " no longer ",
    "n't ",
    " no ",
    " stopped ",
    " quit ",
];

/// Classify a statement's modality and polarity.
pub fn classify(statement: &str) -> ModalReading {
    let s = format!(" {} ", statement.to_lowercase().replace('’', "'"));
    let has = |w: &[&str]| w.iter().any(|x| s.contains(x));
    let trimmed = s.trim_start();

    let mut found: Vec<Modality> = Vec::new();
    if has(REJECT) {
        found.push(Modality::Rejected);
    }
    if CONDITIONAL_START.iter().any(|x| trimmed.starts_with(x)) || has(CONDITIONAL_ANY) {
        found.push(Modality::Conditional);
    }
    if has(CONSIDERED) {
        found.push(Modality::Considered);
    }
    if has(POSSIBLE) {
        found.push(Modality::Possible);
    }
    // "won't"/"will not" are rejections, not plans.
    if has(PLANNED) && !found.contains(&Modality::Rejected) {
        found.push(Modality::Planned);
    }

    let rejected = found.contains(&Modality::Rejected);
    // A rejection already carries its negation; other "not"s flip polarity.
    let negated = !rejected && has(NEGATORS);

    // Precedence: a condition or rejection frames everything inside it.
    let order = [
        Modality::Conditional,
        Modality::Rejected,
        Modality::Considered,
        Modality::Possible,
        Modality::Planned,
    ];
    let modality = order
        .iter()
        .copied()
        .find(|m| found.contains(m))
        .unwrap_or(Modality::Actual);
    // Readings that pull in different directions ("might decide not to",
    // "considered never ...") can't be settled by rules.
    let ambiguous = found.len() > 2
        || (found.contains(&Modality::Rejected) && found.len() > 1)
        || (negated && modality != Modality::Actual);
    ModalReading {
        modality,
        negated,
        ambiguous,
    }
}

const MARKERS: &[&str] = &[
    "decided",
    "chose",
    "plan",
    "planning",
    "plans",
    "going",
    "intend",
    "intends",
    "will",
    "might",
    "may",
    "could",
    "maybe",
    "perhaps",
    "possibly",
    "probably",
    "considered",
    "considering",
    "thought",
    "thinking",
    "about",
    "want",
    "hope",
    "if",
    "unless",
    "would",
    "not",
    "never",
    "no",
    "longer",
    "won't",
    "to",
    "i",
    "we",
    "i'll",
    "against",
    "ruled",
    "out",
    "rejected",
    "gave",
    "up",
    "on",
    "do",
    "does",
    "did",
    "don't",
    "doesn't",
    "didn't",
];

/// A crude lemma: enough to line up "moved"/"move"/"moving" in keys.
fn lemma(token: &str) -> String {
    for suffix in ["ing", "ed", "es", "s"] {
        if token.len() > suffix.len() + 2 && token.ends_with(suffix) {
            let stem = &token[..token.len() - suffix.len()];
            return stem.trim_end_matches('e').to_string();
        }
    }
    token.trim_end_matches('e').to_string()
}

/// The canonical identity of a proposition. Two statements may be merged
/// into one proposition only when their keys are equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct PropositionKey {
    pub content: String,
    pub modality: Modality,
    pub negated: bool,
}

pub fn proposition_key(statement: &str) -> PropositionKey {
    let reading = classify(statement);
    let content: Vec<String> = statement
        .to_lowercase()
        .replace('’', "'")
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty() && !MARKERS.contains(t))
        .map(lemma)
        .collect();
    PropositionKey {
        content: content.join(" "),
        modality: reading.modality,
        negated: reading.negated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_readings_of_moving_to_chicago_stay_distinct() {
        let cases = [
            ("I moved to Chicago", Modality::Actual, false),
            ("I plan to move to Chicago", Modality::Planned, false),
            ("I might move to Chicago", Modality::Possible, false),
            (
                "I decided not to move to Chicago",
                Modality::Rejected,
                false,
            ),
            (
                "If I move to Chicago, I will get a car",
                Modality::Conditional,
                false,
            ),
            (
                "I considered moving to Chicago",
                Modality::Considered,
                false,
            ),
        ];
        let mut keys = std::collections::BTreeSet::new();
        for (s, m, neg) in cases {
            let r = classify(s);
            assert_eq!(r.modality, m, "{s}");
            assert_eq!(r.negated, neg, "{s}");
            keys.insert(proposition_key(s));
        }
        assert_eq!(keys.len(), 6);
        // Only the first may assert a present fact.
        assert!(classify(cases[0].0).asserts_positive_fact());
        for (s, _, _) in &cases[1..] {
            assert!(!classify(s).asserts_positive_fact(), "{s}");
        }
    }

    #[test]
    fn not_is_never_dropped() {
        let yes = proposition_key("I use Redis");
        let no = proposition_key("I do not use Redis");
        assert_eq!(yes.content, no.content);
        assert_ne!(yes, no);
        assert!(classify("I don't use Redis").negated);
        assert!(classify("I no longer use Redis").negated);
    }

    #[test]
    fn conflicting_markers_are_ambiguous() {
        assert!(classify("I might decide not to move").ambiguous);
        assert!(classify("I might not move to Chicago").ambiguous);
        assert!(!classify("I moved to Chicago").ambiguous);
    }

    #[test]
    fn a_conditional_decision_is_not_a_decision() {
        let r = classify("If I move to Chicago, I will use Redis");
        assert_eq!(r.modality, Modality::Conditional);
        assert!(!r.asserts_positive_fact());
    }
}
