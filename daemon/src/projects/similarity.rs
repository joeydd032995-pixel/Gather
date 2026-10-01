//! How alike two projects are, and why.
//!
//! Four signals, each a number between 0 and 1:
//! - **files**: the same files (identical content) in both;
//! - **layout**: files at the same paths (`src/main.rs`, `docs/plan.md`),
//!   which catches two versions of one repository even when every file
//!   changed;
//! - **entities**: the same people, organisations, tools and places
//!   mentioned in both;
//! - **content**: what the files are about — the average meaning of their
//!   text when embeddings are on, otherwise the words they use most
//!   (rarer words counting for more).
//!
//! Set overlaps are weighted by rarity: something every project has
//! (`README.md`, `LICENSE`, the user's own name) says little, something only
//! these two share says a lot. A signal only counts when both projects have
//! something for it, and the weights of the signals that count are rescaled
//! to sum to one, so a folder of photos isn't marked down for having no text.
//!
//! **Keeping it small.** A project is remembered as a few kilobytes however
//! big it is: files, paths and entities as 64-bit hashes, and past
//! [`SAMPLE`] of them only a consistent sample — the ones with the smallest
//! hashes, the same rule in every project, so two projects' samples line up
//! and their overlap estimates the whole. Words and names are interned, so a
//! word a thousand projects use is stored once. Signatures are shared, not
//! copied, and the rarity tables are rebuilt only when a project changes.
//!
//! The scoring here is pure; [`load`] fetches what it scores from the
//! database and keeps it cached until a project changes.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;

/// How much each signal counts when both projects have it.
const WEIGHT_FILES: f64 = 0.30;
const WEIGHT_LAYOUT: f64 = 0.20;
const WEIGHT_ENTITIES: f64 = 0.30;
const WEIGHT_CONTENT: f64 = 0.20;
/// Files and paths kept per project; past this, a consistent sample.
pub const SAMPLE: usize = 256;
/// Entities kept per project; past this, a consistent sample.
const SAMPLE_ENTITIES: usize = 128;
/// Words kept per project for the content signal.
const TOP_TERMS: i64 = 200;
/// Segments read per project for its words, so one enormous project can't
/// make every comparison slow.
const MAX_SEGMENTS: i64 = 5_000;
/// Below this, two projects aren't called similar.
pub const MIN_SCORE: f64 = 0.08;
/// Examples named in a reason.
const EXAMPLES: usize = 4;
/// Projects whose signatures are built per database round trip.
const BATCH: usize = 100;
/// How long a check that the cache is current stays good when nothing was
/// written to a project in between (units read from files arrive meanwhile,
/// so they may show a few seconds late).
const RECHECK: Duration = Duration::from_secs(10);

/// A stable (within this process) 64-bit hash.
fn hash_of<T: Hash + ?Sized>(x: &T) -> u64 {
    let mut h = DefaultHasher::new();
    x.hash(&mut h);
    h.finish()
}

/// Keys that are already hashes need no hashing again.
#[derive(Default)]
struct PassThrough(u64);

impl Hasher for PassThrough {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*b);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
    fn write_u32(&mut self, n: u32) {
        // Word ids are small and dense; spread them.
        self.0 = u64::from(n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type Counts<K> = HashMap<K, u32, BuildHasherDefault<PassThrough>>;

// ------------------------------------------------------------ interning ----

static NAMES: LazyLock<Mutex<HashSet<Arc<str>>>> = LazyLock::new(Default::default);

/// One shared copy of a name, however many projects mention it.
pub fn intern(s: &str) -> Arc<str> {
    let mut set = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = set.get(s) {
        return found.clone();
    }
    let arc: Arc<str> = Arc::from(s);
    set.insert(arc.clone());
    arc
}

/// Forget names no signature uses any more.
fn prune_names() {
    let mut set = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    set.retain(|s| Arc::strong_count(s) > 1);
}

/// Words as numbers: each word some signature uses has an id and is stored
/// once. Ids are counted as signatures take and drop them, and a word no
/// signature uses is forgotten and its id reused, so the vocabulary is only
/// ever that of the projects being compared.
#[derive(Default)]
struct Words {
    ids: HashMap<Arc<str>, u32>,
    words: Vec<Option<Arc<str>>>,
    uses: Vec<u32>,
    free: Vec<u32>,
}

impl Words {
    fn take(&mut self, word: &str) -> u32 {
        let id = match self.ids.get(word) {
            Some(id) => *id,
            None => {
                let arc: Arc<str> = Arc::from(word);
                let id = match self.free.pop() {
                    Some(id) => {
                        self.words[id as usize] = Some(arc.clone());
                        id
                    }
                    None => {
                        self.words.push(Some(arc.clone()));
                        self.uses.push(0);
                        (self.words.len() - 1) as u32
                    }
                };
                self.ids.insert(arc, id);
                id
            }
        };
        self.uses[id as usize] += 1;
        id
    }

    fn drop_use(&mut self, id: u32) {
        let uses = &mut self.uses[id as usize];
        *uses -= 1;
        if *uses == 0 {
            if let Some(w) = self.words[id as usize].take() {
                self.ids.remove(&w);
            }
            self.free.push(id);
        }
    }
}

static WORDS: LazyLock<Mutex<Words>> = LazyLock::new(Default::default);

fn words() -> std::sync::MutexGuard<'static, Words> {
    WORDS.lock().unwrap_or_else(|e| e.into_inner())
}

fn word(id: u32) -> String {
    words()
        .words
        .get(id as usize)
        .and_then(|w| w.as_deref())
        .map(str::to_string)
        .unwrap_or_default()
}

/// A project's words (ids, sorted) with how often each is used; holds its
/// words in the vocabulary for as long as it lives.
#[derive(Debug, Default)]
pub struct Terms(Box<[(u32, f32)]>);

impl Terms {
    fn new<'a>(terms: impl IntoIterator<Item = (&'a str, f32)>) -> Self {
        let mut terms: Vec<(&str, f32)> = terms.into_iter().collect();
        terms.sort_by(|a, b| a.0.cmp(b.0));
        terms.dedup_by(|a, b| a.0 == b.0);
        let mut w = words();
        let mut ids: Vec<(u32, f32)> = terms.into_iter().map(|(t, n)| (w.take(t), n)).collect();
        ids.sort_by_key(|(id, _)| *id);
        Terms(ids.into_boxed_slice())
    }
}

impl std::ops::Deref for Terms {
    type Target = [(u32, f32)];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Clone for Terms {
    fn clone(&self) -> Self {
        if !self.0.is_empty() {
            let mut w = words();
            for (id, _) in self.0.iter() {
                w.uses[*id as usize] += 1;
            }
        }
        Terms(self.0.clone())
    }
}

impl Drop for Terms {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            let mut w = words();
            for (id, _) in self.0.iter() {
                w.drop_use(*id);
            }
        }
    }
}

// ------------------------------------------------------------- samples ----

