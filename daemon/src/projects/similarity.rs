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
//! The scoring here is pure; [`load`] fetches what it scores from the
//! database and keeps it cached until a project changes.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

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
/// Words kept per project for the content signal.
const TOP_TERMS: i64 = 300;
/// Segments read per project for its words, so one enormous project can't
/// make every comparison slow.
const MAX_SEGMENTS: i64 = 5_000;
/// Below this, two projects aren't called similar.
pub const MIN_SCORE: f64 = 0.08;
/// Examples named in a reason.
const EXAMPLES: usize = 4;
/// Projects compared at most (the most recently changed).
const MAX_PROJECTS: i64 = 500;

/// What a project is made of, as far as comparing it goes.
#[derive(Debug, Clone, Default)]
pub struct Signature {
    pub name: String,
    /// Files in Gather (artifact ids).
    pub files: HashSet<Uuid>,
    /// File paths inside the project, lowercased.
    pub paths: HashSet<String>,
    /// Entities mentioned, with their names.
    pub entities: HashMap<Uuid, String>,
    /// Most used words (stemmed), with how often.
    pub terms: HashMap<String, f64>,
    /// Average embedding of the project's text, when there is one.
    pub embedding: Option<Vec<f32>>,
}

/// How many projects have each item: the rarity weights.
#[derive(Debug, Default)]
pub struct Rarity {
    projects: usize,
    files: HashMap<Uuid, usize>,
    paths: HashMap<String, usize>,
    entities: HashMap<Uuid, usize>,
    terms: HashMap<String, usize>,
}

impl Rarity {
    pub fn of<'a>(signatures: impl IntoIterator<Item = &'a Signature>) -> Self {
        let mut r = Rarity::default();
        for s in signatures {
            r.projects += 1;
            for f in &s.files {
                *r.files.entry(*f).or_default() += 1;
            }
            for p in &s.paths {
                *r.paths.entry(p.clone()).or_default() += 1;
            }
            for e in s.entities.keys() {
                *r.entities.entry(*e).or_default() += 1;
            }
            for t in s.terms.keys() {
                *r.terms.entry(t.clone()).or_default() += 1;
            }
        }
        r
    }

    /// Inverse document frequency: rarer items weigh more. Never zero, so
    /// even an item every project has still counts a little.
    fn weight(&self, holders: usize) -> f64 {
        ((self.projects as f64 + 1.0) / holders.max(1) as f64).ln() + 0.1
    }
}

/// The overlap of two sets, weighted by rarity: the weight of what both
/// hold over the weight of what either holds. With the shared items, rarest
/// first.
fn weighted_jaccard<T: Eq + std::hash::Hash + Clone + Ord>(
    a: &HashSet<T>,
    b: &HashSet<T>,
    weight: impl Fn(&T) -> f64,
) -> (f64, Vec<T>) {
    let mut shared: Vec<(f64, T)> = Vec::new();
    let mut union = 0.0;
    for x in a.union(b) {
        let w = weight(x);
        union += w;
        if a.contains(x) && b.contains(x) {
            shared.push((w, x.clone()));
        }
    }
    let both: f64 = shared.iter().map(|(w, _)| w).sum();
    shared.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
    let score = if union > 0.0 { both / union } else { 0.0 };
    (score, shared.into_iter().map(|(_, x)| x).collect())
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())).clamp(0.0, 1.0)
    }
}

/// Cosine over word counts, each word weighted by its rarity (`weight`),
/// with the shared words that count most.
fn term_cosine(
    a: &HashMap<String, f64>,
    b: &HashMap<String, f64>,
    weight: impl Fn(&str) -> f64,
) -> (f64, Vec<String>) {
    let norm = |m: &HashMap<String, f64>| {
        m.iter()
            .map(|(t, v)| (v * weight(t)).powi(2))
            .sum::<f64>()
            .sqrt()
    };
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        return (0.0, Vec::new());
    }
    let mut shared: Vec<(f64, &String)> = a
        .iter()
        .filter_map(|(t, x)| b.get(t).map(|y| (x * y * weight(t).powi(2), t)))
        .collect();
    let dot: f64 = shared.iter().map(|(p, _)| p).sum();
    shared.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(y.1)));
    (
        (dot / (na * nb)).clamp(0.0, 1.0),
        shared.into_iter().map(|(_, t)| t.clone()).collect(),
    )
}

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
    pub path_examples: Vec<String>,
    pub entities: Vec<SharedEntity>,
    /// Words both use a lot (only when the content signal is words).
    pub terms: Vec<String>,
    /// Whether content was compared by meaning (embeddings) or by words.
    pub content_by: Option<&'static str>,
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

