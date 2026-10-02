//! Is this worth keeping? A deterministic quality gate for what Gather
//! "learns" from a file.
//!
//! The extractors match sentence *shapes* ("X is 75", "I use Y", "X means Y"),
//! and a shape is not meaning: an equation, a line of code, a table row or a
//! sentence fragment fits one just as well as a real fact. Left alone, those
//! become stored claims, graph entities named "2+1" or "the thing I bought
//! yesterday", and "possible match" items for you to review.
//!
//! Everything here is pure (no I/O, no clock, no model), so the same text
//! always gets the same answer and each rule is tested on its own. The gate
//! errs toward keeping: it rejects only what is clearly not a statement about
//! the world (arithmetic, code, data rows, fragments, filler), never a
//! statement merely because it is short or plain.

use std::sync::OnceLock;

use regex::Regex;

use super::rules::ExtractedUnit;
use crate::safety::identity::is_generic_name;

/// Words that carry no content on their own. A statement needs at least
/// [`MIN_CONTENT_WORDS`] words that are not in this list.
const FUNCTION_WORDS: &[&str] = &[
    "i", "me", "my", "mine", "we", "us", "our", "you", "your", "he", "she", "it", "its", "they",
    "them", "their", "this", "that", "these", "those", "there", "here", "the", "a", "an", "is",
    "are", "was", "were", "be", "been", "am", "do", "does", "did", "will", "would", "can", "could",
    "should", "may", "might", "shall", "to", "of", "in", "on", "at", "by", "for", "with", "from",
    "as", "and", "or", "but", "not", "no", "so", "if", "then", "than", "too", "very", "also",
    "just", "about", "into", "over", "what", "which", "who", "whom", "when", "where", "why", "how",
    "equal", "equals", "x",
];

/// Words that begin a clause or a pronoun phrase, not a name: a "name" that
/// starts with one is a sentence fragment.
const FRAGMENT_STARTERS: &[&str] = &[
    "to", "that", "which", "what", "when", "while", "because", "if", "no", "not", "how", "why",
    "who", "whether", "about", "there", "it", "this", "these", "those", "they", "he", "she", "we",
    "i", "you", "so", "and", "or", "but", "then", "also", "just",
];

/// One-word "names" that are moods or filler, not things: "I am sure", "I am
/// tired". A graph node called "sure" tells you nothing.
const FILLER_NAMES: &[&str] = &[
    "sure",
    "happy",
    "fine",
    "good",
    "great",
    "ok",
    "okay",
    "glad",
    "sorry",
    "afraid",
    "ready",
    "able",
    "done",
    "tired",
    "busy",
    "confused",
    "lost",
    "excited",
    "worried",
    "unsure",
    "right",
    "wrong",
    "better",
    "worse",
    "back",
    "later",
    "now",
    "today",
    "tomorrow",
    "yesterday",
    "again",
    "something",
    "someone",
    "anything",
    "everything",
    "nothing",
    "more",
    "less",
    "many",
    "much",
];

/// Words that end the name in a phrase: "dark mode IN every editor",
/// "Hetzner CX22 FOR the backup target". What follows describes the thing; it
/// isn't its name.
const CLAUSE_WORDS: &[&str] = &[
    "in", "for", "on", "at", "with", "by", "from", "because", "that", "which", "when", "while",
    "and", "but", "or", "so", "if", "to", "where", "who",
];

/// A statement that is mostly not words is not about anything.
const MIN_CONTENT_WORDS: usize = 2;

/// Longest a name can be and still be a name (words), and (characters).
const MAX_NAME_WORDS: usize = 6;
const MAX_NAME_CHARS: usize = 60;

struct Patterns {
    math: [Regex; 5],
    code: [Regex; 6],
    url: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        math: [
            // 2+1, 3 = 3, 4*x, 2^8
            Regex::new(r"\d\s*[+*×÷^=]\s*[\d(a-zA-Z]").unwrap(),
            // x = 3, a=b
            Regex::new(r"[a-zA-Z)\]]\s*=\s*[\w(\-]").unwrap(),
            // 5 - 2 (spaced, so dates like 2026-03-01 are left alone)
            Regex::new(r"\d\s[-−]\s\d").unwrap(),
            // 3 is 3, 3 is equal to 3, 3 equals 3
            Regex::new(r"(?i)\d\s+(?:is|equals?)\s+(?:equal\s+to\s+)?\d").unwrap(),
            // LaTeX
            Regex::new(r"\\(?:frac|sum|int|sqrt|times|cdot|le|ge|neq)\b|\$[^$]*\\[a-z]+").unwrap(),
        ],
        code: [
            // braces and statement ends
            Regex::new(r"[{}]|;\s*$|;\s").unwrap(),
            // arrows and scope operators
            Regex::new(r"=>|->|::|\+\+|--\w|\|\|").unwrap(),
            // a call: name(...)
            Regex::new(r"\b[A-Za-z_]\w*\([^)]*\)").unwrap(),
            // markup
            Regex::new(r"</?[A-Za-z][^>]*>").unwrap(),
            // a keyword that opens a code line
            Regex::new(
                r"(?m)^\s*(?:import|from|def|fn|let|const|var|class|return|#include|use|pub)\s",
            )
            .unwrap(),
            // hex literals and long identifiers_with_underscores
            Regex::new(r"\b0x[0-9a-fA-F]{2,}\b|\b[a-z]+_[a-z0-9_]+_[a-z0-9_]+\b").unwrap(),
        ],
        url: Regex::new(r"\S*(?:://|www\.)\S*").unwrap(),
    })
}

