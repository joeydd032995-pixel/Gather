//! A broader reader for ordinary prose, used when no reading model is set up.
//!
//! `rules.rs` is precise and narrow: it only takes sentences in a few fixed
//! shapes ("We decided on X", "My rent is 1,150 a month"), which ordinary
//! notes and documents rarely hold. On real prose it finds a few percent of
//! what a person would call a statement, and everything downstream (the
//! knowledge graph, contradictions, entity merges, the review tray) has
//! nothing to work on.
//!
//! This reader keeps any plain sentence that says something specific: it
//! carries a figure, a date, a name, or a decision, report or plan. It is
//! offline, costs no memory worth speaking of, and is deliberately modest:
//! units start at a confidence below the rules' (the store raises it when the
//! subject is a nameable thing), so the uncertain ones are parked in the
//! review tray for a person's verdict instead of being trusted, and their
//! verdicts feed the same threshold tuning as everything else.
//!
//! It never touches a sentence the rules already took, and it is only run
//! when there is no reading model: a model's own statements are better than
//! sentences copied whole.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::json;

use super::rules::ExtractedUnit;
use super::worth;

/// Below the rules' 0.6: a sentence kept for what it contains, not for the
/// shape it matched. The store adds 0.1 when the subject is a nameable thing,
/// which lifts the better ones past the review threshold.
const BASE_CONFIDENCE: f32 = 0.45;

const MIN_SENTENCE_CHARS: usize = 25;
const MAX_SENTENCE_CHARS: usize = 300;
const MIN_WORDS: usize = 5;

/// Words that make a sentence a request, a greeting or a sign-off rather than
/// a statement.
const NON_STATEMENT_OPENERS: &[&str] = &[
    "please",
    "let",
    "remember",
    "thanks",
    "thank",
    "hi",
    "hello",
    "hey",
    "dear",
    "hope",
    "hoping",
    "kind",
    "regards",
    "best",
    "cheers",
    "sincerely",
    "speak",
    "talk",
    "look",
    "see",
    "note",
    "step",
    "do",
    "don't",
    "don’t",
    "ensure",
    "make",
    "click",
    "open",
    "try",
    "feel",
    "send",
    "forward",
    "call",
    "check",
    "apologies",
    "congratulations",
    "welcome",
    "sorry",
    "yes",
    "no",
    "ok",
    "okay",
    "sounds",
    "great",
    "good",
    "nice",
    "lovely",
    "also",
    "anyway",
    "regards",
    "yours",
    "warm",
    "bye",
    "goodbye",
    "sent",
];

/// Mail-header labels ("From: Dana", "Re: budget"). Without the colon these
/// are ordinary words: "From 2022 to 2024 attendance rose from 80 to 140".
const HEADER_LABELS: &[&str] = &["cc", "re", "fwd", "subject", "from", "to"];

/// Words that open a noun phrase.
const DETERMINERS: &[&str] = &[
    "the", "a", "an", "this", "that", "these", "those", "my", "our", "your", "their", "his", "her",
    "its", "some", "each", "every", "all", "any", "no",
];

/// Auxiliaries, copulas and modals: the first of these ends a subject phrase.
const AUXILIARIES: &[&str] = &[
    "is", "are", "was", "were", "am", "be", "been", "has", "have", "had", "will", "would", "can",
    "could", "may", "might", "must", "should", "shall", "does", "did", "do", "isn't", "wasn't",
    "doesn't", "didn't", "won't", "can't", "cannot", "hasn't", "haven't", "aren't", "weren't",
];

const DECISION_VERBS: &[&str] = &[
    "decided",
    "decides",
    "agreed",
    "agrees",
    "voted",
    "votes",
    "approved",
    "approves",
    "chose",
    "chosen",
    "choose",
    "chooses",
    "picked",
    "selected",
    "resolved",
    "committed",
    "settled",
    "opted",
    "promised",
    "undertook",
];

const REPORT_VERBS: &[&str] = &[
    "said",
    "says",
    "reported",
    "reports",
    "found",
    "finds",
    "concluded",
    "concludes",
    "argue",
    "argues",
    "argued",
    "suggested",
    "suggests",
    "recommend",
    "recommends",
    "recommended",
    "noted",
    "notes",
    "announced",
    "announces",
    "confirmed",
    "confirms",
    "expects",
    "expected",
    "explained",
    "explains",
    "stated",
    "states",
    "claimed",
    "claims",
    "showed",
    "shows",
    "warned",
    "warns",
    "wants",
    "wanted",
    "asked",
    "told",
    "thinks",
    "believes",
    "estimates",
    "estimated",
    "observed",
    "writes",
    "wrote",
    "proposed",
    "proposes",
    "requires",
    "required",
    "asks",
];

const PREFERENCE_VERBS: &[&str] = &[
    "prefer",
    "prefers",
    "preferred",
    "like",
    "likes",
    "love",
    "loves",
    "hate",
    "hates",
    "enjoy",
    "enjoys",
    "favour",
    "favours",
    "favor",
    "favors",
];

