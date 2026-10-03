//! A digest of one document: what it is about, and the few sentences that say
//! the most.
//!
//! The atomic-unit extractors answer "which sentences have a particular
//! shape?". A digest answers a different question: "what is this document
//! *for*?". It is classic extractive summarization, deterministic and fully
//! offline: every sentence is scored by how central its words are to the
//! document, with a lift for decision and deadline language and for the
//! opening of a section, then a handful are picked while skipping near
//! repeats. Nothing is invented: each key point is a sentence from the file.
//!
//! Pure (no I/O, no clock, no model): the same text always gives the same
//! digest, and each part is tested on its own. An optional local model can
//! reword the result afterwards (`ollama.rs`); this stands without it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::worth;

/// One section of a document, in reading order.
#[derive(Debug, Clone, Copy)]
pub struct Section<'a> {
    pub seq: i32,
    pub heading: Option<&'a str>,
    pub text: &'a str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyPoint {
    pub text: String,
    /// 0..1, relative to the best sentence in the document.
    pub score: f32,
    /// The section it came from.
    pub segment_seq: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stats {
    pub words: usize,
    pub sentences: usize,
    pub sections: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Digest {
    /// The top key points, joined: a short paragraph in the file's own words.
    pub summary: String,
    /// In reading order.
    pub key_points: Vec<KeyPoint>,
    /// Recurring phrases, most characteristic first.
    pub topics: Vec<String>,
    pub outline: Vec<Heading>,
    pub stats: Stats,
}

impl Digest {
    /// A document with too little prose to say anything about.
    pub fn is_thin(&self) -> bool {
        self.key_points.is_empty()
    }
}

/// Most text read for one digest; a longer document is digested from its
/// start (the keys of a very long file are rarely at the end).
const MAX_CHARS: usize = 300_000;
const MIN_SENTENCE_WORDS: usize = 6;
const MAX_SENTENCE_WORDS: usize = 60;
const MIN_CANDIDATES: usize = 3;
const MAX_KEY_POINTS: usize = 8;
const SUMMARY_POINTS: usize = 3;
const SUMMARY_MAX_CHARS: usize = 700;
const MAX_TOPICS: usize = 8;
const MAX_OUTLINE: usize = 24;
/// How strongly a sentence that overlaps an already chosen one is penalized,
/// and the overlap past which it is skipped outright.
const REDUNDANCY_WEIGHT: f32 = 0.8;
const NEAR_REPEAT: f32 = 0.6;
/// What a word that appears once is worth, next to a recurring word's ln(1+n).
const ONE_OFF_WEIGHT: f32 = 0.35;
/// The lift per cue in a sentence that decides, plans, warns or concludes.
const CUE_LIFT: f32 = 0.45;

/// Words too common to say what a sentence is about.
const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "all",
    "any",
    "can",
    "had",
    "her",
    "was",
    "one",
    "our",
    "out",
    "has",
    "have",
    "him",
    "his",
    "how",
    "its",
    "may",
    "new",
    "now",
    "old",
    "see",
    "two",
    "who",
    "did",
    "get",
    "got",
    "let",
    "put",
    "say",
    "she",
    "too",
    "use",
    "with",
    "this",
    "that",
    "from",
    "they",
    "will",
    "would",
    "there",
    "their",
    "what",
    "about",
    "which",
    "when",
    "make",
    "like",
    "time",
    "just",
    "him",
    "know",
    "take",
    "into",
    "year",
    "your",
    "some",
    "could",
    "them",
    "than",
    "then",
    "other",
    "only",
    "over",
    "such",
    "also",
    "after",
    "these",
    "those",
    "where",
    "while",
    "being",
    "been",
    "were",
    "does",
    "done",
    "each",
    "every",
    "much",
    "many",
    "most",
    "more",
    "very",
    "here",
    "should",
    "because",
    "before",
    "between",
    "through",
    "during",
    "under",
    "again",
    "still",
    "already",
    "always",
    "often",
    "since",
    "until",
    "using",
    "used",
    "uses",
    "including",
    "include",
    "includes",
    "within",
    "without",
    "across",
    "around",
    "against",
    "among",
    "another",
    "however",
    "therefore",
    "thus",
    "per",
    "via",
    "etc",
    "are",
    "is",
    "am",
    "be",
    "it",
    "of",
    "in",
    "on",
    "at",
    "to",
    "as",
    "by",
    "an",
    "or",
    "if",
    "so",
    "we",
    "us",
    "he",
    "me",
    "my",
    "do",
    "no",
    "up",
    "i",
];

