//! Rule-based atomic-unit extraction (write-up §5.2).
//!
//! Deterministic, high-precision/low-recall regex patterns over
//! sentence-split text. Always on and fully offline; the optional local-LLM
//! extractor (ollama.rs) supplements — never replaces — these rules.

use std::sync::OnceLock;

use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use serde_json::{json, Value};

/// A candidate atomic unit produced by an extractor, before persistence
/// (which resolves entities, adjusts confidence for source context, and
/// deduplicates on the normalized statement hash).
#[derive(Debug, Clone)]
pub struct ExtractedUnit {
    pub kind: &'static str, // fact | claim | decision | preference | event
    pub statement: String,
    /// Entity name the statement is about ("Me" for first-person).
    pub subject: Option<String>,
    /// (entity name, relation type) edges asserted by this unit.
    pub objects: Vec<(String, String)>,
    /// Byte offsets of the supporting span within the source chunk.
    pub char_start: usize,
    pub char_end: usize,
    pub confidence: f32,
    pub attrs: Value,
    /// Parsed event time (TEMPORAL_EVENT with an ISO date), overrides the
    /// chunk timestamp as valid_from.
    pub event_time: Option<DateTime<Utc>>,
}

const BASE_CONFIDENCE: f32 = 0.6;

struct Patterns {
    decision: Regex,
    first_person: Regex,
    constraint: Regex,
    numeric: Regex,
    temporal: Regex,
    definition: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        decision: Regex::new(
            r"(?i)\b(?:I|we)(?:'ve| have)?\s+(?:decided(?:\s+on|\s+to\s+(?:use|go\s+with))?|chose|picked|settled\s+on|committed\s+to|switched\s+(?:from\s+.{2,40}?\s+)?to|will\s+use|agreed\s+(?:on|to\s+use)|(?:am|are)\s+going\s+with)\s+(.{2,80}?)[\s.]*$",
        )
        .unwrap(),
        first_person: Regex::new(
            r"(?i)^\s*(?:I|we)\s+(?:(never|don't|do\s+not|no\s+longer)\s+)?(prefer|use|have|am|run|host|keep|store|deploy|live\s+in|work\s+(?:at|for|on))\s+(.{2,80}?)[\s.]*$",
        )
        .unwrap(),
        numeric: Regex::new(
            r"(?i)^\s*(?:my|our|the)?\s*(.{2,60}?)\s+(?:is|costs?|takes?|weighs?)\s+(?:about\s+|around\s+|approximately\s+)?([$€£]?\d[\d,]*(?:\.\d+)?)\s*((?:[a-zA-Z%]+)?(?:\s+per\s+\w+)?)[\s.]*$",
        )
        .unwrap(),
        temporal: Regex::new(
            r"(?i)^\s*(?:on|since|starting)\s+(\d{4}-\d{2}-\d{2})[,]?\s+(.{3,120}?)[\s.]*$",
        )
        .unwrap(),
        definition: Regex::new(
            r"(?i)^\s*(.{2,60}?)\s+(?:means|is\s+defined\s+as|stands\s+for)\s+(.{2,120}?)[\s.]*$",
        )
        .unwrap(),
        constraint: Regex::new(
            r"(?i)^\s*(?:I|we)\s+(must\s+not|must\s+never|cannot|can't|can\s+not|won't|will\s+not|refuse\s+to|never)\s+([a-z]+)\s+(.{2,80}?)[\s.]*$",
        )
        .unwrap(),
    })
}

/// A branch, not a choice. In a developer's notes "I committed to main" and
/// "we switched to dev" are version control, and the branch they name is not
/// a decision worth recording. Only read this way in lower case, so a real
/// name keeps its meaning ("we switched to Trunk Club").
const VCS_TARGETS: &[&str] = &[
    "main",
    "master",
    "trunk",
    "develop",
    "dev",
    "head",
    "origin",
    "upstream",
    "git",
    "branch",
    "repo",
    "repository",
];