/// A set of items, as sorted hashes. Past `limit` items only those with the
/// smallest hashes are kept; the rule is the same everywhere, so two
/// samples compared up to the smaller one's cutoff hold the same items of
/// the union.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    hashes: Box<[u64]>,
    /// How many distinct items there were in all.
    total: usize,
}

impl Sample {
    pub fn new(hashes: impl IntoIterator<Item = u64>, limit: usize) -> Self {
        let mut hashes: Vec<u64> = hashes.into_iter().collect();
        hashes.sort_unstable();
        hashes.dedup();
        let total = hashes.len();
        hashes.truncate(limit);
        Sample {
            hashes: hashes.into_boxed_slice(),
            total,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.total
    }

    fn cutoff(&self) -> u64 {
        if self.total > self.hashes.len() {
            *self.hashes.last().expect("a truncated sample isn't empty")
        } else {
            u64::MAX
        }
    }
}

/// The overlap of two samples, weighted by rarity (`wa`, `wb`: the weight
/// of each sampled item, in order): the weight of what both hold over the
/// weight of what either holds. With the positions in `a` of the shared
/// items, rarest first, and an estimate of how many items are shared in all
/// (`true` when it is an estimate).
fn weighted_jaccard(
    a: &Sample,
    wa: &[f32],
    b: &Sample,
    wb: &[f32],
) -> (f64, Vec<usize>, usize, bool) {
    let cutoff = a.cutoff().min(b.cutoff());
    let (ha, hb) = (&a.hashes, &b.hashes);
    let (mut i, mut j) = (0, 0);
    let (mut both, mut union) = (0.0f64, 0.0f64);
    let mut shared: Vec<(f32, usize)> = Vec::new();
    while i < ha.len() && ha[i] <= cutoff && j < hb.len() && hb[j] <= cutoff {
        if ha[i] == hb[j] {
            let w = wa[i];
            both += f64::from(w);
            union += f64::from(w);
            shared.push((w, i));
            i += 1;
            j += 1;
        } else if ha[i] < hb[j] {
            union += f64::from(wa[i]);
            i += 1;
        } else {
            union += f64::from(wb[j]);
            j += 1;
        }
    }
    while i < ha.len() && ha[i] <= cutoff {
        union += f64::from(wa[i]);
        i += 1;
    }
    while j < hb.len() && hb[j] <= cutoff {
        union += f64::from(wb[j]);
        j += 1;
    }
    shared.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
    let score = if union > 0.0 { both / union } else { 0.0 };
    // Scale the sampled overlap up to the whole of the smaller set.
    let approx = cutoff != u64::MAX;
    let count = if approx {
        let (seen, total) = if a.total <= b.total {
            (i, a.total)
        } else {
            (j, b.total)
        };
        ((shared.len() as f64) * (total as f64 / seen.max(1) as f64)).round() as usize
    } else {
        shared.len()
    };
    (
        score,
        shared.into_iter().map(|(_, k)| k).collect(),
        count,
        approx,
    )
}

// ----------------------------------------------------------- signatures ----

/// An entity a project mentions.
#[derive(Debug, Clone)]
pub struct EntityRef {
    pub id: Uuid,
    pub name: Arc<str>,
}

/// A project's text as an embedding: normalised, then stored in 8 bits per
/// dimension (768 bytes rather than 3 KB).
#[derive(Debug, Clone)]
pub struct Embedding(Box<[i8]>);

impl Embedding {
    pub fn new(v: &[f32]) -> Option<Self> {
        let norm = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        (norm > 0.0).then(|| {
            Embedding(
                v.iter()
                    .map(|x| {
                        ((f64::from(*x) / norm) * 127.0)
                            .round()
                            .clamp(-127.0, 127.0) as i8
                    })
                    .collect(),
            )
        })
    }

    fn cosine(&self, other: &Embedding) -> f64 {
        if self.0.len() != other.0.len() {
            return 0.0;
        }
        let (mut dot, mut na, mut nb) = (0i64, 0i64, 0i64);
        for (x, y) in self.0.iter().zip(other.0.iter()) {
            let (x, y) = (i64::from(*x), i64::from(*y));
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0 || nb == 0 {
            0.0
        } else {
            (dot as f64 / ((na as f64).sqrt() * (nb as f64).sqrt())).clamp(0.0, 1.0)
        }
    }
}

/// What a project is made of, as far as comparing it goes.
#[derive(Debug, Clone, Default)]
pub struct Signature {
    pub name: String,
    /// Files in Gather (hashes of artifact ids).
    pub files: Sample,
    /// File paths inside the project, lowercased (hashes).
    pub paths: Sample,
    /// Entities mentioned (hashes of ids), and who they are, in the same
    /// order.
    pub entities: Sample,
    entity_refs: Box<[EntityRef]>,
    /// Most used words (ids), with how often (dampened), sorted by id.
    pub terms: Terms,
    /// Average embedding of the project's text, when there is one.
    pub embedding: Option<Embedding>,
}

impl Signature {
    pub fn with_files<'a>(mut self, ids: impl IntoIterator<Item = &'a Uuid>) -> Self {
        self.files = Sample::new(ids.into_iter().map(hash_of), SAMPLE);
        self
    }

    pub fn with_paths<'a>(mut self, paths: impl IntoIterator<Item = &'a str>) -> Self {
        self.paths = Sample::new(paths.into_iter().map(hash_of), SAMPLE);
        self
    }

    pub fn with_entities(mut self, entities: impl IntoIterator<Item = (Uuid, Arc<str>)>) -> Self {
        let mut refs: Vec<(u64, EntityRef)> = entities
            .into_iter()
            .map(|(id, name)| (hash_of(&id), EntityRef { id, name }))
            .collect();
        refs.sort_by_key(|(h, _)| *h);
        refs.dedup_by_key(|(h, _)| *h);
        self.entities = Sample::new(refs.iter().map(|(h, _)| *h), SAMPLE_ENTITIES);
        refs.truncate(self.entities.hashes.len());
        self.entity_refs = refs.into_iter().map(|(_, e)| e).collect();
        self
    }

    /// Words with how often each is used.
    pub fn with_terms<'a>(mut self, terms: impl IntoIterator<Item = (&'a str, f32)>) -> Self {
        self.terms = Terms::new(terms);
        self
    }
}

/// Inverse document frequency: of `projects` that could have an item,
/// `holders` do; rarer items weigh more. Never zero, so even an item every
/// project has still counts a little.
fn idf(projects: usize, holders: Option<&u32>) -> f32 {
    let holders = holders.copied().unwrap_or(1).max(1) as f64;
    (((projects as f64 + 1.0) / holders).ln() + 0.1) as f32
}

