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
    // Code literals and number words: "null", "zero" are values, not things.
    "null",
    "nil",
    "none",
    "undefined",
    "true",
    "false",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "else",
    "other",
    "another",
    // Status and degree words: values of something, not things.
    "off",
    "on",
    "yes",
    "complete",
    "partial",
    "omitted",
    "empty",
    "full",
    "same",
    "different",
    "real",
    "very",
    "first",
    "last",
    "next",
];

/// Lowercase words that start a fragment, not a name: "every task", "at
/// least one", "using it".
const LOWER_FRAGMENT_STARTERS: &[&str] = &[
    "every", "each", "any", "some", "all", "both", "either", "neither", "another", "same", "such",
    "other", "most", "few", "several", "at", "of", "in", "on", "per", "being", "having",
];

const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];

/// Whether a file name is source code (by extension). Comments and doc
/// strings in such files read like sentences without saying anything worth
/// learning, so their statements aren't extracted unless asked for.
pub fn is_source_code_file(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// File extensions that mean source code.
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "kts", "c", "h", "cc",
    "cpp", "cxx", "hpp", "cs", "swift", "rb", "php", "scala", "sh", "bash", "zsh", "ps1", "lua",
    "dart", "sql", "css", "scss", "less", "vue", "svelte", "zig", "ex", "exs", "erl", "hs", "ml",
    "clj", "r", "jl", "pl", "m", "mm", "groovy", "gradle", "proto",
];