/// What can follow a branch name without the name turning into a phrase:
/// "main before lunch", "dev at 5pm", "master yesterday". Anything else and
/// the word is modifying a noun, not naming a branch.
const AFTER_VCS_TARGET: &[&str] = &[
    "before",
    "after",
    "at",
    "on",
    "in",
    "this",
    "last",
    "yesterday",
    "today",
    "tonight",
    "earlier",
    "later",
    "again",
    "then",
    "and",
    "so",
    "but",
    "instead",
    "once",
    "twice",
    "when",
    "while",
    "because",
    "from",
    "with",
    "around",
    "by",
    "now",
];

/// Whether a decision's object is really a version-control target. `clean`
/// has already dropped a leading article, so "the repo" arrives as "repo".
/// The first word matching is not enough: "a master plan for the migration"
/// is a plan, and discarding it would lose a real decision.
fn is_vcs_target(object: &str) -> bool {
    let word = |w: &str| {
        w.trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase()
    };
    let mut words = object.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if first.chars().any(char::is_uppercase) || !VCS_TARGETS.contains(&word(first).as_str()) {
        return false;
    }
    match words.next() {
        None => true,
        Some(next) => AFTER_VCS_TARGET.contains(&word(next).as_str()),
    }
}

/// Spoken openers that say nothing about the world ("So I use X", "Yeah, we
/// settled on Y"). Dropping one lets the anchored patterns below see the
/// sentence underneath it. Hedges ("might", "maybe", "probably") are
/// deliberately NOT here: they carry the modality the safety layer reads, and
/// stripping one would turn a possibility into an asserted fact.
const DISCOURSE_MARKERS: &[&str] = &[
    "to be honest",
    "you know",
    "i mean",
    "basically",
    "honestly",
    "actually",
    "frankly",
    "anyway",
    "listen",
    "right",
    "look",
    "okay",
    "yeah",
    "yep",
    "well",
    "ok",
    "so",
];

/// Words a sentence the patterns can read starts with. A marker with no comma
/// after it is only dropped when one of these follows, so "Well, I use X"
/// loses its opener while "Well is 40 metres deep" keeps its subject.
const SENTENCE_OPENERS: &[&str] = &[
    "i", "we", "my", "our", "the", "a", "an", "on", "since", "starting",
];

/// How many leading bytes of `sentence` are discourse markers. Walks at most
/// three ("Okay so yeah, I use X"), and returns 0 — leaving the sentence
/// exactly as it was — whenever dropping the word could change what it says.
fn discourse_prefix_len(sentence: &str) -> usize {
    let mut cut = 0usize;
    for _ in 0..3 {
        let rest = &sentence[cut..];
        let Some(marker) = DISCOURSE_MARKERS.iter().find(|m| {
            rest.get(..m.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(m))
                && rest[m.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric())
        }) else {
            return cut;
        };
        let after = &rest[marker.len()..];
        let comma = after.starts_with(',');
        let tail = after.trim_start_matches(',').trim_start();
        if tail.is_empty() || (!comma && tail.len() == after.len()) {
            return cut;
        }
        let next_word = tail
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        let opens = SENTENCE_OPENERS.contains(&next_word.as_str())
            || DISCOURSE_MARKERS.contains(&next_word.as_str());
        if !comma && !opens {
            return cut;
        }
        cut = sentence.len() - tail.len();
    }
    cut
}

/// Split text into sentences with byte offsets. Sentence boundaries are
/// `.`/`!`/`?` followed by whitespace, and hard newlines.
fn sentences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let is_terminator = (b == b'.' || b == b'!' || b == b'?')
            && bytes
                .get(i + 1)
                .map(|n| n.is_ascii_whitespace())
                .unwrap_or(true);
        if is_terminator || b == b'\n' {
            let end = if is_terminator { i + 1 } else { i };
            if end > start {
                out.push((start, end));
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push((start, bytes.len()));
    }
    out
}