/// Phrases that mark a sentence that settles something: decisions, plans,
/// deadlines, risks, conclusions.
const CUES: &[&str] = &[
    "decided",
    "decision",
    "agreed",
    "we will",
    "must",
    "should",
    "need to",
    "needs to",
    "plan to",
    "goal",
    "deadline",
    "due ",
    "budget",
    "risk",
    "blocked",
    "blocker",
    "conclusion",
    "in summary",
    "to summarize",
    "recommend",
    "next step",
    "action item",
    "open question",
    "important",
    "key ",
    "priority",
    "because",
    "therefore",
    "however",
    "result",
    "found that",
    "launch",
    "migrat",
];

fn is_stopword(w: &str) -> bool {
    STOPWORDS.contains(&w)
}

/// The word stripped of a plural so "backups" and "backup" count together.
fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    if w.len() > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if w.len() > 4 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") {
        return w[..w.len() - 1].to_string();
    }
    w
}

/// The words of a sentence that carry meaning, as stems.
fn terms(sentence: &str) -> Vec<String> {
    sentence
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| w.trim_matches('\''))
        .filter(|w| w.chars().count() >= 3 && w.chars().any(char::is_alphabetic))
        .map(str::to_lowercase)
        .filter(|w| !is_stopword(w))
        .map(|w| stem(&w))
        .collect()
}

fn word_count(sentence: &str) -> usize {
    sentence.split_whitespace().count()
}