/// How many samples hold each item, and the cutoff of every sample.
///
/// A sampled project only keeps items with hashes below its cutoff, so it
/// can't show whether it has an item above it. Counting holders among all
/// projects would make a common item with a high hash look rare (the big
/// projects that have it dropped it from their samples); it is counted
/// instead among the projects whose samples would hold it if they had it.
#[derive(Debug, Default)]
struct Holders {
    counts: Counts<u64>,
    /// Sorted.
    cutoffs: Vec<u64>,
}

impl Holders {
    fn add(&mut self, s: &Sample) {
        for h in s.hashes.iter() {
            *self.counts.entry(*h).or_default() += 1;
        }
        self.cutoffs.push(s.cutoff());
    }

    fn weight(&self, h: u64) -> f32 {
        let eligible = self.cutoffs.len() - self.cutoffs.partition_point(|c| *c < h);
        idf(eligible, self.counts.get(&h))
    }
}

/// How many projects have each item: the rarity weights.
#[derive(Debug, Default)]
pub struct Rarity {
    projects: usize,
    files: Holders,
    paths: Holders,
    entities: Holders,
    terms: Counts<u32>,
}

impl Rarity {
    pub fn of<'a>(signatures: impl IntoIterator<Item = &'a Signature>) -> Self {
        let mut r = Rarity::default();
        for s in signatures {
            r.projects += 1;
            r.files.add(&s.files);
            r.paths.add(&s.paths);
            r.entities.add(&s.entities);
            for (t, _) in s.terms.iter() {
                *r.terms.entry(*t).or_default() += 1;
            }
        }
        for h in [&mut r.files, &mut r.paths, &mut r.entities] {
            h.cutoffs.sort_unstable();
        }
        r
    }
}

/// A signature's items weighted by rarity, in the signature's order:
/// worked out once per change, so a comparison is a walk over numbers.
#[derive(Debug, Default)]
pub struct Weights {
    files: Box<[f32]>,
    paths: Box<[f32]>,
    entities: Box<[f32]>,
    /// Each word's count times its rarity.
    terms: Box<[f32]>,
    terms_norm: f64,
}

impl Weights {
    pub fn of(s: &Signature, r: &Rarity) -> Self {
        let terms: Box<[f32]> = s
            .terms
            .iter()
            .map(|(t, n)| n * idf(r.projects, r.terms.get(t)))
            .collect();
        Weights {
            files: s.files.hashes.iter().map(|h| r.files.weight(*h)).collect(),
            paths: s.paths.hashes.iter().map(|h| r.paths.weight(*h)).collect(),
            entities: s
                .entities
                .hashes
                .iter()
                .map(|h| r.entities.weight(*h))
                .collect(),
            terms_norm: terms
                .iter()
                .map(|x| f64::from(*x).powi(2))
                .sum::<f64>()
                .sqrt(),
            terms,
        }
    }
}

/// Cosine over rarity-weighted word counts, with the shared words that
/// count most.
fn term_cosine(a: &Signature, wa: &Weights, b: &Signature, wb: &Weights) -> (f64, Vec<u32>) {
    if wa.terms_norm == 0.0 || wb.terms_norm == 0.0 {
        return (0.0, Vec::new());
    }
    let (ta, tb) = (&a.terms, &b.terms);
    let (mut i, mut j) = (0, 0);
    let mut dot = 0.0f64;
    let mut shared: Vec<(f64, u32)> = Vec::new();
    while i < ta.len() && j < tb.len() {
        match ta[i].0.cmp(&tb[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let p = f64::from(wa.terms[i]) * f64::from(wb.terms[j]);
                dot += p;
                shared.push((p, ta[i].0));
                i += 1;
                j += 1;
            }
        }
    }
    shared.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
    (
        (dot / (wa.terms_norm * wb.terms_norm)).clamp(0.0, 1.0),
        shared.into_iter().map(|(_, t)| t).collect(),
    )
}

// ------------------------------------------------------------- compare ----

/// The score of each signal that both projects have; `None` for one that
/// doesn't apply.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Signals {
    pub files: Option<f64>,
    pub layout: Option<f64>,
    pub entities: Option<f64>,
    pub content: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SharedEntity {
    pub id: Uuid,
    pub name: String,
}

/// What the two projects have in common, for the reasons.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Shared {
    /// Identical files.
    pub files: usize,
    /// Paths found in both.
    pub paths: usize,
    /// A few of those paths (filled in when asked for, see [`path_examples`]).
    pub path_examples: Vec<String>,
    pub entities: Vec<SharedEntity>,
    /// Words both use a lot (only when the content signal is words).
    pub terms: Vec<String>,
    /// Whether content was compared by meaning (embeddings) or by words.
    pub content_by: Option<&'static str>,
    /// True when the counts are estimated from samples of large projects.
    pub approximate: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// 0 to 1.
    pub score: f64,
    pub signals: Signals,
    pub shared: Shared,
    /// Plain-language reasons, strongest first.
    pub reasons: Vec<String>,
}

impl Comparison {
    /// Rebuild the reasons, after `shared` gained examples.
    pub fn refresh_reasons(&mut self) {
        self.reasons = reasons(&self.signals, &self.shared);
    }
}

/// Compare two projects, working out their weights first. For many
/// comparisons, [`Loaded`] keeps the weights and [`compare_weighted`] reuses
/// them.
pub fn compare(a: &Signature, b: &Signature, rarity: &Rarity) -> Comparison {
    compare_weighted(a, &Weights::of(a, rarity), b, &Weights::of(b, rarity))
}

/// Just the score: what ranking needs for every project, before the few it
/// keeps are described.
fn score_only(a: &Signature, wa: &Weights, b: &Signature, wb: &Weights) -> f64 {
    let mut total = 0.0;
    let mut sum = 0.0;
    if !a.files.is_empty() && !b.files.is_empty() {
        let (s, ..) = weighted_jaccard(&a.files, &wa.files, &b.files, &wb.files);
        total += WEIGHT_FILES;
        sum += WEIGHT_FILES * s;
    }
    if !a.paths.is_empty() && !b.paths.is_empty() {
        let (s, ..) = weighted_jaccard(&a.paths, &wa.paths, &b.paths, &wb.paths);
        total += WEIGHT_LAYOUT;
        sum += WEIGHT_LAYOUT * s;
    }
    if !a.entities.is_empty() && !b.entities.is_empty() {
        let (s, ..) = weighted_jaccard(&a.entities, &wa.entities, &b.entities, &wb.entities);
        total += WEIGHT_ENTITIES;
        sum += WEIGHT_ENTITIES * s;
    }
    if let Some(s) = content_score(a, wa, b, wb).map(|(s, _)| s) {
        total += WEIGHT_CONTENT;
        sum += WEIGHT_CONTENT * s;
    }
    if total > 0.0 {
        ((sum / total) * 1000.0).round() / 1000.0
    } else {
        0.0
    }
}

