//! Chunking-quality gate: does cutting a long paragraph into segments change
//! what the always-on extractor stores?
//!
//! The rule extractor reads one sentence at a time and ignores any sentence over
//! 400 bytes. A segment boundary in the middle of a sentence therefore hands it
//! two halves that are each short enough to be read as a statement of their own:
//! a fact that straddles the cut is lost, and each half can be stored as a
//! truncated statement. `segment_text` ends an oversized paragraph where the
//! extractor would have split anyway, so segmenting must not change what is
//! stored. This test checks exactly that on seeded single-paragraph documents
//! (the shape of most extracted PDF text), against a copy of the old hard cut
//! kept as the control, so the test cannot silently stop exercising the defect.
//! No database, no Ollama.

use std::collections::BTreeMap;

use gather_daemon::extract::rules::extract_units;
use gather_daemon::extract::segment::segment_text;
use gather_daemon::extract::worth::{readable, unit_worth_keeping};

/// `segment_text`'s cap. The old hard cut below needs the number; the first
/// assertion in the test fails if the two ever disagree about it.
const CAP: usize = 2000;
const DOCUMENTS: usize = 300;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

const FACTS: [&str; 8] = [
    "I use PostgreSQL for the knowledge store.",
    "We decided on Hetzner CX22 for the backup target.",
    "I prefer dark mode in every editor.",
    "My Hetzner CX22 costs 5 euros per month.",
    "On 2026-04-01 I migrated the database to pgvector.",
    "We settled on Rust for the daemon.",
    "I work at Anthropic on developer tools.",
    "pgvector means Postgres with vector similarity search.",
];

const FILLER: [&str; 6] = [
    "The committee reviewed the quarterly findings and noted several open questions about scope and timing.",
    "Meanwhile the surrounding neighbourhood continued to change as new buildings replaced the older warehouses.",
    "It was widely agreed that the earlier approach had been too slow, although opinions differed on why.",
    "Several attendees mentioned that the schedule should be revisited once the new data arrived next month.",
    "The report describes the background in some detail before turning to the main results of the study.",
    "Nothing in the appendix contradicts the summary, but the tables are worth reading closely anyway.",
];

/// One long paragraph with no blank lines (like most extracted PDF text): about
/// 30% fact sentences among filler, joined by a space or, one time in six, a
/// single line break. Returns the text and the facts it contains.
fn document(rng: &mut Rng) -> (String, Vec<&'static str>) {
    let sentences = 40 + rng.below(30);
    let mut text = String::new();
    let mut facts = Vec::new();
    for _ in 0..sentences {
        if rng.below(10) < 3 {
            let fact = FACTS[rng.below(FACTS.len())];
            facts.push(fact);
            text.push_str(fact);
        } else {
            text.push_str(FILLER[rng.below(FILLER.len())]);
        }
        text.push(if rng.below(6) == 0 { '\n' } else { ' ' });
    }
    (text, facts)
}

/// The statements the always-on path would store from one chunk, exactly as the
/// extraction worker applies the readability check, the rules and the gate.
fn stored(chunk: &str) -> Vec<String> {
    let Some(text) = readable(chunk) else {
        return Vec::new();
    };
    extract_units(&text)
        .into_iter()
        .filter(unit_worth_keeping)
        .map(|unit| unit.statement)
        .collect()
}

/// What `segment_text` did before: end every chunk of an oversized paragraph at
/// exactly CAP bytes (backed off to a char boundary), wherever that falls.
/// Kept as the control.
fn hard_cut(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text.trim();
    while rest.len() > CAP {
        let mut cut = CAP;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        chunks.push(rest[..cut].trim().to_string());
        rest = &rest[cut..];
    }
    if !rest.trim().is_empty() {
        chunks.push(rest.trim().to_string());
    }
    chunks
}

fn counts(statements: &[String]) -> BTreeMap<&str, usize> {
    let mut map = BTreeMap::new();
    for s in statements {
        *map.entry(s.as_str()).or_insert(0) += 1;
    }
    map
}