/// Strip a leading article from an entity phrase ("the Hetzner VPS" -> "Hetzner VPS").
fn clean(s: &str) -> String {
    let tidied = tidy(s);
    let lowered = tidied.to_lowercase();
    for article in ["a ", "an ", "the "] {
        if lowered.starts_with(article) {
            return tidied[article.len()..].to_string();
        }
    }
    tidied
}

fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run every §5.2 pattern over the chunk; first matching pattern per
/// sentence wins so a single sentence never produces duplicate units.
pub fn extract_units(text: &str) -> Vec<ExtractedUnit> {
    let p = patterns();
    let mut units = Vec::new();

    for (start, end) in sentences(text) {
        let raw = &text[start..end];
        let trimmed = raw.trim();
        // The span still has to point at the words the statement is made of,
        // so skipping an opener moves its start. Nothing stripped, nothing
        // moved: the offsets of every sentence without one are unchanged.
        let (sentence, span_start) = match discourse_prefix_len(trimmed) {
            0 => (trimmed, start),
            n => {
                let kept = trimmed[n..].trim_start();
                let lead = raw.len() - raw.trim_start().len();
                (kept, start + lead + (trimmed.len() - kept.len()))
            }
        };
        if sentence.len() < 10 || sentence.len() > 400 {
            continue;
        }
        // A question states nothing ("What is 2+2?").
        if sentence.ends_with('?') {
            continue;
        }
        let statement = tidy(sentence.trim_end_matches(['.', '!', '?']));
        let span = (span_start, end);

        if let Some(c) = p.decision.captures(sentence) {
            let object = clean(&tidy(c.get(1).unwrap().as_str()));
            if !is_vcs_target(&object) {
                units.push(ExtractedUnit {
                    kind: "decision",
                    statement,
                    subject: Some("Me".to_string()),
                    objects: vec![(object, "decided_on".to_string())],
                    char_start: span.0,
                    char_end: span.1,
                    confidence: BASE_CONFIDENCE,
                    attrs: json!({ "pattern": "decision" }),
                    event_time: None,
                });
                continue;
            }
        }

        if let Some(c) = p.temporal.captures(sentence) {
            let date = c.get(1).unwrap().as_str();
            let event_time = NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|dt| DateTime::from_naive_utc_and_offset(dt, Utc));
            units.push(ExtractedUnit {
                kind: "event",
                statement,
                subject: None,
                objects: vec![],
                char_start: span.0,
                char_end: span.1,
                confidence: BASE_CONFIDENCE,
                attrs: json!({ "pattern": "temporal_event", "date": date }),
                event_time,
            });
            continue;
        }

        if let Some(c) = p.first_person.captures(sentence) {
            let negated = c.get(1).is_some();
            let verb = c.get(2).unwrap().as_str().to_lowercase();
            let object = tidy(c.get(3).unwrap().as_str());
            let (kind, relation): (&'static str, &str) = match verb.as_str() {
                "prefer" => ("preference", "prefers"),
                "use" => ("fact", "uses"),
                "have" => ("fact", "has"),
                "am" => ("fact", "is"),
                "run" => ("fact", "runs"),
                "host" => ("fact", "hosts"),
                "keep" => ("fact", "keeps"),
                "store" => ("fact", "stores"),
                "deploy" => ("fact", "deploys"),
                v if v.starts_with("live") => ("fact", "lives_in"),
                _ => ("fact", "works_at"),
            };
            // A negated statement is still a fact worth keeping (and pairing
            // in the contradiction scan) but asserts no positive graph edge.
            let objects = if negated {
                vec![]
            } else {
                vec![(clean(&object), relation.to_string())]
            };
            units.push(ExtractedUnit {
                kind,
                statement,
                subject: Some("Me".to_string()),
                objects,
                char_start: span.0,
                char_end: span.1,
                confidence: BASE_CONFIDENCE,
                attrs: json!({ "pattern": "first_person", "verb": verb, "negated": negated }),
                event_time: None,
            });
            continue;
        }

        // "I won't use X", "I must not store Y", "I never commit Z": a rule
        // the author holds themselves to. It is a standing stance, so it is
        // kept as a preference — and because it is a prohibition it asserts
        // no edge: the graph must never come away saying they use X. The
        // thing ruled out rides in attrs so the gate can check it is nameable
        // without it becoming a relationship.
        if let Some(c) = p.constraint.captures(sentence) {
            let marker = tidy(c.get(1).unwrap().as_str()).to_lowercase();
            let verb = c.get(2).unwrap().as_str().to_lowercase();
            let object = clean(&tidy(c.get(3).unwrap().as_str()));
            units.push(ExtractedUnit {
                kind: "preference",
                statement,
                subject: Some("Me".to_string()),
                objects: vec![],
                char_start: span.0,
                char_end: span.1,
                confidence: BASE_CONFIDENCE,
                attrs: json!({
                    "pattern": "constraint",
                    "marker": marker,
                    "verb": verb,
                    "object": object,
                    "negated": true,
                }),
                event_time: None,
            });
            continue;
        }

        if let Some(c) = p.definition.captures(sentence) {
            let term = tidy(c.get(1).unwrap().as_str());
            let meaning = tidy(c.get(2).unwrap().as_str());
            units.push(ExtractedUnit {
                kind: "fact",
                statement,
                subject: Some(term.clone()),
                objects: vec![(meaning, "defined_as".to_string())],
                char_start: span.0,
                char_end: span.1,
                confidence: BASE_CONFIDENCE,
                attrs: json!({ "pattern": "definition" }),
                event_time: None,
            });
            continue;
        }

        if let Some(c) = p.numeric.captures(sentence) {
            let subject = tidy(c.get(1).unwrap().as_str());
            let value = c.get(2).unwrap().as_str().to_string();
            let unit = c.get(3).map(|m| tidy(m.as_str())).unwrap_or_default();
            // Skip degenerate subjects ("it", "that", pronouns).
            if subject.len() < 3
                || ["it", "that", "this"].contains(&subject.to_lowercase().as_str())
            {
                continue;
            }
            units.push(ExtractedUnit {
                kind: "claim",
                statement,
                subject: Some(subject),
                objects: vec![],
                char_start: span.0,
                char_end: span.1,
                confidence: BASE_CONFIDENCE,
                attrs: json!({ "pattern": "numeric", "value": value, "unit": unit }),
                event_time: None,
            });
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_pattern() {
        let units = extract_units("We decided on Hetzner CX22 for the backup target.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "decision");
        assert_eq!(units[0].objects[0].1, "decided_on");
        assert!(units[0].objects[0].0.contains("Hetzner"));
    }

    #[test]
    fn first_person_fact_and_preference() {
        let units = extract_units("I use PostgreSQL. I prefer dark mode.");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].kind, "fact");
        assert_eq!(
            units[0].objects[0],
            ("PostgreSQL".to_string(), "uses".to_string())
        );
        assert_eq!(units[1].kind, "preference");
        assert_eq!(units[1].objects[0].1, "prefers");
    }

    #[test]
    fn negated_first_person_keeps_unit_but_drops_edge() {
        let units = extract_units("I never use MongoDB for anything.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].attrs["negated"], serde_json::json!(true));
        assert!(
            units[0].objects.is_empty(),
            "negation must not assert a positive edge"
        );
        assert!(units[0].statement.contains("never"));
    }

    #[test]
    fn numeric_assertion_with_attrs() {
        let units = extract_units("My VPS budget is $75 per month.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "claim");
        assert_eq!(units[0].attrs["value"], "$75");
        assert_eq!(units[0].subject.as_deref(), Some("VPS budget"));
    }

    #[test]
    fn temporal_event_parses_iso_date() {
        let units = extract_units("On 2026-03-01, the VPS migration was completed.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "event");
        assert_eq!(
            units[0].event_time.unwrap().date_naive().to_string(),
            "2026-03-01"
        );
    }

    #[test]
    fn definition_creates_relationship() {
        let units = extract_units("CX22 means the 2-vCPU Hetzner shared instance.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].objects[0].1, "defined_as");
        assert_eq!(units[0].subject.as_deref(), Some("CX22"));
    }

    #[test]
    fn spans_point_into_source_text() {
        let text = "Irrelevant filler line\nI use PostgreSQL.";
        let units = extract_units(text);
        assert_eq!(units.len(), 1);
        let span = &text[units[0].char_start..units[0].char_end];
        assert!(span.contains("I use PostgreSQL"));
    }

    #[test]
    fn decisions_cover_settling_committing_and_switching_away() {
        for text in [
            "We settled on Hetzner CX22 for the backup target.",
            "We committed to Postgres for the knowledge store.",
            "I switched from MySQL to Postgres for the knowledge store.",
        ] {
            let units = extract_units(text);
            assert_eq!(units.len(), 1, "{text}");
            assert_eq!(units[0].kind, "decision", "{text}");
        }
        let switched = extract_units("I switched from MySQL to Postgres for the knowledge store.");
        assert!(
            switched[0].objects[0].0.starts_with("Postgres"),
            "the decision is what was switched TO: {:?}",
            switched[0].objects[0].0
        );
    }

    #[test]
    fn a_branch_is_not_a_decision() {
        assert!(
            extract_units("I committed to main before lunch.").is_empty(),
            "committing to a branch is version control, not a choice"
        );
        assert_eq!(
            extract_units("We committed to Postgres for the knowledge store.").len(),
            1
        );
        // The branch word only disqualifies the object when it IS the object.
        let plan = extract_units("We committed to a master plan for the migration.");
        assert_eq!(plan.len(), 1, "a master plan is a decision, not a branch");
        assert_eq!(plan[0].kind, "decision");
    }

    #[test]
    fn first_person_covers_what_i_run_and_where_i_keep_things() {
        let units = extract_units(
            "I run the daemon on a Hetzner box. I keep backups on an external drive. \
             We deploy Gather with systemd.",
        );
        let relations: Vec<&str> = units.iter().map(|u| u.objects[0].1.as_str()).collect();
        assert_eq!(relations, ["runs", "keeps", "deploys"]);
    }

    #[test]
    fn a_rule_i_hold_myself_to_is_kept_without_an_edge() {
        let units = extract_units("I won't use Google Analytics on any site.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "preference");
        assert_eq!(units[0].attrs["pattern"], json!("constraint"));
        assert_eq!(
            units[0].attrs["object"],
            json!("Google Analytics on any site")
        );
        assert!(
            units[0].objects.is_empty(),
            "a prohibition must not leave the graph saying I use the thing"
        );
    }

    #[test]
    fn a_spoken_opener_is_dropped_and_the_span_follows_it() {
        let text = "Basically I use PostgreSQL.";
        let units = extract_units(text);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].statement, "I use PostgreSQL");
        assert_eq!(
            &text[units[0].char_start..units[0].char_end],
            "I use PostgreSQL.",
            "the span has to point at the words the statement is made of"
        );

        // Several in a row, which is how people actually write.
        let piled = extract_units("Okay so yeah, I use PostgreSQL.");
        assert_eq!(piled.len(), 1);
        assert_eq!(piled[0].statement, "I use PostgreSQL");
    }

    #[test]
    fn an_opener_that_is_the_subject_stays() {
        // "Well" is the thing being described here, not a filler word.
        let units = extract_units("Well is 40 metres.");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].subject.as_deref(), Some("Well"));
    }

    #[test]
    fn a_hedge_is_never_dropped() {
        // Stripping "Maybe" would turn a possibility into a stored fact, and
        // the safety layer reads modality off the statement it is given.
        assert!(extract_units("Maybe I use Postgres for the backend.").is_empty());
        assert!(extract_units("Probably I use Postgres for the backend.").is_empty());
    }

    #[test]
    fn ignores_unmatched_and_short_sentences() {
        let units = extract_units("Hello. The weather might change eventually somehow.");
        assert!(units.is_empty());
    }
}