/// The content signal and how it was measured, when both projects have it.
fn content_score(
    a: &Signature,
    wa: &Weights,
    b: &Signature,
    wb: &Weights,
) -> Option<(f64, &'static str)> {
    match (&a.embedding, &b.embedding) {
        // Texts on any subject share a baseline of closeness, so only
        // what's above it counts.
        (Some(x), Some(y)) => Some((((x.cosine(y) - 0.5) / 0.5).clamp(0.0, 1.0), "meaning")),
        _ if !a.terms.is_empty() && !b.terms.is_empty() => {
            Some((term_cosine(a, wa, b, wb).0, "words"))
        }
        _ => None,
    }
}

/// Compare two projects whose weights are known.
pub fn compare_weighted(a: &Signature, wa: &Weights, b: &Signature, wb: &Weights) -> Comparison {
    let mut signals = Signals::default();
    let mut shared = Shared::default();

    if !a.files.is_empty() && !b.files.is_empty() {
        let (s, _, count, approx) = weighted_jaccard(&a.files, &wa.files, &b.files, &wb.files);
        signals.files = Some(s);
        shared.files = count;
        shared.approximate |= approx;
    }
    if !a.paths.is_empty() && !b.paths.is_empty() {
        let (s, _, count, approx) = weighted_jaccard(&a.paths, &wa.paths, &b.paths, &wb.paths);
        signals.layout = Some(s);
        shared.paths = count;
        shared.approximate |= approx;
    }
    if !a.entities.is_empty() && !b.entities.is_empty() {
        let (s, both, _, approx) =
            weighted_jaccard(&a.entities, &wa.entities, &b.entities, &wb.entities);
        signals.entities = Some(s);
        shared.approximate |= approx;
        shared.entities = both
            .into_iter()
            .filter_map(|k| a.entity_refs.get(k))
            .map(|e| SharedEntity {
                id: e.id,
                name: e.name.to_string(),
            })
            .collect();
    }
    if let Some((s, by)) = content_score(a, wa, b, wb) {
        signals.content = Some(s);
        shared.content_by = Some(by);
        if by == "words" {
            shared.terms = term_cosine(a, wa, b, wb)
                .1
                .into_iter()
                .take(EXAMPLES + 2)
                .map(word)
                .collect();
        }
    }

    let reasons = reasons(&signals, &shared);
    Comparison {
        score: score_only(a, wa, b, wb),
        signals,
        shared,
        reasons,
    }
}

fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn reasons(signals: &Signals, shared: &Shared) -> Vec<String> {
    let about = if shared.approximate { "About " } else { "" };
    let mut out: Vec<(f64, String)> = Vec::new();
    if let Some(s) = signals.files.filter(|_| shared.files > 0) {
        out.push((
            s,
            format!(
                "{about}{} in common",
                plural(shared.files, "identical file", "identical files")
            ),
        ));
    }
    if let Some(s) = signals.layout.filter(|_| shared.paths > 0) {
        let examples = if shared.path_examples.is_empty() {
            String::new()
        } else {
            format!(" ({})", list(&shared.path_examples))
        };
        out.push((
            s,
            format!(
                "{about}{} at the same place{examples}",
                plural(shared.paths, "file", "files")
            ),
        ));
    }
    if let Some(s) = signals.entities.filter(|_| !shared.entities.is_empty()) {
        let names: Vec<String> = shared
            .entities
            .iter()
            .take(EXAMPLES)
            .map(|e| e.name.clone())
            .collect();
        let more = shared.entities.len().saturating_sub(names.len());
        let tail = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        out.push((s, format!("Both mention {}{tail}", list(&names))));
    }
    match (signals.content, shared.content_by) {
        (Some(s), Some("meaning")) if s >= 0.2 => {
            out.push((s, "Their text is about similar things".to_string()));
        }
        (Some(s), Some("words")) if s >= 0.1 && !shared.terms.is_empty() => {
            out.push((s, format!("Similar wording ({})", list(&shared.terms))));
        }
        _ => {}
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter().map(|(_, r)| r).collect()
}

/// One project ranked against another.
#[derive(Debug, Clone, Serialize)]
pub struct Similar {
    pub project_id: Uuid,
    pub name: String,
    #[serde(flatten)]
    pub comparison: Comparison,
}

/// Signatures to compare, with the rarity weights they share.
#[derive(Debug, Default)]
pub struct Loaded {
    pub signatures: HashMap<Uuid, Arc<Signature>>,
    pub rarity: Arc<Rarity>,
    weights: HashMap<Uuid, Weights>,
}

impl Loaded {
    pub fn new(signatures: HashMap<Uuid, Arc<Signature>>) -> Self {
        let rarity = Arc::new(Rarity::of(signatures.values().map(|s| s.as_ref())));
        let weights = signatures
            .iter()
            .map(|(id, s)| (*id, Weights::of(s, &rarity)))
            .collect();
        Loaded {
            signatures,
            rarity,
            weights,
        }
    }

    fn get(&self, id: &Uuid) -> Option<(&Signature, &Weights)> {
        Some((self.signatures.get(id)?, self.weights.get(id)?))
    }
}

/// The projects most like `project`, best first, at most `limit`, each at
/// least [`MIN_SCORE`]. Linear in the number of projects: every one is
/// scored, and only the ones kept are described.
pub fn rank(project: Uuid, loaded: &Loaded, limit: usize) -> Option<Vec<Similar>> {
    let (me, mw) = loaded.get(&project)?;
    let mut scored: Vec<(f64, &str, Uuid)> = loaded
        .signatures
        .iter()
        .filter(|(id, _)| **id != project)
        .filter_map(|(id, other)| {
            let score = score_only(me, mw, other, &loaded.weights[id]);
            (score >= MIN_SCORE).then_some((score, other.name.as_str(), *id))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.truncate(limit);
    Some(
        scored
            .into_iter()
            .map(|(_, name, id)| {
                let (other, ow) = loaded.get(&id).expect("scored above");
                Similar {
                    project_id: id,
                    name: name.to_string(),
                    comparison: compare_weighted(me, mw, other, ow),
                }
            })
            .collect(),
    )
}

/// A pair of similar projects, for the graph.
#[derive(Debug, Clone, Serialize)]
pub struct Pair {
    pub a: Uuid,
    pub b: Uuid,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// Every pair among `among` at least [`MIN_SCORE`] alike, weighted by
/// rarity across all loaded projects, keeping for each project only its
/// `per_project` closest, best first. Quadratic in `among` only.
pub fn pairs(loaded: &Loaded, among: &[Uuid], per_project: usize) -> Vec<Pair> {
    let mut ids: Vec<Uuid> = among
        .iter()
        .copied()
        .filter(|id| loaded.signatures.contains_key(id))
        .collect();
    ids.sort();
    ids.dedup();
    let mut scored = Vec::new();
    for (i, a) in ids.iter().enumerate() {
        let (sa, wa) = loaded.get(a).expect("filtered above");
        for b in &ids[i + 1..] {
            let (sb, wb) = loaded.get(b).expect("filtered above");
            let score = score_only(sa, wa, sb, wb);
            if score >= MIN_SCORE {
                scored.push((score, *a, *b));
            }
        }
    }
    scored.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then_with(|| (x.1, x.2).cmp(&(y.1, y.2)))
    });
    let mut kept: HashMap<Uuid, usize> = HashMap::new();
    scored.retain(|(_, a, b)| {
        let (ka, kb) = (
            kept.get(a).copied().unwrap_or(0),
            kept.get(b).copied().unwrap_or(0),
        );
        // A pair stays when it is among the closest of either project.
        if ka < per_project || kb < per_project {
            *kept.entry(*a).or_default() += 1;
            *kept.entry(*b).or_default() += 1;
            true
        } else {
            false
        }
    });
    scored
        .into_iter()
        .map(|(score, a, b)| {
            let (sa, wa) = loaded.get(&a).expect("kept above");
            let (sb, wb) = loaded.get(&b).expect("kept above");
            Pair {
                a,
                b,
                score,
                reasons: compare_weighted(sa, wa, sb, wb).reasons,
            }
        })
        .collect()
}

/// A few paths found in both projects, for a reason's examples. Signatures
/// keep only hashes of paths, so the names come from the database.
pub async fn path_examples(pool: &PgPool, a: Uuid, b: Uuid) -> Result<Vec<String>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT lower(x.path) AS path FROM project_items x \
         JOIN project_items y ON y.project_id = $2 AND y.item_kind = 'file' \
          AND lower(y.path) = lower(x.path) \
         WHERE x.project_id = $1 AND x.item_kind = 'file' \
         ORDER BY path LIMIT $3",
    )
    .bind(a)
    .bind(b)
    .bind(EXAMPLES as i64)
    .fetch_all(pool)
    .await?)
}

