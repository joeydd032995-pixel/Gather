//! Perplexity Markdown export reader.
//!
//! Perplexity's "Export → Markdown" gives one thread per file:
//!
//! ```text
//! <img src="https://r2cdn.perplexity.ai/…logo…"/>      (first turn only)
//!
//! # your question
//!
//! the answer, with citation markers like [^1_3] …
//!
//! <span style="display:none">[^1_7][^1_8]</span>        (uncited sources)
//!
//! <div align="center">⁂</div>
//!
//! [^1_1]: https://…                                     (this turn's sources)
//! [^1_31]: projects.some.memory_tag                     (not a source)
//!
//! ---
//!
//! # your next question
//! …
//! ```
//!
//! [`parse`] turns that into the JSON the Perplexity thread adapter reads
//! (`{title, entries: [{query, answer}]}`), so a thread arrives as a
//! conversation of your questions and its answers instead of one document.
//! Footnote markers carry the turn number (`[^2_3]` is source 3 of turn 2), so
//! each turn's sources stay with that turn: cited ones are kept as a numbered
//! list under the answer, and the memory-style tags, hidden span, divider and
//! logo are dropped. Like the JSON adapter this is a pure function; the ingest
//! route owns persistence.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Value};

static FOOTNOTE_DEF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\^(\d+)_(\d+)\]:\s*(.*?)\s*$").unwrap());
static FOOTNOTE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\^\d+_(\d+)\]").unwrap());

const DIVIDER: &str = "<div align=\"center\">⁂</div>";
const HIDDEN_SPAN_START: &str = "<span style=\"display:none\">";
const MAX_TITLE_CHARS: usize = 100;

/// Whether `text` is a Perplexity Markdown export: its logo sits at the top,
/// or it carries the ⁂ divider together with numbered footnote sources.
pub fn looks_like(text: &str) -> bool {
    let head: String = text.chars().take(1500).collect();
    let logo = head.contains("<img") && head.contains("perplexity.ai");
    let footer = text.contains(DIVIDER) && text.lines().any(|l| FOOTNOTE_DEF.is_match(l));
    logo || footer
}

struct Turn<'a> {
    question: String,
    body: Vec<&'a str>,
}

/// Split into turns and build the thread JSON. None when no question heading
/// is found (the file is then kept as an ordinary document).
pub fn parse(text: &str) -> Option<Value> {
    let turns = split_turns(text);
    if turns.is_empty() {
        return None;
    }
    let entries: Vec<Value> = turns
        .iter()
        .map(|turn| {
            let answer = answer_text(&turn.body);
            let mut entry = json!({ "query": turn.question });
            if !answer.is_empty() {
                entry["answer"] = json!(answer);
            }
            entry
        })
        .collect();
    Some(json!({
        "title": title(&turns[0].question),
        "entries": entries,
    }))
}

fn title(question: &str) -> String {
    let first = question.lines().next().unwrap_or("").trim();
    let mut chars = first.chars();
    let head: String = chars.by_ref().take(MAX_TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{}…", head.trim_end())
    } else {
        head
    }
}

/// A fence line opens or closes a code block: at most three spaces of
/// indent, then three or more backticks or tildes.
fn fence_marker(line: &str) -> Option<char> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    (rest.chars().take_while(|c| *c == ch).count() >= 3).then_some(ch)
}