/// Words that end a fragment rather than a name: "null here", "where 0
/// already", "repeated failure usually".
const TRAILING_FILLER: &[&str] = &[
    "here", "there", "already", "usually", "always", "never", "often", "really", "simply", "still",
    "again", "anyway", "too", "now", "also", "only", "else",
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
    /// snake_case_identifier: a name from code, not a word.
    identifier: Regex,
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
        identifier: Regex::new(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+){2,}\b").unwrap(),
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

/// Several bare numbers making up most of the text: a table row or a dump of
/// figures. One or two figures in a sentence are not.
fn is_row_of_figures(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let numeric = tokens
        .iter()
        .filter(|t| !t.chars().any(char::is_alphabetic) && t.chars().any(|c| c.is_ascii_digit()))
        .count();
    numeric >= 3 && (numeric * 2 >= tokens.len() || numeric_share(text) >= 0.4)
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
    if looks_like_math(s) || looks_like_code(s) || patterns().identifier.is_match(s) {
        return false;
    }
    // A row of figures: several bare numbers making up most of the text. One
    // stated figure ("Budget is $40,000") is a fact, however long it is.
    if is_row_of_figures(s) {
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
    let n = strip_wrappers(name);
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
    // A banner or comment lifted from code ("// — Fixture shape —", "# TODO",
    // "/* helpers */") is not a name: names start and end on a letter or digit
    // ("C++", ".NET", "@types" are the few that don't).
    let starts_ok = n.chars().next().is_some_and(|c| {
        c.is_alphanumeric()
            || (matches!(c, '.' | '@') && n.chars().nth(1).is_some_and(char::is_alphanumeric))
    });
    let ends_ok = n
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '+' | '#' | ')' | '.' | '"' | '\''));
    let dash_token = words
        .iter()
        .any(|w| matches!(*w, "—" | "–" | "--" | "-" | "…"));
    if !starts_ok || !ends_ok || dash_token {
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
    // Operators and code punctuation, quotes left inside the name, a stray
    // apostrophe, unbalanced brackets: pieces of code or prose, not names.
    if n.contains([
        '≈', '~', '≠', '≤', '≥', '→', '←', '⇒', '±', '"', '`', '“', '”', '‘',
    ]) || n.matches('(').count() != n.matches(')').count()
        || n.matches('[').count() != n.matches(']').count()
    {
        return false;
    }
    let chars_vec: Vec<char> = n.chars().collect();
    if chars_vec.iter().enumerate().any(|(i, &c)| {
        c == '\''
            && !(i > 0
                && i + 1 < chars_vec.len()
                && chars_vec[i - 1].is_alphanumeric()
                && chars_vec[i + 1].is_alphanumeric())
    }) {
        return false;
    }
    let lower_first = words[0] == first;
    if words.len() >= 2 {
        // "every task", "something very", "zero impact", "at least one".
        if lower_first
            && (LOWER_FRAGMENT_STARTERS.contains(&first.as_str())
                || FILLER_NAMES.contains(&first.as_str()))
        {
            return false;
        }
        if NUMBER_WORDS.contains(&first.as_str())
            && words[1..]
                .iter()
                .all(|w| w.chars().all(|c| !c.is_uppercase()))
        {
            return false;
        }
        // "Replaying a run", "Adopting it", "Returning true": a verb phrase.
        let second = words[1]
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if first.chars().count() >= 5
            && first.ends_with("ing")
            && (FUNCTION_WORDS.contains(&second.as_str())
                || LOWER_FRAGMENT_STARTERS.contains(&second.as_str())
                || matches!(second.as_str(), "true" | "false" | "null"))
        {
            return false;
        }
    }
    // Question words and lowercase articles start fragments ("where 0
    // already", "the installs"); "The Hague" keeps its capital.
    if matches!(
        first.as_str(),
        "where" | "what" | "who" | "how" | "why" | "when" | "which"
    ) || (matches!(first.as_str(), "the" | "a" | "an") && words[0] == first)
    {
        return false;
    }
    // "A zero", "An install": a determiner and a bare noun is a phrase.
    if words.len() == 2 && matches!(first.as_str(), "a" | "an") {
        return false;
    }
    let last = words[words.len() - 1]
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if words.len() >= 2 && TRAILING_FILLER.contains(&last.as_str()) {
        return false;
    }
    // "Everything else", "nothing else": every word is filler.
    if words.iter().all(|w| {
        let w = w
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        FILLER_NAMES.contains(&w.as_str()) || FUNCTION_WORDS.contains(&w.as_str())
    }) {
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

/// A name without the prose or Markdown dressing around it: `"PostgreSQL"`,
/// `**PostgreSQL**` and `PostgreSQL…` are all PostgreSQL.
pub fn strip_wrappers(name: &str) -> &str {
    let lead = |c: char| matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '`' | '*' | '_');
    let trail = |c: char| lead(c) || matches!(c, '…' | '.');
    name.trim()
        .trim_start_matches(lead)
        .trim_end_matches(trail)
        .trim()
}

/// The name inside a phrase: "dark mode in every editor" is about "dark
/// mode". None when no name can be found (a fragment, a number, a pronoun).
pub fn entity_head(phrase: &str) -> Option<String> {
    let first_clause = phrase.split([',', ';']).next().unwrap_or("").trim();
    if first_clause == "Me" {
        return Some("Me".to_string());
    }
    let words: Vec<&str> = first_clause.split_whitespace().collect();
    // A word like "in" or "for" ends the name, unless a capitalized word
    // follows: "Ruby on Rails" and "Research in Motion" are names, while
    // "dark mode in every editor" and "Hetzner CX22 for the backup target"
    // are a name and a description of it.
    let cut = words
        .iter()
        .enumerate()
        .skip(1)
        .find(|&(i, w)| {
            CLAUSE_WORDS.contains(
                &w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
                    .as_str(),
            ) && !words
                .get(i + 1)
                .is_some_and(|next| next.starts_with(char::is_uppercase))
        })
        .map_or(words.len(), |(i, _)| i);
    let head = strip_wrappers(&words[..cut].join(" ")).to_string();
    entity_name_worth_keeping(&head).then_some(head)
}

/// Whether a candidate unit is worth storing, graph edges and all.
pub fn unit_worth_keeping(unit: &ExtractedUnit) -> bool {
    if !statement_worth_keeping(&unit.statement) {
        return false;
    }
    // A subject that can't be a graph node ("the project", "the team") is no
    // reason to lose the statement: the unit is kept without a subject entity.
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

/// Whether one line of a chunk is code, markup or data rather than prose.
fn line_is_code(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if looks_like_code(t) || t.matches('|').count() >= 2 || t.matches('\t').count() >= 2 {
        return true;
    }
    if is_row_of_figures(t) {
        return true;
    }
    let total = non_space(t);
    let symbols = t
        .chars()
        .filter(|c| {
            matches!(
                c,
                '{' | '}' | '[' | ']' | '(' | ')' | '<' | '>' | ';' | '=' | '|' | '\\'
            )
        })
        .count();
    total >= 10 && symbols as f32 / total as f32 > 0.15
}

/// `text` with its code, tables and rows of figures blanked out, byte for
/// byte: the result is as long as the original and the prose is where it was,
/// so offsets into it still point into the original.
pub fn mask_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let trimmed = body.trim();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        let mask = if fence {
            in_fence = !in_fence;
            true
        } else {
            in_fence || line_is_code(body)
        };
        if mask {
            out.extend(std::iter::repeat_n(' ', body.len()));
            out.push_str(&line[body.len()..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The part of a chunk worth reading for statements: the chunk with its code
/// and data blanked out (see [`mask_code`]), or None when no prose is left.
pub fn readable(text: &str) -> Option<String> {
    let masked = mask_code(text);
    if masked.trim().is_empty() || !chunk_is_prose(&masked) {
        return None;
    }
    Some(masked)
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
            "user_account_balance is 100",
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
            "I use Rust",
            "I have diabetes",
            "Budget is $40,000",
            "The budget is $1,000,000",
            "Our rent is $1,200 per month",
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
            ".NET",
            "@types/node",
            "\"PostgreSQL\"",
            "**PostgreSQL**",
            "“PostgreSQL”",
            "PostgreSQL…",
            "Canon EF 70–200mm",
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
            "// — Fixture shape —",
            "// helpers",
            "/* helpers */",
            "# TODO fix this",
            "-- section --",
            "— Fixture shape",
            "Fixture shape —",
            "Fixture — shape",
        ] {
            assert!(!entity_name_worth_keeping(bad), "should reject: {bad:?}");
        }
    }

    #[test]
    fn a_phrase_is_reduced_to_its_name() {
        let head = |p: &str| entity_head(p);
        assert_eq!(
            head("\"PostgreSQL\" for storage").as_deref(),
            Some("PostgreSQL")
        );
        assert_eq!(head("**PostgreSQL**").as_deref(), Some("PostgreSQL"));
        assert_eq!(head("PostgreSQL…").as_deref(), Some("PostgreSQL"));
        assert_eq!(
            head("Canon EF 70–200mm").as_deref(),
            Some("Canon EF 70–200mm")
        );
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
        // A preposition followed by a capital is part of the name.
        assert_eq!(head("Ruby on Rails").as_deref(), Some("Ruby on Rails"));
        assert_eq!(
            head("Research in Motion").as_deref(),
            Some("Research in Motion")
        );
        assert_eq!(head("Bank of America").as_deref(), Some("Bank of America"));
        assert_eq!(
            head("Ruby on Rails for the backend").as_deref(),
            Some("Ruby on Rails")
        );
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
        // Arithmetic is rejected on its statement.
        assert!(!unit_worth_keeping(&unit(
            "2+1 costs 3 dollars in the example",
            Some("2+1"),
            &[],
            "numeric",
        )));
        // A subject that can't be a graph node doesn't cost the statement.
        assert!(unit_worth_keeping(&unit(
            "The project costs $5 per month",
            Some("project"),
            &[],
            "numeric",
        )));
        assert!(unit_worth_keeping(&unit(
            "The team meets every Friday morning",
            Some("the team"),
            &[],
            "llm",
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

    #[test]
    fn prose_next_to_code_is_still_read() {
        let chunk =
            "I use PostgreSQL for storage.\n\n```sql\nSELECT id, name FROM t WHERE x = 3;\n```\n\
                     We decided on Hetzner CX22 for the backup target.\n\
                     | name | cost |\n| a | 12 |\n12 4.5 77 0.31 9\n";
        let masked = mask_code(chunk);
        assert_eq!(masked.len(), chunk.len(), "same length, so offsets line up");
        assert!(masked.contains("I use PostgreSQL for storage."));
        assert!(masked.contains("We decided on Hetzner CX22"));
        assert!(!masked.contains("SELECT"));
        assert!(!masked.contains("| a |"));
        assert!(!masked.contains("0.31"));
        let readable = readable(chunk).expect("there is prose in it");
        assert_eq!(readable, masked);
        // The prose sits at the same offsets in both.
        let at = chunk.find("We decided").unwrap();
        assert_eq!(&readable[at..at + 10], "We decided");
        // Multi-byte text keeps its length too.
        let accented = "Café résumé is fine.\n```\ncafé = 1;\n```\n";
        assert_eq!(mask_code(accented).len(), accented.len());
    }

    #[test]
    fn a_chunk_that_is_all_code_or_data_has_nothing_to_read() {
        assert!(readable("```\nfn main() { let x = 1; }\n```\n").is_none());
        assert!(readable("12 4.5 77 0.31 9 15 22 1.5\n8 40 31 7 12 4.5 77 0.31\n").is_none());
        assert!(readable("   \n\n").is_none());
        assert!(readable("I use PostgreSQL.").is_some());
    }

    #[test]
    fn dressed_up_objects_do_not_cost_the_whole_statement() {
        for text in [
            "I use \"PostgreSQL\" for storage.",
            "I use **PostgreSQL** for storage.",
            "I use PostgreSQL\u{2026}",
            "I use Canon EF 70\u{2013}200mm for portraits.",
        ] {
            let units = crate::extract::rules::extract_units(text);
            assert_eq!(units.len(), 1, "{text}");
            assert!(unit_worth_keeping(&units[0]), "should keep: {text}");
        }
    }

    #[test]
    fn source_files_are_recognised_by_extension() {
        for yes in ["main.rs", "App.tsx", "build.GRADLE", "a/b/c.py", "x.sql"] {
            assert!(is_source_code_file(yes), "{yes}");
        }
        for no in [
            "README.md",
            "notes.txt",
            "report.pdf",
            "data.csv",
            "Makefile",
            "r",
            ".rs.md",
        ] {
            assert!(!is_source_code_file(no), "{no}");
        }
    }

    #[test]
    fn fragments_from_code_and_comments_are_not_names() {
        // From a real review tray (code comments, doc strings and the prose
        // around them).
        for bad in [
            "League average OPS ≈ 0.720",
            "something)",
            "nothing)",
            "// a brace anywhere",
            "/** The one-letter square",
            "interrupted run\" invariant",
            "start reading\" never",
            "`.gitignore` excludes",
            "'.gitignore' excludes",
            "something dynamic (e.g",
            "every task",
            "materialising every",
            "something very",
            "something different",
            "nothing overlapped",
            "Zero impact",
            "at least one",
            "Replaying a run",
            "Adopting it",
            "Returning true",
            "being replaced",
            "one",
            "off",
            "Complete",
            "Omitted",
            "a tenant",
            "the rest",
            "a real statement",
            "prototype/fn-ptr — null",
        ] {
            assert!(!entity_name_worth_keeping(bad), "should reject: {bad:?}");
        }
        for good in [
            "Spring Boot",
            "Boeing 747",
            "One Direction",
            "Two Sigma",
            "Record.serialize",
            "SINGLE_BATCH",
            "P/L",
            "TCP/IP",
            "Acme (UK)",
            "the file's",
        ] {
            // "the file's" is a fragment (lowercase article), the rest are names.
            let expect = good != "the file's";
            assert_eq!(entity_name_worth_keeping(good), expect, "{good:?}");
        }
    }
}