/// Third-person and past-tense verbs common in notes, so a subject phrase
/// ends at the verb ("The guesthouse charges ...", "Hartley pays ..."). It
/// does not need to be complete: a subject that runs on is cut at four words.
const VERBS: &[&str] = &[
    "beat",
    "beats",
    "won",
    "wins",
    "lost",
    "loses",
    "scored",
    "scores",
    "lifts",
    "lifted",
    "defeated",
    "drew",
    "draws",
    "played",
    "plays",
    "led",
    "leads",
    "grew",
    "grows",
    "sank",
    "saw",
    "sees",
    "heard",
    "hears",
    "brought",
    "brings",
    "caught",
    "taught",
    "teaches",
    "thought",
    "knew",
    "knows",
    "felt",
    "feels",
    "retires",
    "retired",
    "founded",
    "founds",
    "employs",
    "employed",
    "studied",
    "studies",
    "graduated",
    "rotated",
    "rotates",
    "exposed",
    "migrated",
    "migrates",
    "stood",
    "stands",
    "sits",
    "sat",
    "ate",
    "eats",
    "drank",
    "wore",
    "wears",
    "carried",
    "carries",
    "delivered",
    "delivers",
    "replaced",
    "replaces",
    "released",
    "releases",
    "published",
    "publishes",
    "completed",
    "completes",
    "finished",
    "finishes",
    "added",
    "adds",
    "removed",
    "removes",
    "changed",
    "changes",
    "created",
    "creates",
    "deleted",
    "deletes",
    "updated",
    "updates",
    "installed",
    "installs",
    "stopped",
    "stops",
    "failed",
    "fails",
    "passed",
    "passes",
    "checked",
    "checks",
    "cancelled",
    "cancels",
    "billed",
    "bills",
    "rented",
    "rents",
    "borrowed",
    "lent",
    "lends",
    "owns",
    "owned",
    "manages",
    "managed",
    "trains",
    "trained",
    "visited",
    "visits",
    "called",
    "calls",
    "emailed",
    "emails",
    "reviewed",
    "reviews",
    "tested",
    "tests",
    "stores",
    "stored",
    "charges",
    "charged",
    "costs",
    "cost",
    "holds",
    "held",
    "takes",
    "took",
    "covers",
    "covered",
    "pays",
    "paid",
    "listens",
    "ran",
    "runs",
    "absorbs",
    "fixes",
    "fixed",
    "arrives",
    "arrived",
    "arrive",
    "opens",
    "opened",
    "closes",
    "closed",
    "starts",
    "started",
    "begins",
    "began",
    "ends",
    "ended",
    "sends",
    "sent",
    "books",
    "booked",
    "lives",
    "lived",
    "works",
    "worked",
    "needs",
    "needed",
    "keeps",
    "kept",
    "uses",
    "used",
    "dropped",
    "drops",
    "ships",
    "shipped",
    "launched",
    "launches",
    "moved",
    "moves",
    "planted",
    "plants",
    "landed",
    "lands",
    "went",
    "goes",
    "came",
    "comes",
    "left",
    "leaves",
    "met",
    "meets",
    "bought",
    "buys",
    "sold",
    "sells",
    "built",
    "builds",
    "made",
    "makes",
    "gave",
    "gives",
    "got",
    "gets",
    "set",
    "sets",
    "ordered",
    "orders",
    "improved",
    "improves",
    "rose",
    "rises",
    "fell",
    "falls",
    "increased",
    "increases",
    "decreased",
    "decreases",
    "raised",
    "raises",
    "reduced",
    "reduces",
    "signed",
    "signs",
    "joined",
    "joins",
    "turns",
    "turned",
    "became",
    "becomes",
    "reached",
    "reaches",
    "survey",
    "surveyed",
    "ran",
    "dropped",
    "owes",
    "owed",
    "earns",
    "earned",
    "spent",
    "spends",
    "sold",
    "wrote",
    "write",
    "show",
    "shown",
    "accrue",
    "accrues",
    "accrued",
    "requires",
    "expires",
    "expired",
    "renews",
    "renewed",
    "offers",
    "offered",
    "includes",
    "included",
    "contains",
    "contained",
    "supports",
    "supported",
    "dropped",
    "weighs",
    "weighed",
    "measures",
    "measured",
    "lasts",
    "lasted",
    "runs",
    "listens",
    "applies",
    "applied",
    "takes",
    "tracks",
    "tracked",
    "recommend",
    "dropped",
    "approve",
    "approved",
    "voted",
    "chose",
];

const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
];

const TIME_WORDS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "tomorrow",
    "yesterday",
    "tonight",
    "weekend",
];

const NUMBER_WORDS: &[&str] = &[
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "hundred",
    "thousand",
    "million",
    "billion",
    "zero",
    "dozen",
    "half",
    "third",
    "quarter",
    "twice",
    "double",
    "triple",
    "eighteen",
    "fifteen",
    "fourteen",
    "thirteen",
    "sixteen",
    "seventeen",
    "nineteen",
];

const PRONOUN_SUBJECTS: &[&str] = &[
    "it",
    "this",
    "that",
    "they",
    "he",
    "she",
    "there",
    "these",
    "those",
    "which",
    "who",
    "what",
    "one",
    "some",
    "someone",
    "something",
    "everyone",
    "everything",
    "nobody",
    "nothing",
    "both",
];

const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "st", "vs", "etc", "e.g", "i.e", "no", "inc", "ltd", "jr",
    "sr", "approx", "ca", "fig", "eg", "ie", "cf", "co", "corp", "vol", "pp", "dept", "est",
];