fn split_turns(text: &str) -> Vec<Turn<'_>> {
    let mut turns: Vec<Turn<'_>> = Vec::new();
    let mut fence: Option<char> = None;
    // The last non-blank line outside a code block, and whether anything but
    // the logo has come before the first question.
    let mut previous: Option<&str> = None;
    let mut preamble_is_logo_only = true;

    for line in text.lines() {
        if let Some(open) = fence {
            if fence_marker(line) == Some(open) {
                fence = None;
            }
            if let Some(turn) = turns.last_mut() {
                turn.body.push(line);
            }
            continue;
        }
        if let Some(marker) = fence_marker(line) {
            fence = Some(marker);
            match turns.last_mut() {
                Some(turn) => turn.body.push(line),
                None => preamble_is_logo_only = false,
            }
            previous = Some(line);
            continue;
        }

        let trimmed = line.trim();
        if let Some(question) = line.strip_prefix("# ") {
            // A question heading opens the thread, or follows a `---` divider.
            // A `# ` line inside an answer (rare, and never after a divider)
            // stays part of the answer.
            let opens_thread = turns.is_empty() && preamble_is_logo_only;
            let after_divider = !turns.is_empty() && previous.map(str::trim) == Some("---");
            if opens_thread || after_divider {
                if let Some(turn) = turns.last_mut() {
                    while turn.body.last().is_some_and(|l| l.trim().is_empty()) {
                        turn.body.pop();
                    }
                    if turn.body.last().map(|l| l.trim()) == Some("---") {
                        turn.body.pop(); // the divider belongs to neither turn
                    }
                }
                turns.push(Turn {
                    question: question.trim().to_string(),
                    body: Vec::new(),
                });
                previous = Some(line);
                continue;
            }
        }

        match turns.last_mut() {
            Some(turn) => turn.body.push(line),
            None => {
                if !trimmed.is_empty() && !trimmed.starts_with("<img") {
                    preamble_is_logo_only = false;
                }
            }
        }
        if !trimmed.is_empty() {
            previous = Some(line);
        }
    }
    turns
}