/// Compare two projects.
pub fn compare(a: &Signature, b: &Signature, rarity: &Rarity) -> Comparison {
    let mut signals = Signals::default();
    let mut shared = Shared::default();
    let mut weighted = Vec::new();

    if !a.files.is_empty() && !b.files.is_empty() {
        let (s, both) = weighted_jaccard(&a.files, &b.files, |f| {
            rarity.weight(rarity.files.get(f).copied().unwrap_or(1))
        });
        signals.files = Some(s);
        shared.files = both.len();
        weighted.push((WEIGHT_FILES, s));
    }
    if !a.paths.is_empty() && !b.paths.is_empty() {
        let (s, both) = weighted_jaccard(&a.paths, &b.paths, |p| {
            rarity.weight(rarity.paths.get(p).copied().unwrap_or(1))
        });
        signals.layout = Some(s);
        shared.paths = both.len();
        shared.path_examples = both.into_iter().take(EXAMPLES).collect();
        weighted.push((WEIGHT_LAYOUT, s));
    }
    if !a.entities.is_empty() && !b.entities.is_empty() {
        let ka: HashSet<Uuid> = a.entities.keys().copied().collect();
        let kb: HashSet<Uuid> = b.entities.keys().copied().collect();
        let (s, both) = weighted_jaccard(&ka, &kb, |e| {
            rarity.weight(rarity.entities.get(e).copied().unwrap_or(1))
        });
        signals.entities = Some(s);
        shared.entities = both
            .into_iter()
            .map(|id| SharedEntity {
                id,
                name: a.entities[&id].clone(),
            })
            .collect();
        weighted.push((WEIGHT_ENTITIES, s));
    }
    match (&a.embedding, &b.embedding) {
        (Some(x), Some(y)) => {
            // Texts on any subject share a baseline of closeness, so only
            // what's above it counts.
            let s = ((cosine(x, y) - 0.5) / 0.5).clamp(0.0, 1.0);
            signals.content = Some(s);
            shared.content_by = Some("meaning");
            weighted.push((WEIGHT_CONTENT, s));
        }
        _ if !a.terms.is_empty() && !b.terms.is_empty() => {
            let (s, both) = term_cosine(&a.terms, &b.terms, |t| {
                rarity.weight(rarity.terms.get(t).copied().unwrap_or(1))
            });
            signals.content = Some(s);
            shared.terms = both.into_iter().take(EXAMPLES + 2).collect();
            shared.content_by = Some("words");
            weighted.push((WEIGHT_CONTENT, s));
        }
        _ => {}
    }

    let total: f64 = weighted.iter().map(|(w, _)| w).sum();
    let score = if total > 0.0 {
        weighted.iter().map(|(w, s)| w * s).sum::<f64>() / total
    } else {
        0.0
    };
    let reasons = reasons(&signals, &shared);
    Comparison {
        score: (score * 1000.0).round() / 1000.0,
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
    let mut out: Vec<(f64, String)> = Vec::new();
    if let Some(s) = signals.files.filter(|_| shared.files > 0) {
        out.push((
            s,
            format!(
                "{} in common",
                plural(shared.files, "identical file", "identical files")
            ),
        ));
    }
    if let Some(s) = signals.layout.filter(|_| shared.paths > 0) {
        out.push((
            s,
            format!(
                "{} at the same place ({})",
                plural(shared.paths, "file", "files"),
                list(&shared.path_examples)
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

/// The projects most like `project`, best first, at most `limit`, each at
/// least [`MIN_SCORE`].
pub fn rank(
    project: Uuid,
    signatures: &HashMap<Uuid, Signature>,
    limit: usize,
) -> Option<Vec<Similar>> {
    let me = signatures.get(&project)?;
    let rarity = Rarity::of(signatures.values());
    let mut out: Vec<Similar> = signatures
        .iter()
        .filter(|(id, _)| **id != project)
        .map(|(id, other)| Similar {
            project_id: *id,
            name: other.name.clone(),
            comparison: compare(me, other, &rarity),
        })
        .filter(|s| s.comparison.score >= MIN_SCORE)
        .collect();
    out.sort_by(|a, b| {
        b.comparison
            .score
            .total_cmp(&a.comparison.score)
            .then_with(|| a.name.cmp(&b.name))
    });
    out.truncate(limit);
    Some(out)
}

/// A pair of similar projects, for the graph.
#[derive(Debug, Clone, Serialize)]
pub struct Pair {
    pub a: Uuid,
    pub b: Uuid,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// Every pair of projects at least [`MIN_SCORE`] alike, keeping for each
/// project only its `per_project` closest, best first.
pub fn pairs(signatures: &HashMap<Uuid, Signature>, per_project: usize) -> Vec<Pair> {
    let rarity = Rarity::of(signatures.values());
    let mut ids: Vec<Uuid> = signatures.keys().copied().collect();
    ids.sort();
    let mut all = Vec::new();
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            let c = compare(&signatures[a], &signatures[b], &rarity);
            if c.score >= MIN_SCORE {
                all.push(Pair {
                    a: *a,
                    b: *b,
                    score: c.score,
                    reasons: c.reasons,
                });
            }
        }
    }
    all.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then_with(|| (x.a, x.b).cmp(&(y.a, y.b)))
    });
    let mut kept: HashMap<Uuid, usize> = HashMap::new();
    all.retain(|p| {
        let (ka, kb) = (
            kept.get(&p.a).copied().unwrap_or(0),
            kept.get(&p.b).copied().unwrap_or(0),
        );
        // A pair stays when it is among the closest of either project.
        if ka < per_project || kb < per_project {
            *kept.entry(p.a).or_default() += 1;
            *kept.entry(p.b).or_default() += 1;
            true
        } else {
            false
        }
    });
    all
}

// ------------------------------------------------------------ loading ----

/// What decides whether a cached signature is still current: the project
/// changed, or more has been read from its files since.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    updated_at: DateTime<Utc>,
    units: i64,
    segments: i64,
    embedded: i64,
}

type Cache = HashMap<Uuid, (Fingerprint, Signature)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// Signatures of the projects to compare (the most recently changed, at
/// most [`MAX_PROJECTS`]), from the cache where it is current.
pub async fn load(pool: &PgPool) -> Result<HashMap<Uuid, Signature>, ApiError> {
    let rows = sqlx::query(
        "SELECT p.id, p.name, p.updated_at, \
                (SELECT count(*) FROM project_items i \
                   JOIN atomic_unit_provenance pv ON pv.artifact_id = i.artifact_id \
                  WHERE i.project_id = p.id) AS units, \
                (SELECT count(*) FROM project_items i \
                   JOIN documents d ON d.artifact_id = i.artifact_id \
                   JOIN document_segments s ON s.document_id = d.id \
                  WHERE i.project_id = p.id) AS segments, \
                (SELECT count(*) FROM project_items i \
                   JOIN documents d ON d.artifact_id = i.artifact_id \
                   JOIN document_segments s ON s.document_id = d.id \
                  WHERE i.project_id = p.id AND s.embedding IS NOT NULL) AS embedded \
         FROM projects p ORDER BY p.updated_at DESC, p.id LIMIT $1",
    )
    .bind(MAX_PROJECTS)
    .fetch_all(pool)
    .await?;
    let current: Vec<(Uuid, String, Fingerprint)> = rows
        .iter()
        .map(|r| {
            (
                r.get("id"),
                r.get("name"),
                Fingerprint {
                    updated_at: r.get("updated_at"),
                    units: r.get("units"),
                    segments: r.get("segments"),
                    embedded: r.get("embedded"),
                },
            )
        })
        .collect();

    let stale: Vec<Uuid> = {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        current
            .iter()
            .filter(|(id, _, fp)| cache.get(id).map_or(true, |(old, _)| old != fp))
            .map(|(id, _, _)| *id)
            .collect()
    };
    let fresh = if stale.is_empty() {
        HashMap::new()
    } else {
        compute(pool, &stale).await?
    };

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let keep: HashSet<Uuid> = current.iter().map(|(id, _, _)| *id).collect();
    cache.retain(|id, _| keep.contains(id));
    let mut out = HashMap::with_capacity(current.len());
    for (id, name, fp) in current {
        if let Some(mut sig) = fresh.get(&id).cloned() {
            sig.name = name.clone();
            cache.insert(id, (fp, sig));
        }
        if let Some((_, sig)) = cache.get_mut(&id) {
            sig.name = name;
            out.insert(id, sig.clone());
        }
    }
    Ok(out)
}

/// Build the signatures of `ids` from the database.
async fn compute(pool: &PgPool, ids: &[Uuid]) -> Result<HashMap<Uuid, Signature>, ApiError> {
    let mut sigs: HashMap<Uuid, Signature> =
        ids.iter().map(|id| (*id, Signature::default())).collect();

    let files = sqlx::query(
        "SELECT project_id, artifact_id, lower(path) AS path FROM project_items \
         WHERE project_id = ANY($1) AND item_kind = 'file' \
           AND status IN ('ingested', 'deduplicated', 'stored')",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    for r in &files {
        let sig = sigs
            .get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for");
        if let Some(a) = r.get::<Option<Uuid>, _>("artifact_id") {
            sig.files.insert(a);
        }
        sig.paths.insert(r.get("path"));
    }

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
        let sig = sigs
            .get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for");
        sig.entities.insert(r.get("id"), r.get("name"));
    }

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
        let sig = sigs
            .get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for");
        // Dampened, so one word repeated in a long file doesn't drown the rest.
        sig.terms
            .insert(r.get("lexeme"), (1.0 + r.get::<f64, _>("n")).ln());
    }

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
        let sig = sigs
            .get_mut(&r.get::<Uuid, _>("project_id"))
            .expect("asked for");
        sig.embedding = r
            .get::<Option<pgvector::Vector>, _>("centroid")
            .map(|v| v.to_vec());
    }
    Ok(sigs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(files: &[u128], paths: &[&str], entities: &[(u128, &str)], terms: &[&str]) -> Signature {
        Signature {
            name: String::new(),
            files: files.iter().map(|f| Uuid::from_u128(*f)).collect(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            entities: entities
                .iter()
                .map(|(id, n)| (Uuid::from_u128(*id), n.to_string()))
                .collect(),
            terms: terms.iter().map(|t| (t.to_string(), 1.0)).collect(),
            embedding: None,
        }
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
        let c = compare(&v1, &v2, &r);
        assert_eq!(c.signals.files, Some(0.0));
        assert_eq!(c.signals.layout, Some(1.0));
        assert!(c.score >= 0.3, "{c:?}");
        assert!(c.reasons.iter().any(|r| r.contains("at the same place")));
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
        a.embedding = Some(vec![1.0, 0.0]);
        b.embedding = Some(vec![1.0, 0.1]);
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
        let mut all = HashMap::new();
        let base = sig(&[1, 2, 3], &["x.md", "y.md"], &[(9, "Atlas")], &["atlas"]);
        all.insert(Uuid::from_u128(100), base.clone());
        all.insert(Uuid::from_u128(101), base.clone());
        let mut half = base.clone();
        half.files = [Uuid::from_u128(1), Uuid::from_u128(50)].into();
        all.insert(Uuid::from_u128(102), half);
        all.insert(
            Uuid::from_u128(103),
            sig(&[70], &["z.md"], &[(8, "Lee")], &["garden"]),
        );
        let ranked = rank(Uuid::from_u128(100), &all, 10).unwrap();
        let order: Vec<u128> = ranked.iter().map(|s| s.project_id.as_u128()).collect();
        assert_eq!(order, vec![101, 102], "the unrelated one is left out");
        assert!(rank(Uuid::from_u128(999), &all, 10).is_none());

        let p = pairs(&all, 1);
        assert!(p.iter().all(|x| x.score >= MIN_SCORE));
        assert_eq!((p[0].a.as_u128(), p[0].b.as_u128()), (100, 101));
        assert!(!p
            .iter()
            .any(|x| x.a.as_u128() == 103 || x.b.as_u128() == 103));
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