struct Patterns {
    digit: Regex,
    iso_date: Regex,
    day_month: Regex,
    clock: Regex,
    number_tail: Regex,
    first_person: Regex,
    list_marker: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        digit: Regex::new(r"\d").unwrap(),
        iso_date: Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").unwrap(),
        day_month: Regex::new(
            r"(?i)\b(?:\d{1,2}(?:st|nd|rd|th)?\s+(?:of\s+)?(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*|(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\s+\d{1,2}(?:st|nd|rd|th)?)\b",
        )
        .unwrap(),
        clock: Regex::new(r"(?i)\b\d{1,2}:\d{2}\b|\b\d{1,2}\s?(?:am|pm)\b").unwrap(),
        // A figure straight after the verb: "is 12,000 dollars", "holds 300
        // people", "is capped at $18,000", "rose to 2.9%".
        number_tail: Regex::new(
            r"(?i)^(?:(?:about|around|roughly|approximately|only|just|now|currently|nearly|over|under|at|to|from|up to|capped at|set at|set to|going up to|down to|up|down|a total of|an average of|an average|averaged|a balance of|a deposit of)\s+)*(?P<value>[$€£]?\d[\d,]*(?:\.\d+)?)\s*(?P<unit>%|percent|[a-z]+(?:\s+(?:a|per|each)\s+[a-z]+)?)?",
        )
        .unwrap(),
        first_person: Regex::new(r"(?i)^(?:i|we|my|our|i'm|i've|i'll|we're|we've|we'll)\b").unwrap(),
        list_marker: Regex::new(r"^(?:[-*•>]+|\d{1,3}[.)]|[a-z][.)])\s+").unwrap(),
    })
}

/// A sentence with where it sits in the text.
#[derive(Debug, Clone, PartialEq)]
struct Sentence {
    start: usize,
    end: usize,
}

fn word_key(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’')
        .to_lowercase()
}

/// Lines starting a new item, whatever the line before ended with.
fn starts_new_block(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || patterns().list_marker.is_match(t)
}

/// Sentences of `text` with byte spans into it. Lines that are only a wrapped
/// stretch of one sentence (text taken from a PDF breaks every 70 characters
/// or so) are joined; headings, list items and blank lines end a sentence;
/// "Dr. Chen", "Mr Okafor", initials and decimals don't.
fn sentences(text: &str) -> Vec<Sentence> {
    // Blocks first: runs of lines that belong together.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut block_start: Option<usize> = None;
    let mut prev: Option<(usize, usize)> = None; // trimmed span of the previous line
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim();
        if trimmed.is_empty() {
            if let (Some(s), Some((_, e))) = (block_start.take(), prev) {
                blocks.push((s, e));
            }
            prev = None;
            continue;
        }
        let lead = content.len() - content.trim_start().len();
        let span = (line_start + lead, line_start + lead + trimmed.len());
        if let (Some(s), Some((ps, pe))) = (block_start, prev) {
            let before = text[ps..pe].trim_end();
            let ends_sentence = before.ends_with(['.', '!', '?', ':', ';']);
            let next_lower = trimmed.chars().next().is_some_and(char::is_lowercase);
            let long_line = before.chars().count() >= 50;
            let join = !starts_new_block(trimmed) && (next_lower || (!ends_sentence && long_line));
            if !join {
                blocks.push((s, pe));
                block_start = Some(span.0);
            }
        } else {
            block_start = Some(span.0);
        }
        prev = Some(span);
    }
    if let (Some(s), Some((_, e))) = (block_start, prev) {
        blocks.push((s, e));
    }

    // Then sentences within each block.
    let mut out = Vec::new();
    for (bs, be) in blocks {
        let block = &text[bs..be];
        let bytes = block.as_bytes();
        let mut start = 0usize;
        for i in 0..bytes.len() {
            let b = bytes[i];
            if !matches!(b, b'.' | b'!' | b'?') {
                continue;
            }
            let rest = &block[i + 1..];
            let after_space = rest.chars().next().is_some_and(char::is_whitespace);
            if !after_space {
                continue;
            }
            let next = rest.trim_start().chars().next();
            let starts_sentence = next.is_none_or(|c| {
                c.is_uppercase() || c.is_ascii_digit() || matches!(c, '"' | '“' | '(' | '\'')
            });
            if !starts_sentence {
                continue;
            }
            if b == b'.' {
                let before = block[start..i].split_whitespace().next_back().unwrap_or("");
                let key = before
                    .trim_matches(|c: char| !c.is_alphanumeric() && c != '.')
                    .to_lowercase();
                let initial = before.chars().count() == 1 && before.chars().all(char::is_uppercase);
                if ABBREVIATIONS.contains(&key.as_str()) || initial {
                    continue;
                }
            }
            push_trimmed(&mut out, text, bs + start, bs + i + 1);
            start = i + 1;
        }
        push_trimmed(&mut out, text, bs + start, be);
    }
    out
}

fn push_trimmed(out: &mut Vec<Sentence>, text: &str, start: usize, end: usize) {
    let raw = &text[start..end];
    let lead = raw.len() - raw.trim_start().len();
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        out.push(Sentence {
            start: start + lead,
            end: start + lead + trimmed.len(),
        });
    }
}

fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A sentence as plain words: the bold and italic markers around words are
/// dropped (the sentence is shown and compared as text; the span still
/// points at the markup).
fn plain(s: &str) -> String {
    let unmarked = s.replace("**", "").replace("__", "");
    let words: Vec<String> = unmarked
        .split_whitespace()
        .map(|w| {
            let t = w.trim_start_matches('*');
            let t = if t.ends_with('*') && !t.ends_with("**") {
                t.trim_end_matches('*')
            } else {
                t
            };
            t.to_string()
        })
        .collect();
    words.join(" ")
}

/// Whether a lowercase word is one of `list`.
fn is_in(list: &[&str], w: &str) -> bool {
    list.contains(&w)
}

/// Words of a sentence with their lowercase keys.
fn words(sentence: &str) -> Vec<(&str, String)> {
    sentence
        .split_whitespace()
        .map(|w| (w, word_key(w)))
        .collect()
}

/// "has one maintainer", "with one exception": "one" is a count after a word
/// that asks how many, and a pronoun or filler elsewhere ("one of the",
/// "no one", "one day").
fn counts_one(words: &[(&str, String)]) -> bool {
    words.windows(3).any(|w| {
        matches!(
            w[0].1.as_str(),
            "has" | "have" | "had" | "with" | "exactly" | "only"
        ) && w[1].1 == "one"
            && !matches!(
                w[2].1.as_str(),
                "of" | "more" | "another" | "or" | "day" | "way" | "thing"
            )
    })
}

/// What makes a sentence worth keeping: a figure, a date, a name, or a
/// decision, report or plan. Without one it is an opinion or small talk.
fn is_specific(sentence: &str, words: &[(&str, String)]) -> bool {
    let p = patterns();
    if p.digit.is_match(sentence) || p.day_month.is_match(sentence) {
        return true;
    }
    if words.iter().any(|(_, k)| {
        is_in(MONTHS, k) && k.len() > 3 || is_in(TIME_WORDS, k) || is_in(NUMBER_WORDS, k)
    }) || counts_one(words)
    {
        return true;
    }
    // A decision, report or preference verb says something when someone or
    // something named is involved, or the sentence has room for content
    // ("a line that says nothing much" is neither).
    let named = words.iter().skip(1).any(|(w, k)| {
        w.chars().next().is_some_and(char::is_uppercase) && k != "i" && !k.starts_with("i'")
    });
    let first_person = patterns().first_person.is_match(sentence);
    if (named || first_person || words.len() >= 8)
        && words.iter().any(|(_, k)| {
            is_in(DECISION_VERBS, k) || is_in(REPORT_VERBS, k) || is_in(PREFERENCE_VERBS, k)
        })
    {
        return true;
    }
    // Something the author did, said in the first person and in the past
    // tense ("We rotated all credentials the same day").
    if first_person
        && words.len() >= 6
        && words.iter().skip(1).any(|(w, k)| {
            let plain = !w.chars().next().is_some_and(char::is_uppercase);
            plain && ((k.ends_with("ed") && k.len() > 4) || is_in(VERBS, k) && is_past(k))
        })
    {
        return true;
    }
    // A name: a capital letter anywhere but the first word.
    words.iter().skip(1).any(|(w, k)| {
        w.chars().next().is_some_and(char::is_uppercase) && k != "i" && !k.starts_with("i'")
    })
}

/// The subject phrase: the words up to the first verb, with a leading
/// determiner kept off the name later by the store. None for a pronoun or
/// when no verb is found within a few words.
fn subject_of(words: &[(&str, String)]) -> Option<(String, usize)> {
    let limit = words.len().min(10);
    for (i, (_, k)) in words.iter().enumerate().take(limit) {
        // "Orders older than two years are archived": a verb-looking word that
        // opens a sentence is a noun when an auxiliary follows soon after.
        let opens_as_noun = (i == 0 || (i == 1 && is_in(DETERMINERS, &words[0].1)))
            && !is_in(AUXILIARIES, k)
            && words
                .iter()
                .skip(i + 1)
                .take(6)
                .any(|(_, a)| is_in(AUXILIARIES, a));
        if opens_as_noun {
            continue;
        }
        let verbish = is_in(AUXILIARIES, k)
            || (i > 0 && contraction_aux(k))
            || is_in(VERBS, k)
            || is_in(DECISION_VERBS, k)
            || is_in(REPORT_VERBS, k)
            || is_in(PREFERENCE_VERBS, k)
            || (i > 0
                && k.len() > 4
                && k.ends_with("ed")
                && !words[i].0.chars().next().is_some_and(char::is_uppercase));
        if verbish {
            if i == 0 {
                return None;
            }
            let phrase: Vec<&str> = words[..i].iter().map(|(w, _)| *w).collect();
            // "The landlord, Mr Weber, wants ...": the first noun phrase.
            let first = phrase.join(" ");
            let first = first.split(',').next().unwrap_or("").trim().to_string();
            // An article alone is no subject.
            if is_in(DETERMINERS, &word_key(&first)) {
                return Some((String::new(), i));
            }
            // A phrase that runs into a relative clause ("A system that
            // suggests ...") or holds quoted or bracketed text is not a name.
            let last = word_key(first.split_whitespace().next_back().unwrap_or(""));
            if matches!(
                last.as_str(),
                "that" | "which" | "who" | "whom" | "whose" | "where" | "when"
            ) || first.contains(['"', '`', '(', ')', '[', ']', '“', '”'])
            {
                return Some((String::new(), i));
            }
            return (!first.is_empty()).then_some((first, i));
        }
    }
    None
}