/// Fill in path examples for ranked results, and their reasons.
pub async fn with_examples(
    pool: &PgPool,
    project: Uuid,
    mut items: Vec<Similar>,
) -> Result<Vec<Similar>, ApiError> {
    for s in &mut items {
        if s.comparison.shared.paths > 0 {
            s.comparison.shared.path_examples = path_examples(pool, project, s.project_id).await?;
            s.comparison.refresh_reasons();
        }
    }
    Ok(items)
}

// ------------------------------------------------------------ loading ----

/// What decides whether a cached signature is still current: the project
/// changed, or more has been read from its files since.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    semantic_revision: Arc<[i64]>,
    updated_at: DateTime<Utc>,
    units: i64,
    segments: i64,
    embedded: i64,
}

#[derive(Default)]
struct Store {
    entries: HashMap<Uuid, (Fingerprint, Arc<Signature>)>,
    /// What the last load returned, while nothing has changed.
    loaded: Option<Arc<Loaded>>,
    /// When the fingerprints were last read, for how many projects, and at
    /// which count of project writes.
    checked: Option<(Instant, usize, u64, Arc<[i64]>)>,
    /// The last graph pairs: for which projects, and the pairs, worked out
    /// from `loaded` (cleared whenever it is replaced).
    pairs: Option<(u64, usize, Arc<Vec<Pair>>)>,
}

static STORE: LazyLock<tokio::sync::Mutex<Store>> = LazyLock::new(Default::default);
/// Bumped whenever a project is written to, so the next load looks again
/// at once instead of trusting a recent check.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// A project was created, changed or removed.
pub fn touch() {
    WRITES.fetch_add(1, Ordering::Relaxed);
}

/// Signatures of the projects to compare (the most recently changed, at
/// most `max`), from the cache where it is current.
pub async fn load(pool: &PgPool, max: usize) -> Result<Arc<Loaded>, ApiError> {
    let mut store = STORE.lock().await;
    let writes = WRITES.load(Ordering::Relaxed);
    let revision: Arc<[i64]> =
        sqlx::query_scalar("SELECT revision FROM gather_semantic_revisions ORDER BY revision")
            .fetch_all(pool)
            .await?
            .into();
    if let (Some(loaded), Some((at, n, w, r))) = (&store.loaded, &store.checked) {
        if at.elapsed() < RECHECK && *n == max && *w == writes && r == &revision {
            return Ok(loaded.clone());
        }
    }

    // One grouped query for every project's fingerprint.
    let rows = sqlx::query(
        "WITH p AS ( \
             SELECT id, name, updated_at FROM projects \
              ORDER BY updated_at DESC, id LIMIT $1), \
         f AS ( \
             SELECT DISTINCT i.project_id, i.artifact_id FROM project_items i \
               JOIN p ON p.id = i.project_id WHERE i.artifact_id IS NOT NULL), \
         u AS ( \
             SELECT f.project_id, count(*) AS n FROM f \
               JOIN atomic_unit_provenance pv ON pv.artifact_id = f.artifact_id \
              GROUP BY 1), \
         s AS ( \
             SELECT f.project_id, count(*) AS n, count(s.embedding) AS e FROM f \
               JOIN documents d ON d.artifact_id = f.artifact_id \
               JOIN document_segments s ON s.document_id = d.id \
              GROUP BY 1) \
         SELECT p.id, p.name, p.updated_at, coalesce(u.n, 0) AS units, \
                coalesce(s.n, 0) AS segments, coalesce(s.e, 0) AS embedded \
         FROM p LEFT JOIN u ON u.project_id = p.id LEFT JOIN s ON s.project_id = p.id",
    )
    .bind(max as i64)
    .fetch_all(pool)
    .await?;
    let current: Vec<(Uuid, String, Fingerprint)> = rows
        .iter()
        .map(|r| {
            (
                r.get("id"),
                r.get("name"),
                Fingerprint {
                    semantic_revision: revision.clone(),
                    updated_at: r.get("updated_at"),
                    units: r.get("units"),
                    segments: r.get("segments"),
                    embedded: r.get("embedded"),
                },
            )
        })
        .collect();

    let stale: Vec<Uuid> = current
        .iter()
        .filter(|(id, _, fp)| store.entries.get(id).map_or(true, |(old, _)| old != fp))
        .map(|(id, _, _)| *id)
        .collect();
    let mut fresh = HashMap::new();
    for chunk in stale.chunks(BATCH) {
        fresh.extend(compute(pool, chunk).await?);
    }

    let keep: HashSet<Uuid> = current.iter().map(|(id, _, _)| *id).collect();
    let before = store.entries.len();
    store.entries.retain(|id, _| keep.contains(id));
    let changed = !fresh.is_empty() || store.entries.len() != before;
    let mut renamed = false;
    for (id, name, fp) in current {
        if let Some(mut sig) = fresh.remove(&id) {
            sig.name = name;
            store.entries.insert(id, (fp, Arc::new(sig)));
        } else if let Some((_, sig)) = store.entries.get_mut(&id) {
            if sig.name != name {
                Arc::make_mut(sig).name = name;
                renamed = true;
            }
        }
    }
    if changed || renamed || store.loaded.is_none() {
        let signatures = store
            .entries
            .iter()
            .map(|(id, (_, sig))| (*id, sig.clone()))
            .collect();
        // Rebuilt whole, weights included: a rename alone changes no weight,
        // but rebuilding is cheap enough not to single it out.
        store.loaded = Some(Arc::new(Loaded::new(signatures)));
        store.pairs = None;
    }
    if changed {
        prune_names();
    }
    store.checked = Some((Instant::now(), max, writes, revision));
    Ok(store.loaded.clone().expect("set above"))
}