/// How `got` differs from `whole`, as multisets: statements it lost and
/// statements it made up (a fragment of a sentence is a made-up statement).
fn difference(whole: &[String], got: &[String]) -> (usize, usize) {
    let (whole, got) = (counts(whole), counts(got));
    let lost = whole
        .iter()
        .map(|(s, n)| n.saturating_sub(got.get(s).copied().unwrap_or(0)))
        .sum();
    let invented = got
        .iter()
        .map(|(s, n)| n.saturating_sub(whole.get(s).copied().unwrap_or(0)))
        .sum();
    (lost, invented)
}

/// How many planted facts (counting repeats) appear among `got`.
fn recovered(planted: &[&str], got: &[String]) -> usize {
    let mut available = counts(got)
        .into_iter()
        .map(|(s, n)| (s.to_string(), n))
        .collect::<BTreeMap<_, _>>();
    planted
        .iter()
        .filter(|fact| {
            let statement = fact.trim_end_matches('.');
            match available.get_mut(statement) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    true
                }
                _ => false,
            }
        })
        .count()
}

#[derive(Default)]
struct Totals {
    recovered: usize,
    lost: usize,
    invented: usize,
    segments: usize,
}

#[test]
fn segmenting_does_not_change_what_the_extractor_stores() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut planted_total, mut over_cap) = (0usize, 0usize);
    let (mut whole_t, mut aware_t, mut hard_t) =
        (Totals::default(), Totals::default(), Totals::default());
    let mut first_difference: Option<String> = None;

    for index in 0..DOCUMENTS {
        let (text, planted) = document(&mut rng);
        over_cap += (text.trim().len() > CAP) as usize;
        planted_total += planted.len();

        let whole = stored(&text);
        let segments = segment_text(&text);
        assert!(
            segments.iter().all(|s| s.content.len() <= CAP),
            "document {index}: a segment is over the {CAP}-byte cap"
        );
        let aware: Vec<String> = segments.iter().flat_map(|s| stored(&s.content)).collect();
        let hard_chunks = hard_cut(&text);
        let hard: Vec<String> = hard_chunks.iter().flat_map(|c| stored(c)).collect();

        for (totals, got, count) in [
            (&mut whole_t, &whole, 1),
            (&mut aware_t, &aware, segments.len()),
            (&mut hard_t, &hard, hard_chunks.len()),
        ] {
            let (lost, invented) = difference(&whole, got);
            totals.recovered += recovered(&planted, got);
            totals.lost += lost;
            totals.invented += invented;
            totals.segments += count;
        }
        if first_difference.is_none() && difference(&whole, &aware) != (0, 0) {
            first_difference = Some(format!(
                "document {index}: whole {:?} vs segmented {:?}",
                whole, aware
            ));
        }
    }

    let pct = |n: usize| 100.0 * n as f64 / planted_total as f64;
    eprintln!("── chunking quality (rule-based, offline) ──");
    eprintln!(
        "documents:        {DOCUMENTS}  ({over_cap} longer than the {CAP}-byte cap), {planted_total} planted facts"
    );
    eprintln!("                      facts recovered   lost   invented   segments");
    for (name, t) in [
        ("whole document", &whole_t),
        ("sentence-aware cut", &aware_t),
        ("hard cut (control)", &hard_t),
    ] {
        eprintln!(
            "{name:<20}  {:>5} ({:>5.1}%)  {:>5}  {:>8}  {:>9}",
            t.recovered,
            pct(t.recovered),
            t.lost,
            t.invented,
            t.segments
        );
    }
    eprintln!("(lost / invented: statements missing from / not in the whole-document result)");

    assert!(
        over_cap * 10 >= DOCUMENTS * 9,
        "the documents must mostly be longer than one segment, or nothing is cut"
    );
    assert_eq!(
        (aware_t.lost, aware_t.invented),
        (0, 0),
        "segmenting changed what is stored; {}",
        first_difference.unwrap_or_default()
    );
    // The control must show the defect, or this test would pass vacuously.
    assert!(
        hard_t.lost + hard_t.invented > 0,
        "the hard cut changed nothing, so these documents do not exercise the cut"
    );
}