/// "I'll", "we've", "they're": a pronoun with its auxiliary, as one word.
fn contraction_aux(k: &str) -> bool {
    [
        "'ll", "'ve", "'re", "'m", "'d", "’ll", "’ve", "’re", "’m", "’d",
    ]
    .iter()
    .any(|suffix| k.ends_with(suffix))
}

/// Whether `s`'s subject phrase is a pronoun or other non-name.
/// "My landlord" names the landlord, as the rules read "My budget is ...".
fn without_possessive(phrase: &str) -> String {
    match phrase.split_once(char::is_whitespace) {
        Some((first, rest))
            if matches!(word_key(first).as_str(), "my" | "our") && !rest.trim().is_empty() =>
        {
            rest.trim().to_string()
        }
        _ => phrase.to_string(),
    }
}

fn is_pronoun_phrase(subject: &str) -> bool {
    let k = word_key(subject.split_whitespace().next().unwrap_or(""));
    PRONOUN_SUBJECTS.contains(&k.as_str())
}

/// The reading of one sentence.
struct Reading {
    kind: &'static str,
    subject: Option<String>,
    objects: Vec<(String, String)>,
    attrs: serde_json::Value,
    event_time: Option<chrono::DateTime<chrono::Utc>>,
}

/// Spoken openers ("ok so", "well,") that say nothing: the sentence is read
/// without them.
fn without_openers(sentence: &str) -> &str {
    let mut rest = sentence;
    for _ in 0..2 {
        let first = rest.split_whitespace().next().unwrap_or("");
        let key = word_key(first);
        if matches!(
            key.as_str(),
            "ok" | "okay" | "so" | "well" | "and" | "but" | "anyway" | "also" | "then" | "yeah"
        ) && rest.len() > first.len()
        {
            rest = rest[first.len()..].trim_start();
        } else {
            break;
        }
    }
    rest
}