fn content_words(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
        .map(|w| w.trim_matches(['\'', '-']).to_lowercase())
        .filter(|w| w.chars().any(char::is_alphabetic) && !FUNCTION_WORDS.contains(&w.as_str()))
        .count()
}

fn non_space(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// The share of the text that is bare numbers (tokens with no letter in them):
/// high for a table row or a dump of figures, low for prose or for a name that
/// contains digits ("CX22", "RTX 4090 Ti").
fn numeric_share(text: &str) -> f32 {
    let total = non_space(text);
    if total == 0 {
        return 0.0;
    }
    let numeric: usize = text
        .split_whitespace()
        .filter(|t| !t.chars().any(char::is_alphabetic))
        .map(|t| t.chars().count())
        .sum();
    numeric as f32 / total as f32
}

fn has_figure(text: &str) -> bool {
    text.split_whitespace()
        .any(|t| !t.chars().any(char::is_alphabetic) && t.chars().any(|c| c.is_ascii_digit()))
}

/// An equation or arithmetic: "2+1 is 3", "x = 4", "3 is equal to 3".
pub fn looks_like_math(text: &str) -> bool {
    patterns().math.iter().any(|re| re.is_match(text))
}

/// A line of code, markup or a command rather than prose. One weak signal (a
/// pair of parentheses) isn't enough; two signals, or braces or markup, are.
pub fn looks_like_code(text: &str) -> bool {
    let signals = patterns()
        .code
        .iter()
        .filter(|re| re.is_match(text))
        .count();
    let backticks = text.matches('`').count() >= 2;
    signals >= 2 || (signals >= 1 && backticks) || text.contains(['{', '}']) || text.contains("</")
}

/// Whether one statement says something: not arithmetic, code or a data row,
/// and with enough words of its own.
pub fn statement_worth_keeping(statement: &str) -> bool {
    let s = statement.trim();
    if non_space(s) < 8 {
        return false;
    }
    if looks_like_math(s) || looks_like_code(s) {
        return false;
    }
    // A row of figures: mostly digits, however it is punctuated.
    if numeric_share(s) >= 0.4 {
        return false;
    }
    if s.matches('|').count() >= 2 || s.matches('\t').count() >= 2 {
        return false;
    }
    // Links don't count as words.
    let without_links = patterns().url.replace_all(s, " ");
    // A stated figure is content: "The budget is $40,000" says something.
    let figure = usize::from(has_figure(&without_links));
    content_words(&without_links) + figure >= MIN_CONTENT_WORDS
}

/// Whether `name` could be the name of a thing: short, mostly letters, and not
/// a pronoun, a number, an expression or a piece of a sentence.
pub fn entity_name_worth_keeping(name: &str) -> bool {
    let n = name.trim();
    if n == "Me" {
        return true;
    }
    let chars = n.chars().count();
    if !(2..=MAX_NAME_CHARS).contains(&chars) {
        return false;
    }
    let words: Vec<&str> = n.split_whitespace().collect();
    if words.is_empty() || words.len() > MAX_NAME_WORDS {
        return false;
    }
    // A name has a letter in it ("A100", "CX22", "v2"); "42", "$75" and
    // "2026-03-01" don't, and neither does an expression.
    if !n.chars().any(char::is_alphabetic) {
        return false;
    }
    if n.contains([
        '=', '<', '>', '{', '}', '|', '\\', '^', '×', '÷', '?', '!', ';', ':',
    ]) || n.contains(" + ")
        || (n.contains(',') && words.len() > 4)
    {
        return false;
    }
    let first = words[0].to_lowercase();
    if FRAGMENT_STARTERS.contains(&first.as_str()) {
        return false;
    }
    if words.iter().all(|w| {
        FUNCTION_WORDS.contains(
            &w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
                .as_str(),
        )
    }) {
        return false;
    }
    // Names in prose are short noun phrases. A long run that starts in lower
    // case is a clause ("thing i bought at the store").
    if words.len() >= 4 && n.chars().next().is_some_and(char::is_lowercase) {
        return false;
    }
    if words.len() == 1 && FILLER_NAMES.contains(&words[0].to_lowercase().as_str()) {
        return false;
    }
    !is_generic_name(n)
}

/// The name inside a phrase: "dark mode in every editor" is about "dark
/// mode". None when no name can be found (a fragment, a number, a pronoun).
pub fn entity_head(phrase: &str) -> Option<String> {
    let first_clause = phrase.split([',', ';']).next().unwrap_or("").trim();
    if first_clause == "Me" {
        return Some("Me".to_string());
    }
    let words: Vec<&str> = first_clause.split_whitespace().collect();
    let cut = words
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, w)| {
            CLAUSE_WORDS.contains(
                &w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
                    .as_str(),
            )
        })
        .map_or(words.len(), |(i, _)| i);
    let head = words[..cut].join(" ");
    entity_name_worth_keeping(&head).then_some(head)
}

