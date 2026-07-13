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
    numeric: Regex,
    temporal: Regex,
    definition: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        decision: Regex::new(
            r"(?i)\b(?:I|we)(?:'ve| have)?\s+(?:decided(?:\s+on|\s+to\s+(?:use|go\s+with))?|chose|picked|switched\s+to|will\s+use|agreed\s+(?:on|to\s+use)|(?:am|are)\s+going\s+with)\s+(.{2,80}?)[\s.]*$",
        )
        .unwrap(),
        first_person: Regex::new(
            r"(?i)^\s*(?:I|we)\s+(?:(never|don't|do\s+not|no\s+longer)\s+)?(prefer|use|have|am|live\s+in|work\s+(?:at|for|on))\s+(.{2,80}?)[\s.]*$",
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
    })
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
        let sentence = raw.trim();
        if sentence.len() < 10 || sentence.len() > 400 {
            continue;
        }
        let statement = tidy(sentence.trim_end_matches(['.', '!', '?']));
        let span = (start, end);

        if let Some(c) = p.decision.captures(sentence) {
            let object = tidy(c.get(1).unwrap().as_str());
            units.push(ExtractedUnit {
                kind: "decision",
                statement,
                subject: Some("Me".to_string()),
                objects: vec![(clean(&object), "decided_on".to_string())],
                char_start: span.0,
                char_end: span.1,
                confidence: BASE_CONFIDENCE,
                attrs: json!({ "pattern": "decision" }),
                event_time: None,
            });
            continue;
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
    fn ignores_unmatched_and_short_sentences() {
        let units = extract_units("Hello. The weather might change eventually somehow.");
        assert!(units.is_empty());
    }
}