fn read(sentence: &str, statement: &str) -> Option<Reading> {
    let p = patterns();
    let sentence = without_openers(sentence);
    let ws = words(sentence);
    if ws.len() < MIN_WORDS {
        return None;
    }
    let first_key = ws[0].1.as_str();
    let header = is_in(HEADER_LABELS, first_key) && ws[0].0.ends_with(':');
    if (header || is_in(NON_STATEMENT_OPENERS, first_key)) && !p.first_person.is_match(sentence) {
        return None;
    }
    if !is_specific(sentence, &ws) {
        return None;
    }
    if !worth::structural_ok(statement) {
        return None;
    }

    let first_person = p.first_person.is_match(sentence);
    // "My landlord lives in Paris" is about the landlord, not the author.
    let speaker = first_person && !matches!(first_key, "my" | "our");
    let (subject_phrase, verb_at) = match subject_of(&ws) {
        Some((s, i)) => (Some(s), Some(i)),
        None => (None, None),
    };
    // A sentence with no verb we can place is more likely a heading or a
    // fragment than a statement. (A first-person one gets the benefit of the
    // doubt: "I'll ping Omar in the morning".)
    let verb_at = match verb_at {
        Some(i) => i,
        None if first_person => 0,
        None => return None,
    };
    let subject = match subject_phrase {
        _ if speaker => Some("Me".to_string()),
        Some(s) if !s.is_empty() && !is_pronoun_phrase(&s) => Some(without_possessive(&s)),
        _ => None,
    };
    let verb_key = ws[verb_at].1.as_str();
    let lower = sentence.to_lowercase();

    let has = |list: &[&str]| ws.iter().any(|(_, k)| is_in(list, k));
    let iso = p.iso_date.captures(sentence);
    let dated = iso.is_some() || p.day_month.is_match(sentence) || p.clock.is_match(sentence);
    let past_action = ws.iter().skip(1).any(|(w, k)| {
        let plain = !w.chars().next().is_some_and(char::is_uppercase);
        plain && (k.ends_with("ed") && k.len() > 4 || is_in(VERBS, k) && is_past(k))
    });

    let (kind, mut objects): (&'static str, Vec<(String, String)>) = if has(DECISION_VERBS) {
        ("decision", decision_objects(&ws))
    } else if has(PREFERENCE_VERBS) {
        ("preference", Vec::new())
    } else if has(REPORT_VERBS) {
        ("claim", Vec::new())
    } else if dated && past_action {
        ("event", Vec::new())
    } else if lower.contains(" will ")
        || lower.contains(" going to ")
        || lower.contains(" plans to ")
    {
        ("claim", Vec::new())
    } else {
        ("fact", Vec::new())
    };
    if kind == "fact" || kind == "claim" {
        if let Some(rel) = place_relation(&ws, verb_at) {
            objects.push(rel);
        }
    }

    let mut attrs = json!({ "pattern": "general", "reader": "general", "shape": kind });
    // A figure stated about the subject, so two notes that disagree about it
    // can be set against each other by the contradiction scan.
    if let Some(s) = &subject {
        if s != "Me" && (AUXILIARIES.contains(&verb_key) || is_in(VERBS, verb_key)) {
            if let Some((value, unit)) = figure_after(&ws, verb_at) {
                attrs = json!({ "pattern": "numeric", "reader": "general", "value": value, "unit": unit, "shape": kind });
            }
        }
    }

    let event_time = iso.filter(|_| kind == "event").and_then(|c| {
        chrono::NaiveDate::parse_from_str(c.get(0)?.as_str(), "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| chrono::DateTime::from_naive_utc_and_offset(dt, chrono::Utc))
    });
    Some(Reading {
        kind,
        subject,
        objects,
        attrs,
        event_time,
    })
}

/// A verb in the past tense, among the ones listed.
fn is_past(k: &str) -> bool {
    matches!(
        k,
        "went"
            | "took"
            | "made"
            | "sent"
            | "booked"
            | "paid"
            | "ran"
            | "met"
            | "left"
            | "arrived"
            | "landed"
            | "bought"
            | "sold"
            | "wrote"
            | "built"
            | "shipped"
            | "launched"
            | "started"
            | "finished"
            | "moved"
            | "planted"
            | "covered"
            | "signed"
            | "kept"
            | "held"
            | "gave"
            | "got"
            | "came"
            | "rose"
            | "fell"
            | "became"
            | "began"
            | "ended"
            | "spent"
            | "owed"
            | "earned"
            | "joined"
            | "opened"
            | "closed"
            | "fixed"
            | "ordered"
            | "reached"
            | "dropped"
            | "improved"
            | "used"
            | "needed"
            | "worked"
            | "lived"
            | "stored"
            | "charged"
            | "turned"
            | "increased"
            | "decreased"
            | "raised"
            | "reduced"
    )
}

/// "chose X over Y", "decided on X", "picked X": what was chosen.
fn decision_objects(ws: &[(&str, String)]) -> Vec<(String, String)> {
    let at = ws.iter().position(|(_, k)| {
        matches!(
            k.as_str(),
            "chose" | "choose" | "picked" | "selected" | "approved" | "voted" | "decided"
        )
    });
    let Some(at) = at else {
        return Vec::new();
    };
    let mut i = at + 1;
    // "decided on X", "voted for X", "chose to X" (an action, not a thing).
    if ws
        .get(i)
        .is_some_and(|(_, k)| matches!(k.as_str(), "on" | "for" | "against"))
    {
        i += 1;
    }
    if ws.get(i).is_some_and(|(_, k)| k == "to") {
        return Vec::new();
    }
    let object: Vec<&str> = ws[i..]
        .iter()
        .take_while(|(_, k)| {
            !matches!(
                k.as_str(),
                "over"
                    | "because"
                    | "for"
                    | "so"
                    | "and"
                    | "to"
                    | "from"
                    | "in"
                    | "at"
                    | "when"
                    | "that"
            )
        })
        .map(|(w, _)| *w)
        .take(5)
        .collect();
    let object = tidy(&object.join(" "));
    let object = object
        .trim_matches(|c: char| !c.is_alphanumeric())
        .trim_start_matches("the ")
        .trim_start_matches("a ")
        .trim_start_matches("an ")
        .to_string();
    if object.len() < 3 {
        return Vec::new();
    }
    vec![(object, "decided_on".to_string())]
}

/// "X lives in Y", "X works at Y": the relations the contradiction scan
/// treats as one value at a time.
fn place_relation(ws: &[(&str, String)], verb_at: usize) -> Option<(String, String)> {
    let verb = ws[verb_at].1.as_str();
    let relation = match verb {
        "lives" | "lived" => "lives_in",
        "works" | "worked" => "works_at",
        _ => return None,
    };
    let prep = ws.get(verb_at + 1)?.1.as_str();
    if !matches!(prep, "in" | "at" | "for") {
        return None;
    }
    let object: Vec<&str> = ws[verb_at + 2..]
        .iter()
        .take_while(|(w, _)| w.chars().next().is_some_and(char::is_uppercase))
        .map(|(w, _)| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .collect();
    (!object.is_empty()).then(|| (object.join(" "), relation.to_string()))
}

/// The figure and its unit straight after the verb.
fn figure_after(ws: &[(&str, String)], verb_at: usize) -> Option<(String, String)> {
    // The verb itself, then (for "is going up to", "has risen to") a few
    // connecting words: let the pattern take them.
    let rest: Vec<&str> = ws[verb_at + 1..].iter().map(|(w, _)| *w).collect();
    let tail = rest.join(" ");
    let c = patterns().number_tail.captures(&tail)?;
    let value_end = c.name("value")?.end();
    let value = c.name("value")?.as_str().to_string();
    // "9:30" and "2025-06-30" are times and dates: the digits run on into the
    // rest of one.
    let mut after = tail[value_end..].chars();
    if matches!(after.next(), Some(':' | '-' | '/'))
        && after.next().is_some_and(|d| d.is_ascii_digit())
    {
        return None;
    }
    let unit = c
        .name("unit")
        .map(|m| m.as_str().to_lowercase())
        .unwrap_or_default();
    // "12 March" and "9:30" are dates and times, not amounts.
    if is_in(MONTHS, &unit) || is_in(TIME_WORDS, &unit) || matches!(unit.as_str(), "am" | "pm") {
        return None;
    }
    // A unit that is only a grammar word says nothing about the figure.
    if matches!(
        unit.as_str(),
        "and"
            | "or"
            | "the"
            | "of"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "is"
            | "was"
            | "by"
            | "with"
            | "from"
            | "but"
            | "so"
    ) {
        return Some((value, String::new()));
    }
    Some((value, unit))
}

/// Read ordinary sentences out of `text`, leaving alone any that overlap the
/// `taken` spans (what the rules already read).
pub fn extract_units(text: &str, taken: &[(usize, usize)]) -> Vec<ExtractedUnit> {
    let mut units = Vec::new();
    for s in sentences(text) {
        if taken.iter().any(|&(a, b)| s.start < b && a < s.end) {
            continue;
        }
        let raw = &text[s.start..s.end];
        if raw.trim_start().starts_with('#') {
            continue;
        }
        let raw = patterns().list_marker.replace(raw, "");
        let sentence = plain(&tidy(&raw));
        let len = sentence.chars().count();
        if !(MIN_SENTENCE_CHARS..=MAX_SENTENCE_CHARS).contains(&len) || sentence.ends_with('?') {
            continue;
        }
        let statement = sentence
            .trim_end_matches(['.', '!', '?'])
            .trim()
            .to_string();
        let Some(reading) = read(&sentence, &statement) else {
            continue;
        };
        units.push(ExtractedUnit {
            kind: reading.kind,
            statement,
            subject: reading.subject,
            objects: reading.objects,
            char_start: s.start,
            char_end: s.end,
            confidence: BASE_CONFIDENCE,
            attrs: reading.attrs,
            event_time: reading.event_time,
        });
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(text: &str) -> Vec<String> {
        extract_units(text, &[])
            .into_iter()
            .map(|u| u.statement)
            .collect()
    }

    fn units(text: &str) -> Vec<ExtractedUnit> {
        extract_units(text, &[])
    }

    #[test]
    fn a_mail_header_is_not_read_but_prose_that_starts_with_the_same_word_is() {
        assert!(statements("From: Dana Weber <dana@example.com>").is_empty());
        assert!(statements("Subject: Quarterly budget figures for 2024").is_empty());
        assert_eq!(
            statements(
                "From 2022 to 2024, the building's attendance rose from 80 to 140 visitors."
            )
            .len(),
            1
        );
    }

    #[test]
    fn my_landlord_is_the_subject_of_a_sentence_about_the_landlord() {
        let u = units("My landlord lives in Paris.");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].subject.as_deref(), Some("landlord"));
        // Only I and we speak for the author.
        let u = units("We rented the Leeds office in 2019.");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].subject.as_deref(), Some("Me"));
    }

    #[test]
    fn a_date_in_a_fact_is_not_when_the_fact_became_true() {
        let u = units("The audit deadline is 2025-06-30.");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].kind, "fact");
        assert!(u[0].event_time.is_none(), "{:?}", u[0].event_time);
        // Nor is it an amount.
        assert_ne!(u[0].attrs["pattern"], "numeric", "{}", u[0].attrs);
        // An event is dated by its own date.
        let u = units("Marta moved to Leeds on 2024-03-01.");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].kind, "event");
        assert!(u[0].event_time.is_some());
    }

    #[test]
    fn counts_in_words_are_figures_only_where_they_count_something() {
        assert_eq!(
            statements("The project has one maintainer on call.").len(),
            1
        );
        assert_eq!(statements("The project has zero open incidents.").len(), 1);
        assert_eq!(
            statements("The pilot ran with one exception in the second term.").len(),
            1
        );
        assert!(statements("Honestly one never knows how these things go.").is_empty());
        assert!(statements("It is one of those things we do sometimes.").is_empty());
    }

    #[test]
    fn a_noun_that_looks_like_a_verb_after_the_article_stays_in_the_subject() {
        let u = units("The orders older than two years are archived.");
        assert_eq!(u.len(), 1);
        assert_eq!(
            u[0].subject.as_deref(),
            Some("The orders older than two years")
        );
    }

    #[test]
    fn a_clock_time_is_not_an_amount() {
        for text in [
            "The project meeting starts at 9:30.",
            "The project meeting starts at 12:30.",
            "The project meeting starts at 9 pm.",
            "The project meeting starts at 9am.",
        ] {
            let u = units(text);
            assert_eq!(u.len(), 1, "{text}");
            assert_ne!(u[0].attrs["pattern"], "numeric", "{text}: {}", u[0].attrs);
        }
        // A real amount after the same verb still is.
        let u = units("The project budget is 9,000 euros.");
        assert_eq!(u[0].attrs["pattern"], "numeric", "{}", u[0].attrs);
    }

    #[test]
    fn soft_wrapped_lines_are_one_sentence() {
        let text = "The pilot programme ran in four schools between September 2021 and June\n2022. Attendance in the participating classes improved by 6 percent compared\nwith the previous year.";
        let s = sentences(text);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(text[s[0].start..s[0].end].starts_with("The pilot"));
        assert!(text[s[0].start..s[0].end].ends_with("2022."));
        assert!(text[s[1].start..s[1].end].ends_with("previous year."));
    }

    #[test]
    fn headings_list_items_and_paragraphs_end_a_sentence() {
        let text = "# Title\n\nFirst paragraph sentence here.\n- one item\n- another item\n\nSecond paragraph.";
        let s: Vec<&str> = sentences(text)
            .iter()
            .map(|s| &text[s.start..s.end])
            .collect();
        assert_eq!(
            s,
            [
                "# Title",
                "First paragraph sentence here.",
                "- one item",
                "- another item",
                "Second paragraph."
            ]
        );
    }

    #[test]
    fn abbreviations_initials_and_decimals_do_not_end_a_sentence() {
        let text = "Dr. Chen saw Mr. Okafor on 3.2 million occasions. J. Smith agreed to the plan. Then it rained.";
        let s: Vec<&str> = sentences(text)
            .iter()
            .map(|s| &text[s.start..s.end])
            .collect();
        assert_eq!(
            s,
            [
                "Dr. Chen saw Mr. Okafor on 3.2 million occasions.",
                "J. Smith agreed to the plan.",
                "Then it rained."
            ]
        );
    }

    #[test]
    fn a_figure_a_date_a_name_or_a_decision_makes_a_sentence_worth_keeping() {
        for kept in [
            "The treasurer reported a balance of 2,340 pounds in the club account.",
            "The next meeting will be held on 8 April at the village hall.",
            "Joao said the electrician can start the week after.",
            "We also agreed to stop doing Friday releases.",
            "The landlord, Mr Weber, wants a deposit of two months before we sign.",
        ] {
            assert_eq!(statements(kept).len(), 1, "{kept}");
        }
    }

    #[test]
    fn small_talk_requests_questions_and_vague_opinions_are_left_out() {
        for left in [
            "Hope you had a lovely weekend!",
            "Can you forward me the agenda? Also, did anyone book the room?",
            "Please send your documents when you can.",
            "I really enjoyed the film.",
            "The ending was a bit predictable though.",
            "Thanks a lot, really appreciate it.",
            "Step 1: Preheat the oven.",
            "Do not open the door while baking.",
            "Morale is generally good.",
        ] {
            assert!(statements(left).is_empty(), "{left}");
        }
    }

    #[test]
    fn what_a_sentence_is_about_and_what_kind_it_is() {
        let units = extract_units(
            "The committee voted to raise annual fees from 30 to 35 pounds. We chose the grey quartz worktop over the oak one because it is easier to clean. The guesthouse charges 55 euros a night. I prefer the cherry variety because they ripen sooner.",
            &[],
        );
        let kinds: Vec<_> = units.iter().map(|u| u.kind).collect();
        assert_eq!(kinds, ["decision", "decision", "fact", "preference"]);
        assert_eq!(units[0].subject.as_deref(), Some("The committee"));
        assert_eq!(units[1].subject.as_deref(), Some("Me"));
        assert_eq!(
            units[1].objects,
            vec![("grey quartz worktop".to_string(), "decided_on".to_string())]
        );
        assert_eq!(units[2].subject.as_deref(), Some("The guesthouse"));
    }

    #[test]
    fn a_stated_figure_carries_its_value_and_unit_for_the_contradiction_scan() {
        let a = extract_units("The venue holds 300 people.", &[]);
        let b = extract_units("The conference budget is 12,000 dollars.", &[]);
        let c = extract_units("The guesthouse charges 55 euros a night.", &[]);
        assert_eq!(a[0].attrs["pattern"], "numeric");
        assert_eq!(a[0].attrs["value"], "300");
        assert_eq!(a[0].attrs["unit"], "people");
        assert_eq!(b[0].attrs["value"], "12,000");
        assert_eq!(b[0].attrs["unit"], "dollars");
        assert_eq!(c[0].attrs["value"], "55");
        assert_eq!(c[0].attrs["unit"], "euros a night");
    }

    #[test]
    fn a_date_is_not_a_figure() {
        let u = extract_units("Registration opens on 15 March for everyone.", &[]);
        assert_ne!(u[0].attrs["pattern"], "numeric", "{:?}", u[0].attrs);
    }

    #[test]
    fn sentences_the_rules_took_are_not_read_twice() {
        let text = "We decided on Hetzner CX22 for the backup target. The venue holds 300 people.";
        let rules = crate::extract::rules::extract_units(text);
        assert_eq!(rules.len(), 1);
        let taken: Vec<(usize, usize)> = rules.iter().map(|u| (u.char_start, u.char_end)).collect();
        let general = extract_units(text, &taken);
        assert_eq!(general.len(), 1);
        assert!(general[0].statement.contains("venue"));
    }

    #[test]
    fn spans_point_at_the_sentence_in_the_text() {
        let text =
            "Intro line that says nothing much.\nThe venue holds 300 people, which is enough.";
        let units = extract_units(text, &[]);
        assert_eq!(units.len(), 1);
        assert_eq!(
            &text[units[0].char_start..units[0].char_end],
            "The venue holds 300 people, which is enough."
        );
    }

    #[test]
    fn confidence_starts_below_the_rules() {
        let units = extract_units("The venue holds 300 people.", &[]);
        assert!(units[0].confidence < 0.6);
    }
}