/// Whether a candidate unit is worth storing, graph edges and all.
pub fn unit_worth_keeping(unit: &ExtractedUnit) -> bool {
    if !statement_worth_keeping(&unit.statement) {
        return false;
    }
    if let Some(subject) = unit.subject.as_deref() {
        if entity_head(subject).is_none() {
            return false;
        }
    }
    // For these sentence shapes the object *is* the point ("I use X",
    // "we decided on X", "X means Y"). When it is a fragment there is nothing
    // left to keep.
    let object_is_the_point = matches!(
        unit.attrs.get("pattern").and_then(|p| p.as_str()),
        Some("decision" | "first_person" | "definition")
    );
    if object_is_the_point
        && !unit.objects.is_empty()
        && !unit
            .objects
            .iter()
            .any(|(name, _)| entity_head(name).is_some())
    {
        return false;
    }
    true
}

/// Whether a stretch of text is prose worth reading for statements, as opposed
/// to code, a data table or a dump of figures. Short text can't be judged and
/// is left to the sentence-level checks.
pub fn chunk_is_prose(text: &str) -> bool {
    let total = non_space(text);
    if total < 80 {
        return true;
    }
    if numeric_share(text) >= 0.35 {
        return false;
    }
    let symbols = text
        .chars()
        .filter(|c| {
            matches!(
                c,
                '{' | '}' | '[' | ']' | '(' | ')' | ';' | '=' | '<' | '>' | '|' | '\\'
            )
        })
        .count();
    symbols as f32 / total as f32 <= 0.08
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unit(
        statement: &str,
        subject: Option<&str>,
        objects: &[&str],
        pattern: &str,
    ) -> ExtractedUnit {
        ExtractedUnit {
            kind: "fact",
            statement: statement.to_string(),
            subject: subject.map(String::from),
            objects: objects
                .iter()
                .map(|o| ((*o).to_string(), "rel".to_string()))
                .collect(),
            char_start: 0,
            char_end: statement.len(),
            confidence: 0.6,
            attrs: json!({ "pattern": pattern }),
            event_time: None,
        }
    }

    #[test]
    fn arithmetic_is_not_a_fact() {
        for s in [
            "2+1 is 3 is equal to = 3",
            "2 + 1 is 3",
            "3 is equal to 3",
            "x = 4 so y = 5",
            "The result is 4*x plus 2",
            "5 - 2 is 3",
            "The sum \\frac{1}{2} is small",
        ] {
            assert!(!statement_worth_keeping(s), "should reject: {s}");
        }
    }

    #[test]
    fn code_and_data_are_not_facts() {
        for s in [
            "const total = items.map(i => i.price);",
            "fn main() { println!(\"hi\"); }",
            "<div class=\"price\">75</div>",
            "result = compute_total(items, tax_rate)",
            "user_account_balance is 100 | 200 | 300",
            "12 | 4.5 | 77 | 0.31 | 9",
        ] {
            assert!(!statement_worth_keeping(s), "should reject: {s}");
        }
    }

    #[test]
    fn real_statements_are_kept() {
        for s in [
            "We decided on Hetzner CX22 for the backup target",
            "The budget is $40,000",
            "I use Snapshot9f3a1c0d7b2e4a58b6c1d2e3f4a5b6c7 for storage",
            "My VPS budget is $75 per month",
            "I use PostgreSQL for the knowledge store",
            "On 2025-12-01 we launched the beta",
            "Our latency budget is 150 ms",
            "I work at Anthropic on developer tools",
            "The deadline moved to 2026-03-01 because of the audit",
            "I have a dog",
        ] {
            assert!(statement_worth_keeping(s), "should keep: {s}");
        }
    }

    #[test]
    fn filler_has_too_few_words() {
        for s in [
            "I am happy",
            "It is what it is",
            "I have 3",
            "We are here now",
        ] {
            assert!(!statement_worth_keeping(s), "should reject: {s}");
        }
    }

    #[test]
    fn links_do_not_count_as_words() {
        assert!(!statement_worth_keeping(
            "It is at https://example.com/a/b/c/d"
        ));
        assert!(statement_worth_keeping(
            "The backup runbook lives at https://example.com/runbook"
        ));
    }

    #[test]
    fn names_must_look_like_names() {
        for good in [
            "PostgreSQL",
            "Hetzner CX22",
            "dark mode",
            "latency budget",
            "Me",
            "3D printer",
            "v2",
            "RTX 4090",
            "A100",
            "GPT-4",
            "C++",
            "Snapshot9f3a1c0d7b2e4a58b6c1d2e3f4a5b6c7",
        ] {
            assert!(entity_name_worth_keeping(good), "should keep: {good}");
        }
        for bad in [
            "2+1",
            "3",
            "$75",
            "2026-03-01",
            "it",
            "this",
            "to think about it later",
            "no idea what to do next",
            "that I bought at the store yesterday",
            "thing i bought at the store",
            "sure",
            "Tired",
            "What now?",
            "Project",
            "x",
            "",
        ] {
            assert!(!entity_name_worth_keeping(bad), "should reject: {bad:?}");
        }
    }

    #[test]
    fn a_phrase_is_reduced_to_its_name() {
        let head = |p: &str| entity_head(p);
        assert_eq!(
            head("dark mode in every editor").as_deref(),
            Some("dark mode")
        );
        assert_eq!(
            head("Hetzner CX22 for the backup target").as_deref(),
            Some("Hetzner CX22")
        );
        assert_eq!(
            head("Anthropic on developer tools").as_deref(),
            Some("Anthropic")
        );
        assert_eq!(head("PostgreSQL").as_deref(), Some("PostgreSQL"));
        assert_eq!(head("Acme, Inc.").as_deref(), Some("Acme"));
        assert_eq!(head("Me").as_deref(), Some("Me"));
        // Nothing left that is a name.
        for none in [
            "no idea what to do next",
            "to think about it later",
            "thing that I bought at the store",
            "2+1",
            "3",
            "it",
        ] {
            assert_eq!(head(none), None, "{none}");
        }
    }

    #[test]
    fn a_unit_needs_a_real_subject_and_object() {
        assert!(unit_worth_keeping(&unit(
            "I use PostgreSQL for the knowledge store",
            Some("Me"),
            &["PostgreSQL for the knowledge store"],
            "first_person",
        )));
        // Vacuous: the object is a fragment.
        assert!(!unit_worth_keeping(&unit(
            "I have no idea what to do next",
            Some("Me"),
            &["no idea what to do next"],
            "first_person",
        )));
        assert!(!unit_worth_keeping(&unit(
            "We decided to think about it later",
            Some("Me"),
            &["to think about it later"],
            "decision",
        )));
        // A subject that isn't a name.
        assert!(!unit_worth_keeping(&unit(
            "2+1 costs 3 dollars in the example",
            Some("2+1"),
            &[],
            "numeric",
        )));
        // No objects to check: kept on the statement.
        assert!(unit_worth_keeping(&unit(
            "Our latency budget is 150 ms",
            Some("latency budget"),
            &[],
            "numeric",
        )));
    }

    #[test]
    fn code_and_tables_are_not_prose() {
        let code = "fn main() { let x = vec![1, 2, 3]; for i in x { println!(\"{}\", i); } } \
                    fn other() { let y = (1 + 2) * 3; if y > 4 { return; } }";
        assert!(!chunk_is_prose(code));
        let table = "12 4.5 77 0.31 9 15 22 1.5 8 40 31 7 12 4.5 77 0.31 9 15 22 1.5 8 40 31 7 \
                     12 4.5 77 0.31 9 15 22 1.5 8 40 31 7 12 4.5 77 0.31 9 15 22 1.5 8 40 31 7";
        assert!(!chunk_is_prose(table));
        let prose = "We decided on Hetzner for the backup target. The migration finished in \
                     March, and the latency budget is 150 ms. I prefer dark mode in every editor.";
        assert!(chunk_is_prose(prose));
        // Too short to judge.
        assert!(chunk_is_prose("I use PostgreSQL."));
    }
}