/// The answer of one turn: its text without the hidden span, divider and
/// footnote list, citation markers renumbered `[n]`, and the cited sources
/// listed underneath.
fn answer_text(body: &[&str]) -> String {
    let mut sources: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
    let mut kept: Vec<&str> = Vec::with_capacity(body.len());
    let mut fence: Option<char> = None;
    for line in body {
        if let Some(open) = fence {
            if fence_marker(line) == Some(open) {
                fence = None;
            }
            kept.push(line);
            continue;
        }
        if let Some(marker) = fence_marker(line) {
            fence = Some(marker);
            kept.push(line);
            continue;
        }
        let trimmed = line.trim();
        if trimmed == DIVIDER
            || (trimmed.starts_with(HIDDEN_SPAN_START) && trimmed.ends_with("</span>"))
        {
            continue;
        }
        if let Some(def) = FOOTNOTE_DEF.captures(trimmed) {
            let number: u32 = def[2].parse().unwrap_or(0);
            let value = def[3].replace("\\&", "&");
            // Only links are sources; the rest are Perplexity's memory-style
            // tags (`projects.some.thing`), which say nothing about the answer.
            if number > 0 && (value.starts_with("http://") || value.starts_with("https://")) {
                sources.insert(number, value);
            }
            continue;
        }
        kept.push(line);
    }

    let joined = kept.join("\n");
    let mut cited: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let renumbered = FOOTNOTE_REF.replace_all(&joined, |caps: &regex::Captures<'_>| {
        match caps[1].parse::<u32>() {
            Ok(n) if sources.contains_key(&n) => {
                cited.insert(n);
                format!("[{n}]")
            }
            _ => String::new(),
        }
    });

    let mut answer = renumbered.trim().to_string();
    if !cited.is_empty() {
        answer.push_str("\n\nSources:");
        for n in &cited {
            answer.push_str(&format!("\n[{n}] {}", sources[n]));
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-turn export in the shape Perplexity writes: logo, question,
    /// answer with a table and a code block that contains `# ` and `---`
    /// lines, citation markers, hidden span, divider and footnotes.
    const SINGLE: &str = r#"<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# What is needed for a useful agent swarm?

A useful swarm is **measurable**.[^1_1][^1_2]

## The layers

| Layer | Needs |
| :-- | :-- |
| Goal contract | success criteria |

```yaml
# a comment, not a question
---
task_id: demo
```

Add agents only when they help.[^1_1]

<span style="display:none">[^1_3]</span>

<div align="center">⁂</div>

[^1_1]: https://example.com/agents
[^1_2]: https://example.com/swarms?hl=en\&co=x
[^1_3]: https://example.com/unused
"#;

    /// Three turns, with a memory-style tag among the footnotes.
    const MULTI: &str = r#"<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png"/>

# First question: which systems compound?

Five systems compound.[^1_1]

<div align="center">⁂</div>

[^1_1]: https://example.com/one
[^1_2]: projects.some.memory_tag

---

# How to build one?

Start with a form.[^2_1] Then a sheet.[^2_2]

<span style="display:none">[^2_3]</span>

<div align="center">⁂</div>

[^2_1]: https://example.com/forms
[^2_2]: https://example.com/sheets
[^2_3]: https://example.com/hidden

---

# How to prevent manipulation?

Lock forecasts.[^3_2]

<div align="center">⁂</div>

[^3_1]: https://example.com/a
[^3_2]: https://example.com/b
"#;

    #[test]
    fn recognises_perplexity_markdown_and_nothing_else() {
        assert!(looks_like(SINGLE));
        assert!(looks_like(MULTI));
        assert!(!looks_like("# Notes\n\nJust a document.\n"));
        assert!(!looks_like("# A doc\n\n[^1_1]: https://example.com\n"));
    }

    #[test]
    fn a_single_turn_becomes_a_question_and_an_answer() {
        let thread = parse(SINGLE).unwrap();
        let entries = thread["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["query"],
            "What is needed for a useful agent swarm?"
        );
        let answer = entries[0]["answer"].as_str().unwrap();
        assert!(answer.starts_with("A useful swarm is **measurable**.[1][2]"));
        assert!(answer.contains("| Goal contract | success criteria |"));
        // Lines inside the code block are not read as a question or divider.
        assert!(answer.contains("# a comment, not a question\n---\ntask_id: demo"));
        assert!(answer.contains("Add agents only when they help.[1]"));
        // Cited sources are listed; the uncited one, the span, the divider and
        // the footnote lines are gone; `\&` is unescaped.
        assert!(answer.ends_with(
            "Sources:\n[1] https://example.com/agents\n[2] https://example.com/swarms?hl=en&co=x"
        ));
        assert!(!answer.contains("unused"));
        assert!(!answer.contains("display:none"));
        assert!(!answer.contains('⁂'));
        assert!(!answer.contains("[^"));
        assert!(!answer.contains("<img"));
    }

    #[test]
    fn each_turn_keeps_its_own_sources() {
        let thread = parse(MULTI).unwrap();
        let entries = thread["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1]["query"], "How to build one?");
        let second = entries[1]["answer"].as_str().unwrap();
        assert!(second.starts_with("Start with a form.[1] Then a sheet.[2]"));
        assert!(second.contains("[1] https://example.com/forms"));
        assert!(second.contains("[2] https://example.com/sheets"));
        assert!(!second.contains("hidden"));
        // Turn 3 cites only its second source: it keeps that number.
        let third = entries[2]["answer"].as_str().unwrap();
        assert!(third.contains("Lock forecasts.[2]"));
        assert!(third.ends_with("Sources:\n[2] https://example.com/b"));
        // The memory-style tag is not a source anywhere.
        assert!(!entries[0]["answer"]
            .as_str()
            .unwrap()
            .contains("memory_tag"));
        // No divider left at the end of a turn.
        assert!(!entries[0]["answer"].as_str().unwrap().contains("---"));
    }

    #[test]
    fn the_thread_feeds_the_perplexity_adapter() {
        let thread = parse(MULTI).unwrap();
        let out = super::super::perplexity::parse(&thread).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.messages.len(), 6);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(
            conv.title.as_deref(),
            Some("First question: which systems compound?")
        );
    }

    #[test]
    fn a_long_first_question_makes_a_short_title() {
        let long = "word ".repeat(80);
        let text =
            format!("<img src=\"https://r2cdn.perplexity.ai/x.png\"/>\n\n# {long}\n\nAnswer.\n");
        let thread = parse(&text).unwrap();
        let title = thread["title"].as_str().unwrap();
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
        // The question itself is kept whole.
        assert_eq!(thread["entries"][0]["query"], long.trim());
    }

    #[test]
    fn a_heading_inside_an_answer_stays_in_the_answer() {
        let text = "# Question\n\nIntro.\n\n# Not a question\n\nMore.\n";
        let thread = parse(text).unwrap();
        let entries = thread["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["answer"]
            .as_str()
            .unwrap()
            .contains("# Not a question"));
    }

    #[test]
    fn text_without_a_question_heading_is_not_a_thread() {
        assert!(parse("Just text.\n\n## Subheading\n").is_none());
        assert!(parse("Intro\n\n# Later heading\n").is_none());
    }
}