/// Graph pairs among `among`, remembered until a project changes.
pub async fn cached_pairs(
    pool: &PgPool,
    max: usize,
    among: &[Uuid],
    per_project: usize,
) -> Result<Arc<Vec<Pair>>, ApiError> {
    let loaded = load(pool, max).await?;
    let mut ids = among.to_vec();
    ids.sort();
    let key = hash_of(&ids);
    let current = |store: &Store| {
        store
            .loaded
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(l, &loaded))
    };
    {
        let store = STORE.lock().await;
        if let (true, Some((k, per, pairs))) = (current(&store), &store.pairs) {
            if *k == key && *per == per_project {
                return Ok(pairs.clone());
            }
        }
    }
    // Worked out without holding the cache, so loads aren't kept waiting;
    // kept only if no newer load replaced the signatures meanwhile.
    let computed = Arc::new(pairs(&loaded, &ids, per_project));
    let mut store = STORE.lock().await;
    if current(&store) {
        store.pairs = Some((key, per_project, computed.clone()));
    }
    Ok(computed)
}

/// Build the signatures of `ids` from the database.
async fn compute(pool: &PgPool, ids: &[Uuid]) -> Result<HashMap<Uuid, Signature>, ApiError> {
    #[derive(Default)]
    struct Raw {
        files: Vec<u64>,
        paths: Vec<u64>,
        entities: Vec<(Uuid, Arc<str>)>,
        terms: Vec<(String, f32)>,
        embedding: Option<Embedding>,
    }
    let mut raw: HashMap<Uuid, Raw> = ids.iter().map(|id| (*id, Raw::default())).collect();

    let files = sqlx::query(
        "SELECT project_id, artifact_id, lower(path) AS path FROM project_items \
         WHERE project_id = ANY($1) AND item_kind = 'file' \
           AND status IN ('ingested', 'deduplicated', 'stored')",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    for r in &files {
        let entry = raw
            .get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for");
        if let Some(a) = r.get::<Option<Uuid>, _>("artifact_id") {
            entry.files.push(hash_of(&a));
        }
        entry.paths.push(hash_of(r.get::<&str, _>("path")));
    }
    drop(files);

    let entities = sqlx::query(
        "WITH units AS ( \
             SELECT DISTINCT i.project_id, pv.atomic_unit_id \
               FROM project_items i \
               JOIN atomic_unit_provenance pv ON pv.artifact_id = i.artifact_id \
              WHERE i.project_id = ANY($1)) \
         SELECT DISTINCT x.project_id, e.id, e.name FROM ( \
             SELECT un.project_id, u.subject_entity_id AS entity_id \
               FROM units un JOIN atomic_units u ON u.id = un.atomic_unit_id \
              WHERE u.status IN ('active', 'disputed') AND u.subject_entity_id IS NOT NULL \
             UNION ALL \
             SELECT un.project_id, y.entity_id \
               FROM units un \
               JOIN relationships r ON r.atomic_unit_id = un.atomic_unit_id \
              CROSS JOIN LATERAL (VALUES (r.source_entity_id), (r.target_entity_id)) y(entity_id) \
              WHERE r.status = 'active') x \
         JOIN entities e ON e.id = x.entity_id AND e.merged_into_entity_id IS NULL",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    for r in &entities {
        raw.get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for")
            .entities
            .push((r.get("id"), intern(r.get::<&str, _>("name"))));
    }
    drop(entities);

    // The words each project uses most, from a bounded sample of its text:
    // whole words as written (lowercased), so the reasons read naturally,
    // without English stop words, short words and numbers, which say little
    // about what a project is about.
    let terms = sqlx::query(
        "WITH segs AS ( \
             SELECT i.project_id, s.content, \
                    row_number() OVER (PARTITION BY i.project_id ORDER BY s.id) AS n \
               FROM project_items i \
               JOIN documents d ON d.artifact_id = i.artifact_id \
               JOIN document_segments s ON s.document_id = d.id \
              WHERE i.project_id = ANY($1)), \
         counts AS ( \
             SELECT project_id, w.lexeme, \
                    sum(coalesce(array_length(w.positions, 1), 1))::float8 AS n \
               FROM segs, unnest(to_tsvector('simple', left(content, 20000))) w \
              WHERE segs.n <= $2 AND length(w.lexeme) BETWEEN 3 AND 40 \
                AND w.lexeme ~ '^[[:alpha:]][[:alnum:]_-]*$' \
              GROUP BY 1, 2), \
         words AS ( \
             SELECT * FROM counts \
              WHERE plainto_tsquery('english', lexeme)::text <> ''), \
         ranked AS ( \
             SELECT project_id, lexeme, n, \
                    row_number() OVER (PARTITION BY project_id ORDER BY n DESC, lexeme) AS r \
               FROM words) \
         SELECT project_id, lexeme, n FROM ranked WHERE r <= $3",
    )
    .bind(ids)
    .bind(MAX_SEGMENTS)
    .bind(TOP_TERMS)
    .fetch_all(pool)
    .await?;
    for r in &terms {
        // Dampened, so one word repeated in a long file doesn't drown the rest.
        raw.get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for")
            .terms
            .push((
                r.get::<String, _>("lexeme"),
                (1.0 + r.get::<f64, _>("n")).ln() as f32,
            ));
    }
    drop(terms);

    let embeddings = sqlx::query(
        "SELECT i.project_id, avg(s.embedding) AS centroid \
           FROM project_items i \
           JOIN documents d ON d.artifact_id = i.artifact_id \
           JOIN document_segments s ON s.document_id = d.id \
          WHERE i.project_id = ANY($1) AND s.embedding IS NOT NULL \
          GROUP BY 1",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    for r in &embeddings {
        raw.get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for")
            .embedding = r
            .get::<Option<pgvector::Vector>, _>("centroid")
            .and_then(|v| Embedding::new(v.as_slice()));
    }

    Ok(raw
        .into_iter()
        .map(|(id, r)| {
            let mut sig = Signature::default()
                .with_entities(r.entities)
                .with_terms(r.terms.iter().map(|(t, n)| (t.as_str(), *n)));
            sig.files = Sample::new(r.files, SAMPLE);
            sig.paths = Sample::new(r.paths, SAMPLE);
            sig.embedding = r.embedding;
            (id, sig)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(files: &[u128], paths: &[&str], entities: &[(u128, &str)], terms: &[&str]) -> Signature {
        let files: Vec<Uuid> = files.iter().map(|f| Uuid::from_u128(*f)).collect();
        Signature::default()
            .with_files(&files)
            .with_paths(paths.iter().copied())
            .with_entities(
                entities
                    .iter()
                    .map(|(id, n)| (Uuid::from_u128(*id), intern(n))),
            )
            .with_terms(terms.iter().map(|t| (*t, 1.0)))
    }

    fn loaded(sigs: Vec<(u128, Signature)>) -> Loaded {
        Loaded::new(
            sigs.into_iter()
                .map(|(id, s)| (Uuid::from_u128(id), Arc::new(s)))
                .collect(),
        )
    }

    #[test]
    fn identical_projects_score_one_and_unrelated_ones_zero() {
        let a = sig(
            &[1, 2],
            &["readme.md", "src/main.rs"],
            &[(10, "Dana")],
            &["budget"],
        );
        let b = a.clone();
        let c = sig(&[3], &["photo.jpg"], &[(11, "Lee")], &["garden"]);
        let r = Rarity::of([&a, &b, &c]);
        let same = compare(&a, &b, &r);
        assert_eq!(same.score, 1.0);
        assert_eq!(same.shared.files, 2);
        assert!(!same.shared.approximate);
        assert!(
            same.reasons[0].contains("identical file"),
            "{:?}",
            same.reasons
        );
        let apart = compare(&a, &c, &r);
        assert_eq!(apart.score, 0.0);
        assert!(apart.reasons.is_empty());
    }

    #[test]
    fn two_versions_of_a_repository_match_by_layout() {
        // Every file changed, so no identical files, but the paths line up.
        let v1 = sig(
            &[1, 2, 3],
            &["cargo.toml", "src/main.rs", "src/db.rs"],
            &[],
            &[],
        );
        let v2 = sig(
            &[4, 5, 6],
            &["cargo.toml", "src/main.rs", "src/db.rs"],
            &[],
            &[],
        );
        let other = sig(&[7], &["notes.txt"], &[], &[]);
        let r = Rarity::of([&v1, &v2, &other]);
        let mut c = compare(&v1, &v2, &r);
        assert_eq!(c.signals.files, Some(0.0));
        assert_eq!(c.signals.layout, Some(1.0));
        assert!(c.score >= 0.3, "{c:?}");
        assert!(c.reasons.contains(&"3 files at the same place".to_string()));
        c.shared.path_examples = vec!["cargo.toml".into(), "src/db.rs".into()];
        c.refresh_reasons();
        assert!(c
            .reasons
            .contains(&"3 files at the same place (cargo.toml and src/db.rs)".to_string()));
    }

    #[test]
    fn common_items_count_for_less_than_rare_ones() {
        // Everyone has a README and mentions "Me"; only a and b share "Atlas".
        let a = sig(
            &[],
            &["readme.md", "atlas.md"],
            &[(1, "Me"), (2, "Atlas")],
            &[],
        );
        let b = sig(
            &[],
            &["readme.md", "atlas.md"],
            &[(1, "Me"), (2, "Atlas")],
            &[],
        );
        let c = sig(
            &[],
            &["readme.md", "garden.md"],
            &[(1, "Me"), (3, "Garden")],
            &[],
        );
        let d = sig(
            &[],
            &["readme.md", "trip.md"],
            &[(1, "Me"), (4, "Trip")],
            &[],
        );
        let r = Rarity::of([&a, &b, &c, &d]);
        let close = compare(&a, &b, &r);
        let far = compare(&a, &c, &r);
        assert!(
            close.score > far.score * 3.0,
            "{} vs {}",
            close.score,
            far.score
        );
        // The rarer shared entity is named first.
        assert_eq!(close.shared.entities[0].name, "Atlas");
    }

    #[test]
    fn missing_signals_dont_count_against_a_project() {
        // Photos only: no text, no entities. Same files is a perfect match.
        let a = sig(&[1, 2], &["a.jpg", "b.jpg"], &[], &[]);
        let b = sig(&[1, 2], &["a.jpg", "b.jpg"], &[], &[]);
        let c = compare(&a, &b, &Rarity::of([&a, &b]));
        assert_eq!(c.signals.entities, None);
        assert_eq!(c.signals.content, None);
        assert_eq!(c.score, 1.0);
    }

    #[test]
    fn content_uses_meaning_when_both_have_it() {
        let mut a = sig(&[], &[], &[], &["budget"]);
        let mut b = sig(&[], &[], &[], &["garden"]);
        a.embedding = Embedding::new(&[1.0, 0.0]);
        b.embedding = Embedding::new(&[1.0, 0.1]);
        let c = compare(&a, &b, &Rarity::of([&a, &b]));
        assert_eq!(c.shared.content_by, Some("meaning"));
        assert!(c.signals.content.unwrap() > 0.9);
        // Without embeddings, the words decide.
        a.embedding = None;
        let c = compare(&a, &b, &Rarity::of([&a, &b]));
        assert_eq!(c.shared.content_by, Some("words"));
        assert_eq!(c.signals.content, Some(0.0));
    }

    #[test]
    fn ranking_and_pairs_keep_the_closest() {
        let base = sig(&[1, 2, 3], &["x.md", "y.md"], &[(9, "Atlas")], &["atlas"]);
        let mut half = base.clone();
        half.files = Sample::new(
            [Uuid::from_u128(1), Uuid::from_u128(50)]
                .iter()
                .map(hash_of),
            SAMPLE,
        );
        let all = loaded(vec![
            (100, base.clone()),
            (101, base.clone()),
            (102, half),
            (103, sig(&[70], &["z.md"], &[(8, "Lee")], &["garden"])),
        ]);
        let ranked = rank(Uuid::from_u128(100), &all, 10).unwrap();
        let order: Vec<u128> = ranked.iter().map(|s| s.project_id.as_u128()).collect();
        assert_eq!(order, vec![101, 102], "the unrelated one is left out");
        assert!(rank(Uuid::from_u128(999), &all, 10).is_none());

        let every: Vec<Uuid> = all.signatures.keys().copied().collect();
        let p = pairs(&all, &every, 1);
        assert!(p.iter().all(|x| x.score >= MIN_SCORE));
        assert_eq!((p[0].a.as_u128(), p[0].b.as_u128()), (100, 101));
        assert!(!p
            .iter()
            .any(|x| x.a.as_u128() == 103 || x.b.as_u128() == 103));
        // Pairs only among the projects asked about.
        let some = [Uuid::from_u128(100), Uuid::from_u128(102)];
        let p = pairs(&all, &some, 3);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].a.as_u128(), p[0].b.as_u128()), (100, 102));
    }

    #[test]
    fn rare_shared_words_count_for_more_than_common_ones() {
        // Everyone writes "project"; only a and b write "kepler".
        let a = sig(&[], &[], &[], &["project", "kepler"]);
        let b = sig(&[], &[], &[], &["project", "kepler"]);
        let c = sig(&[], &[], &[], &["project", "tomato"]);
        let d = sig(&[], &[], &[], &["project", "budget"]);
        let r = Rarity::of([&a, &b, &c, &d]);
        let close = compare(&a, &b, &r);
        let far = compare(&a, &c, &r);
        assert_eq!(close.shared.terms[0], "kepler");
        assert!(close.signals.content > far.signals.content);
        assert!(far.signals.content.unwrap() < 0.3, "{far:?}");
    }

    #[test]
    fn big_projects_are_sampled_and_still_compare_well() {
        // Two versions of a 5,000-file repository sharing 4,000 paths, and
        // one sharing none: samples of 512 keep the scores apart and the
        // estimated overlap close to the truth.
        let paths = |from: usize, to: usize| -> Vec<String> {
            (from..to).map(|i| format!("src/mod{i}.rs")).collect()
        };
        let a_paths = paths(0, 5000);
        let b_paths = paths(1000, 6000);
        let c_paths = paths(10_000, 15_000);
        let mk = |p: &[String]| Signature::default().with_paths(p.iter().map(|s| s.as_str()));
        let (a, b, c) = (mk(&a_paths), mk(&b_paths), mk(&c_paths));
        assert_eq!(a.paths.len(), 5000);
        assert_eq!(a.paths.hashes.len(), SAMPLE);
        let r = Rarity::of([&a, &b, &c]);
        let close = compare(&a, &b, &r);
        let far = compare(&a, &c, &r);
        assert!(close.shared.approximate);
        let est = close.shared.paths as f64;
        assert!((3000.0..=5000.0).contains(&est), "estimated {est} of 4000");
        assert!(close.signals.layout.unwrap() > 0.4, "{close:?}");
        assert_eq!(far.signals.layout, Some(0.0));
        assert!(
            close.reasons[0].starts_with("About "),
            "{:?}",
            close.reasons
        );
    }

    /// How long ranking and graph pairs take at scale, and how big the
    /// signatures are. `cargo test --release scale -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scale() {
        let words: Vec<String> = (0..20_000).map(|i| format!("word{i}")).collect();
        let started = Instant::now();
        let mut sigs = HashMap::new();
        for p in 0..5000u128 {
            let n = 50 + (p as usize * 37) % 2000; // 50 to 2,049 files
            let files: Vec<Uuid> = (0..n as u128)
                .map(|i| Uuid::from_u128(p * 10_000 + i))
                .collect();
            let paths: Vec<String> = (0..n)
                .map(|i| format!("dir{}/file{i}.md", p % 40))
                .collect();
            let s = Signature::default()
                .with_files(&files)
                .with_paths(paths.iter().map(|s| s.as_str()))
                .with_entities((0..40).map(|i| {
                    (
                        Uuid::from_u128(1_000_000 + (p * 7 + i) % 3000),
                        intern("Someone"),
                    )
                }))
                .with_terms(
                    (0..200)
                        .map(|i| (words[(p as usize * 13 + i * 7) % words.len()].as_str(), 1.0)),
                );
            sigs.insert(Uuid::from_u128(p), Arc::new(s));
        }
        let built = started.elapsed();
        let bytes: usize = sigs
            .values()
            .map(|s| {
                // Hashes, entity refs (id + name pointer), word ids and
                // counts, plus the per-item weights a load keeps.
                let items = s.files.hashes.len() + s.paths.hashes.len() + s.entities.hashes.len();
                items * (8 + 4) + s.entity_refs.len() * 32 + s.terms.len() * (8 + 4)
            })
            .sum();
        let t = Instant::now();
        let all = Loaded::new(sigs);
        let rarity = t.elapsed();
        let t = Instant::now();
        let ranked = rank(Uuid::from_u128(7), &all, 10).unwrap();
        let ranking = t.elapsed();
        let shown: Vec<Uuid> = (0..400).map(Uuid::from_u128).collect();
        let t = Instant::now();
        let p = pairs(&all, &shown, 3);
        let pairing = t.elapsed();
        println!(
            "5000 projects: built {built:?}, ~{} KB of signatures, rarity {rarity:?}, \
             rank {ranking:?} ({} similar), pairs among 400 {pairing:?} ({} pairs)",
            bytes / 1024,
            ranked.len(),
            p.len()
        );
    }

    /// A path every project has, but whose hash sits above the cutoff of
    /// the big projects' samples, is still common, not rare.
    #[test]
    fn sampled_projects_dont_make_common_items_look_rare() {
        // A path with a high hash: every big sample (256 of 2,001) drops it.
        let common = (0..)
            .map(|k| format!("readme{k}.md"))
            .find(|p| hash_of(p.as_str()) > u64::MAX / 2)
            .unwrap();
        let small = |p: u128| {
            Signature::default().with_paths(
                [common.clone(), format!("only{p}.md")]
                    .iter()
                    .map(|s| s.as_str()),
            )
        };
        let big = |p: u128| {
            let paths: Vec<String> = (0..2000)
                .map(|i| format!("big{p}/f{i}.md"))
                .chain([common.clone()])
                .collect();
            Signature::default().with_paths(paths.iter().map(|s| s.as_str()))
        };
        let smalls: Vec<Signature> = (0..3).map(small).collect();
        let bigs: Vec<Signature> = (0..20).map(big).collect();
        let readme = hash_of(common.as_str());
        assert!(bigs.iter().all(|b| b.paths.cutoff() < readme));
        let r = Rarity::of(smalls.iter().chain(bigs.iter()));
        let weight = r.paths.weight(readme);
        let rare = r.paths.weight(hash_of("only0.md"));
        assert!(weight < rare / 2.0, "common {weight}, a one-off {rare}");
    }

    #[test]
    fn unused_words_are_forgotten() {
        let a = Signature::default().with_terms([("reclaimed-word-abc", 1.0)]);
        let id = a.terms[0].0;
        assert_eq!(word(id), "reclaimed-word-abc");
        let b = a.clone();
        drop(a);
        assert_eq!(word(id), "reclaimed-word-abc", "still used by the clone");
        drop(b);
        assert!(!words().ids.contains_key("reclaimed-word-abc"));
    }

    #[test]
    fn strings_are_interned_once() {
        let a = intern("interned-word-xyz");
        let b = intern("interned-word-xyz");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn reasons_read_naturally() {
        assert_eq!(list(&["a".into(), "b".into(), "c".into()]), "a, b and c");
        assert_eq!(plural(1, "file", "files"), "1 file");
        let s = Shared {
            entities: (0..6)
                .map(|i| SharedEntity {
                    id: Uuid::from_u128(i),
                    name: format!("E{i}"),
                })
                .collect(),
            ..Shared::default()
        };
        let r = reasons(
            &Signals {
                entities: Some(0.5),
                ..Signals::default()
            },
            &s,
        );
        assert_eq!(r, vec!["Both mention E0, E1, E2 and E3 and 2 more"]);
    }
}