/// Drop list markers, quote marks and heading hashes from one line.
fn strip_marker(line: &str) -> &str {
    let t = line.trim_start();
    let t = t.trim_start_matches(['>', '#']).trim_start();
    for marker in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    // "1. " / "12) "
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &t[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    t
}

/// Remove inline Markdown (emphasis, code ticks, link syntax) from a sentence.
fn plain(sentence: &str) -> String {
    let mut out = String::with_capacity(sentence.len());
    let mut chars = sentence.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' | '`' | '~' => {}
            '[' => {
                // [text](url) -> text
                let mut label = String::new();
                for n in chars.by_ref() {
                    if n == ']' {
                        break;
                    }
                    label.push(n);
                }
                if chars.peek() == Some(&'(') {
                    for n in chars.by_ref() {
                        if n == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&label);
            }
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Heading level of a Markdown heading line, with its text.
fn heading_line(line: &str) -> Option<(u8, String)> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let text = plain(t[hashes..].trim());
        if !text.is_empty() && t[hashes..].starts_with(' ') {
            return Some((hashes as u8, text));
        }
    }
    None
}

/// One candidate sentence, with where it came from.
struct Candidate {
    text: String,
    seq: i32,
    /// First sentence of its section.
    opens_section: bool,
    terms: Vec<String>,
}

/// Split one section's text into sentences, leaving out code, tables and
/// headings (the latter are collected separately).
fn section_sentences(text: &str, outline: &mut Vec<Heading>) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut paragraph = String::new();

    let flush = |paragraph: &mut String, out: &mut Vec<String>| {
        if paragraph.trim().is_empty() {
            paragraph.clear();
            return;
        }
        let text = plain(paragraph);
        paragraph.clear();
        let mut start = 0usize;
        let bytes = text.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            let end_of_sentence = matches!(b, b'.' | b'!' | b'?')
                && bytes
                    .get(i + 1)
                    .copied()
                    .unwrap_or(b' ')
                    .is_ascii_whitespace();
            if end_of_sentence {
                let s = text[start..=i].trim();
                // "e.g." and "3.5" don't end a sentence: only split when what
                // follows starts a new one (an upper-case letter or a digit).
                let next = text[i + 1..].trim_start().chars().next();
                if next.is_none() || next.is_some_and(|c| c.is_uppercase() || c.is_ascii_digit()) {
                    if !s.is_empty() {
                        out.push(s.to_string());
                    }
                    start = i + 1;
                }
            }
        }
        let rest = text[start..].trim();
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush(&mut paragraph, &mut out);
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some((level, heading)) = heading_line(trimmed) {
            flush(&mut paragraph, &mut out);
            if outline.len() < MAX_OUTLINE {
                outline.push(Heading {
                    level,
                    text: heading,
                });
            }
            continue;
        }
        // Table rows and rules are data, not sentences.
        if trimmed.is_empty()
            || trimmed.matches('|').count() >= 2
            || trimmed
                .chars()
                .all(|c| matches!(c, '-' | '=' | '_' | '*' | ' '))
        {
            flush(&mut paragraph, &mut out);
            continue;
        }
        let is_item = trimmed.starts_with(['-', '*', '+', '•'])
            || trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit() && strip_marker(trimmed) != trimmed);
        if is_item {
            // Each list item stands alone.
            flush(&mut paragraph, &mut out);
            let item = strip_marker(trimmed);
            let item = item.trim_end_matches([';', ',']);
            if !item.is_empty() {
                out.push(plain(item));
            }
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(strip_marker(trimmed));
    }
    flush(&mut paragraph, &mut out);
    out
}

fn is_candidate(sentence: &str) -> bool {
    let words = word_count(sentence);
    if !(MIN_SENTENCE_WORDS..=MAX_SENTENCE_WORDS).contains(&words) {
        return false;
    }
    if sentence.ends_with('?') {
        return false;
    }
    // Mostly capitals: a title or a label, not a statement.
    let letters = sentence.chars().filter(|c| c.is_alphabetic()).count();
    let upper = sentence.chars().filter(|c| c.is_uppercase()).count();
    if letters > 0 && upper * 10 > letters * 7 {
        return false;
    }
    worth::statement_worth_keeping(sentence)
}

fn jaccard(a: &BTreeSet<&str>, b: &BTreeSet<&str>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    inter / union
}

/// How many distinct decision/plan/risk cues a sentence carries.
fn cue_hits(sentence: &str) -> usize {
    let lower = sentence.to_lowercase();
    CUES.iter().filter(|c| lower.contains(*c)).count()
}

/// Digest a document from its sections, in reading order.
pub fn build(sections: &[Section]) -> Digest {
    let mut outline: Vec<Heading> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut words = 0usize;
    let mut sentences_seen = 0usize;
    let mut budget = MAX_CHARS;

    for section in sections {
        if budget == 0 {
            break;
        }
        let text: &str = if section.text.len() > budget {
            // Cut on a character boundary.
            let mut end = budget;
            while !section.text.is_char_boundary(end) {
                end -= 1;
            }
            &section.text[..end]
        } else {
            section.text
        };
        budget = budget.saturating_sub(text.len());
        if let Some(h) = section.heading.map(plain).filter(|h| !h.is_empty()) {
            if outline.len() < MAX_OUTLINE && !outline.iter().any(|o| o.text == h) {
                outline.push(Heading { level: 2, text: h });
            }
        }
        // Code and data sections aren't read for sentences.
        if !worth::chunk_is_prose(text) {
            continue;
        }
        words += word_count(text);
        let mut first = true;
        for s in section_sentences(text, &mut outline) {
            sentences_seen += 1;
            if !is_candidate(&s) {
                continue;
            }
            candidates.push(Candidate {
                terms: terms(&s),
                text: s,
                seq: section.seq,
                opens_section: first,
            });
            first = false;
        }
    }

    let stats = Stats {
        words,
        sentences: sentences_seen,
        sections: sections.len(),
    };
    if candidates.len() < MIN_CANDIDATES {
        return Digest {
            outline,
            stats,
            ..Digest::default()
        };
    }

    // How often each term occurs across the document; heading words count
    // double, because they say what the document is about.
    let mut tf: BTreeMap<&str, f32> = BTreeMap::new();
    for c in &candidates {
        for t in &c.terms {
            *tf.entry(t.as_str()).or_insert(0.0) += 1.0;
        }
    }
    let heading_terms: Vec<String> = outline.iter().flat_map(|h| terms(&h.text)).collect();
    for t in &heading_terms {
        if let Some(v) = tf.get_mut(t.as_str()) {
            *v += 2.0;
        }
    }
    let heading_set: BTreeSet<&str> = heading_terms.iter().map(String::as_str).collect();

    let mut scored: Vec<(f32, usize)> = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        let unique: BTreeSet<&str> = c.terms.iter().map(String::as_str).collect();
        if unique.is_empty() {
            continue;
        }
        // Words that recur matter most, but a one-off word still counts for a
        // little, so a short document with no repeats is still ranked.
        // Frequency is damped (log) so the most repeated word doesn't drown
        // out everything else. Normalized by length so long sentences don't
        // win just for being long.
        let centrality: f32 = unique
            .iter()
            .map(|t| {
                let n = tf.get(t).copied().unwrap_or(0.0);
                if n > 1.0 {
                    (1.0 + n).ln()
                } else {
                    ONE_OFF_WEIGHT
                }
            })
            .sum::<f32>()
            / (unique.len() as f32).sqrt();
        let mut score = centrality;
        let overlap =
            unique.iter().filter(|t| heading_set.contains(*t)).count() as f32 / unique.len() as f32;
        score *= 1.0 + overlap * 0.5;
        // Each cue lifts the sentence, up to three: "we decided ... because
        // ..." says more than a sentence with one.
        score *= 1.0 + CUE_LIFT * cue_hits(&c.text).min(3) as f32;
        if c.opens_section {
            score *= 1.1;
        }
        if i == 0 {
            score *= 1.15;
        }
        scored.push((score, i));
    }
    let best = scored.iter().map(|s| s.0).fold(0.0f32, f32::max);
    if best <= 0.0 {
        return Digest {
            outline,
            stats,
            ..Digest::default()
        };
    }
    for s in &mut scored {
        s.0 /= best;
    }

    // Greedy pick, skipping sentences that repeat one already chosen.
    let target = (words / 150)
        .max(candidates.len() / 3 + 1)
        .clamp(MIN_CANDIDATES, MAX_KEY_POINTS);
    let mut chosen: Vec<(f32, usize)> = Vec::new();
    let mut remaining = scored.clone();
    while chosen.len() < target && !remaining.is_empty() {
        let mut best_pos = None;
        let mut best_value = f32::MIN;
        for (pos, &(score, i)) in remaining.iter().enumerate() {
            let mine: BTreeSet<&str> = candidates[i].terms.iter().map(String::as_str).collect();
            let overlap = chosen
                .iter()
                .map(|&(_, j)| {
                    let theirs: BTreeSet<&str> =
                        candidates[j].terms.iter().map(String::as_str).collect();
                    jaccard(&mine, &theirs)
                })
                .fold(0.0f32, f32::max);
            if overlap >= NEAR_REPEAT {
                continue; // says what an earlier point already says
            }
            let value = score - REDUNDANCY_WEIGHT * overlap;
            // Ties go to the earlier sentence, so the result never depends on
            // iteration order.
            if value > best_value + 1e-6 || (best_value - value).abs() <= 1e-6 && best_pos.is_none()
            {
                best_value = value;
                best_pos = Some(pos);
            }
        }
        let Some(pos) = best_pos else { break };
        // A weak remainder isn't worth adding once there are enough points.
        if chosen.len() >= MIN_CANDIDATES && best_value < 0.2 {
            break;
        }
        chosen.push(remaining.remove(pos));
    }

    let mut by_score = chosen.clone();
    by_score.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut summary_ids: Vec<usize> = by_score
        .iter()
        .take(SUMMARY_POINTS)
        .map(|&(_, i)| i)
        .collect();
    summary_ids.sort_unstable();
    let mut summary = String::new();
    for i in summary_ids {
        let s = &candidates[i].text;
        if summary.len() + s.len() + 1 > SUMMARY_MAX_CHARS && !summary.is_empty() {
            break;
        }
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(s);
    }

    chosen.sort_by_key(|&(_, i)| i);
    let key_points = chosen
        .iter()
        .map(|&(score, i)| KeyPoint {
            text: candidates[i].text.clone(),
            score: (score * 100.0).round() / 100.0,
            segment_seq: candidates[i].seq,
        })
        .collect();

    Digest {
        summary,
        key_points,
        topics: topics(&candidates),
        outline,
        stats,
    }
}

/// Phrases that recur: runs of meaningful words between stop words, such as
/// "backup target" or "latency budget".
fn topics(candidates: &[Candidate]) -> Vec<String> {
    // phrase (lower-cased stems) -> (count, first surface form)
    let mut counts: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for c in candidates {
        let words: Vec<&str> = c
            .text
            .split(|ch: char| !ch.is_alphanumeric() && ch != '\'' && ch != '-')
            .filter(|w| !w.is_empty())
            .collect();
        let mut run: Vec<&str> = Vec::new();
        let mut runs: Vec<Vec<&str>> = Vec::new();
        for w in words {
            let lower = w.to_lowercase();
            let meaningful = lower.chars().count() >= 2
                && lower.chars().any(char::is_alphabetic)
                && !is_stopword(&lower);
            if meaningful {
                run.push(w);
            } else if !run.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        for run in runs {
            for n in 1..=3usize.min(run.len()) {
                for window in run.windows(n) {
                    let key = window.iter().map(|w| stem(w)).collect::<Vec<_>>().join(" ");
                    let surface = window.join(" ");
                    let entry = counts.entry(key).or_insert((0, surface));
                    entry.0 += 1;
                }
            }
        }
    }
    let mut ranked: Vec<(f32, String, String)> = counts
        .into_iter()
        .filter_map(|(key, (count, surface))| {
            let n = key.split(' ').count();
            let enough = match n {
                1 => count >= 3,
                _ => count >= 2,
            };
            if !enough || !worth::entity_name_worth_keeping(&surface) {
                return None;
            }
            let score = count as f32 * (1.0 + 0.6 * (n as f32 - 1.0));
            Some((score, key, surface))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

    // The longer phrase wins: "backup" adds nothing next to "backup target",
    // and "backup target" replaces a "backup" chosen before it.
    let contains = |long: &str, short: &str| format!(" {long} ").contains(&format!(" {short} "));
    let mut out: Vec<(String, String)> = Vec::new();
    for (_, key, surface) in ranked {
        if out.iter().any(|(k, _)| contains(k, &key)) {
            continue;
        }
        out.retain(|(k, _)| !contains(&key, k));
        out.push((key, surface));
        if out.len() == MAX_TOPICS {
            break;
        }
    }
    out.into_iter().map(|(_, surface)| surface).collect()
}

/// The share of `text`'s meaningful words that also appear in `source`. A
/// model asked to reword a document may add things the document never said;
/// text that shares few words with its source is not trusted.
pub fn grounding(text: &str, source: &str) -> f32 {
    let known: BTreeSet<String> = terms(source).into_iter().collect();
    let mine = terms(text);
    if mine.is_empty() {
        return 0.0;
    }
    mine.iter().filter(|t| known.contains(*t)).count() as f32 / mine.len() as f32
}

/// The items that are grounded in `source` well enough to keep.
pub fn keep_grounded(items: Vec<String>, source: &str, minimum: f32) -> Vec<String> {
    items
        .into_iter()
        .filter(|item| grounding(item, source) >= minimum)
        .collect()
}

/// What a model is shown to reword: the outline and the key sentences, not
/// the whole file, so even a small model with a short context can do it.
pub fn material_for_model(digest: &Digest) -> String {
    let mut out = String::new();
    if !digest.outline.is_empty() {
        let headings: Vec<&str> = digest.outline.iter().map(|h| h.text.as_str()).collect();
        out.push_str("Sections: ");
        out.push_str(&headings.join(" > "));
        out.push_str("\n\n");
    }
    out.push_str("Key sentences from the document:\n");
    for point in &digest.key_points {
        out.push_str("- ");
        out.push_str(&point.text);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Digest {
        build(&[Section {
            seq: 0,
            heading: None,
            text,
        }])
    }

    const PLAN: &str = "\
# Backup migration plan

The backup migration moves the nightly backup target from the old server to Hetzner. \
We decided on Hetzner CX22 for the backup target because the monthly cost is lower. \
The migration must finish before the 2026-03-01 deadline, otherwise the old contract renews. \
Thanks everyone for joining the call today. \
The risk is that the first full backup to the new target takes longer than the nightly window. \
Please see the attached spreadsheet for details. \
The backup target will keep thirty days of nightly backups and one monthly archive.

## Next steps

- Dana owns the migration and reports progress every Friday.
- Test a full restore from the new backup target before the deadline.
";

    #[test]
    fn the_sentences_that_settle_things_are_chosen() {
        let d = doc(PLAN);
        assert!(!d.is_thin());
        let text: Vec<&str> = d.key_points.iter().map(|k| k.text.as_str()).collect();
        let joined = text.join(" | ");
        assert!(joined.contains("decided on Hetzner CX22"), "{joined}");
        assert!(joined.contains("2026-03-01 deadline"), "{joined}");
        assert!(
            !joined.contains("Thanks everyone"),
            "pleasantries are not key points: {joined}"
        );
        assert!(
            !joined.contains("attached spreadsheet"),
            "pointers are not key points: {joined}"
        );
        assert!(d.summary.len() <= SUMMARY_MAX_CHARS + 200);
        assert!(!d.summary.is_empty());
    }

    #[test]
    fn key_points_come_back_in_reading_order_and_are_real_sentences() {
        let d = doc(PLAN);
        let positions: Vec<usize> = d
            .key_points
            .iter()
            .map(|k| PLAN.find(&k.text[..20.min(k.text.len())]).unwrap())
            .collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(positions, sorted, "reading order");
        for k in &d.key_points {
            assert!(k.score > 0.0 && k.score <= 1.0, "{k:?}");
        }
    }

    #[test]
    fn near_repeats_are_not_chosen_twice() {
        let text = "\
The migration to the new backup target is the main goal of this quarter. \
The migration to the new backup target is the main goal of the quarter. \
The restore test checks that every nightly backup can be read back in full. \
Budget approval for the new storage contract is still waiting on finance. \
The old server will be retired after the migration is finished and verified.";
        let d = doc(text);
        let repeats = d
            .key_points
            .iter()
            .filter(|k| k.text.contains("main goal"))
            .count();
        assert_eq!(repeats, 1, "{:?}", d.key_points);
    }

    #[test]
    fn code_math_and_tables_say_nothing() {
        let text = "\
```rust
fn main() { println!(\"the budget is 75 and the deadline is near\"); }
```
2 + 1 is 3 is equal to = 3. x = 4 so y = 5.
| name | cost |
| a | 12 |
12 4.5 77 0.31 9 15 22 1.5 8 40 31 7 12 4.5 77 0.31 9 15 22 1.5 8 40 31 7
";
        let d = doc(text);
        assert!(d.is_thin(), "{d:?}");
        assert!(d.summary.is_empty());
    }

    #[test]
    fn a_thin_document_has_no_key_points_but_keeps_its_outline() {
        let d = doc("# Title\n\nHello there.\n\nSee you.\n");
        assert!(d.is_thin());
        assert_eq!(
            d.outline,
            vec![Heading {
                level: 1,
                text: "Title".into()
            }]
        );
    }

    #[test]
    fn topics_are_phrases_that_recur() {
        let d = doc(PLAN);
        let topics = d.topics.join(" | ").to_lowercase();
        assert!(topics.contains("backup target"), "{topics}");
        assert!(
            !d.topics.iter().any(|t| {
                matches!(t.to_lowercase().as_str(), "the" | "and" | "will" | "thanks")
            }),
            "{topics}"
        );
    }

    #[test]
    fn the_outline_follows_the_headings() {
        let d = doc(PLAN);
        assert_eq!(
            d.outline,
            vec![
                Heading {
                    level: 1,
                    text: "Backup migration plan".into()
                },
                Heading {
                    level: 2,
                    text: "Next steps".into()
                },
            ]
        );
    }

    #[test]
    fn the_same_text_always_gives_the_same_digest() {
        let a = doc(PLAN);
        let b = doc(PLAN);
        assert_eq!(a, b);
    }

    #[test]
    fn sections_keep_their_own_numbers() {
        let sections = [
            Section {
                seq: 4,
                heading: Some("Budget"),
                text: "The budget for the backup migration is approved by finance this week. \
                       The budget covers the new storage contract for the whole year. \
                       Any budget change needs approval from the finance lead first.",
            },
            Section {
                seq: 5,
                heading: Some("Risks"),
                text: "The main risk is that the first backup runs past the nightly window. \
                       Another risk is that the restore test finds unreadable archives.",
            },
        ];
        let d = build(&sections);
        assert!(!d.is_thin());
        assert!(d
            .key_points
            .iter()
            .all(|k| k.segment_seq == 4 || k.segment_seq == 5));
        assert!(d.key_points.iter().any(|k| k.segment_seq == 4));
        assert!(d.outline.iter().any(|h| h.text == "Budget"));
    }

    #[test]
    fn abbreviations_and_decimals_do_not_split_sentences() {
        let text = "\
The monthly cost is 5.25 euros for the CX22, e.g. the smallest shared instance on offer. \
The decision was made on the 2026-03-01 call with the whole infrastructure team present. \
The restore test must pass before the old contract is cancelled for good.";
        let d = doc(text);
        assert!(
            d.key_points
                .iter()
                .any(|k| k.text.contains("5.25 euros")
                    && k.text.contains("shared instance on offer")),
            "{:?}",
            d.key_points
        );
    }

    #[test]
    fn model_text_must_be_grounded_in_the_document() {
        let d = doc(PLAN);
        let material = material_for_model(&d);
        assert!(material.contains("Backup migration plan"));
        assert!(material.contains("decided on Hetzner CX22"));
        let kept = keep_grounded(
            vec![
                "The team chose Hetzner CX22 for the backup target to lower the monthly cost."
                    .to_string(),
                "The company will also open an office in Berlin and hire twelve engineers."
                    .to_string(),
            ],
            &material,
            0.5,
        );
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(kept[0].contains("Hetzner"));
        assert_eq!(grounding("", &material), 0.0);
    }
}
