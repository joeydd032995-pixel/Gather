//! Conflict scoring (write-up §6.2): pure functions from a pair of unit
//! facts to an optional contradiction score. No I/O — the worker (mod.rs)
//! gathers the inputs, so every rule is unit-testable in isolation.

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// Everything the scorer needs to know about one atomic unit.
#[derive(Debug, Clone)]
pub struct UnitFacts {
    pub id: Uuid,
    pub statement: String,
    /// `atomic_units.attrs` — carries `pattern`, `value`, `unit` from rules.rs.
    pub attrs: Value,
    pub subject_entity_id: Option<Uuid>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    /// (source entity, relation, target entity) edges asserted by this unit.
    pub assignments: Vec<(Uuid, String, Uuid)>,
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub score: f32,
    pub method: &'static str,
    pub explanation: String,
}

/// Relations where one subject can hold at most one value at a time.
const FUNCTIONAL_RELATIONS: &[&str] = &["lives_in", "works_at", "is", "decided_on"];

const ANTONYM_PAIRS: &[(&str, &str)] = &[
    ("enabled", "disabled"),
    ("enable", "disable"),
    ("on", "off"),
    ("always", "never"),
    ("prefer", "avoid"),
    ("love", "hate"),
    ("start", "stop"),
    ("increase", "decrease"),
    ("public", "private"),
    ("allow", "forbid"),
    ("paused", "active"),
    ("inactive", "active"),
];

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "i", "we", "my", "our", "is", "are", "was", "were", "be", "been", "am",
    "for", "of", "to", "in", "on", "at", "and", "or", "it", "this", "that", "per", "with",
];

const NEGATORS: &[&str] = &["not", "never", "no", "stopped", "quit", "dropped"];

/// Score a pair. `cosine_sim` is the pgvector cosine similarity when both
/// embeddings exist; otherwise a token-Jaccard fallback is used. Returns
/// None when no structural conflict signal fires (per §6.2, similarity alone
/// is never enough to call something a contradiction).
pub fn score_pair(a: &UnitFacts, b: &UnitFacts, cosine_sim: Option<f32>) -> Option<Conflict> {
    let (base, method, explanation) = structural_signal(a, b)?;

    let sim = cosine_sim.unwrap_or_else(|| {
        if method == "rule:numeric-mismatch" {
            // The differing values are the conflict itself, so they must not
            // also count against the statements' similarity: "Our rent is
            // $1200 per month" vs "$1500 per month" is the same statement.
            let words = |u: &UnitFacts| -> Vec<String> {
                content_tokens(u)
                    .into_iter()
                    .filter(|t| !t.chars().any(|c| c.is_ascii_digit()))
                    .collect()
            };
            jaccard(&words(a), &words(b))
        } else {
            jaccard(&content_tokens(a), &content_tokens(b))
        }
    });
    let temporal = if windows_disjoint(a, b) { 0.5 } else { 1.0 };
    let score = (base * (0.6 + 0.4 * sim) * temporal).clamp(0.0, 1.0);

    Some(Conflict {
        score,
        method,
        explanation,
    })
}

fn structural_signal(a: &UnitFacts, b: &UnitFacts) -> Option<(f32, &'static str, String)> {
    if let Some(explanation) = numeric_mismatch(a, b) {
        return Some((0.80, "rule:numeric-mismatch", explanation));
    }
    if let Some(explanation) = clock_mismatch(a, b) {
        return Some((0.80, "rule:time-mismatch", explanation));
    }
    if let Some(explanation) = negation_mismatch(a, b) {
        return Some((0.75, "rule:negation", explanation));
    }
    if let Some(explanation) = exclusive_assignment(a, b) {
        return Some((0.70, "rule:exclusive-assignment", explanation));
    }
    if let Some(explanation) = antonym_predicate(a, b) {
        return Some((0.60, "rule:antonym", explanation));
    }
    None
}

// ---------------------------------------------------------------------------
// Structural rules
// ---------------------------------------------------------------------------

/// Same subject entity + same unit of measure, values differing by >10%.
/// Values are normalized first ("$1.2M" and "$1,200,000" are the same
/// amount), so equivalent quantities never look like a conflict.
fn numeric_mismatch(a: &UnitFacts, b: &UnitFacts) -> Option<String> {
    if a.attrs.get("pattern")?.as_str()? != "numeric"
        || b.attrs.get("pattern")?.as_str()? != "numeric"
    {
        return None;
    }
    let subject = a.subject_entity_id?;
    if b.subject_entity_id? != subject {
        return None;
    }
    let (value_a, unit_a) = normalize_quantity(
        a.attrs.get("value")?.as_str()?,
        a.attrs.get("unit")?.as_str()?,
    )?;
    let (value_b, unit_b) = normalize_quantity(
        b.attrs.get("value")?.as_str()?,
        b.attrs.get("unit")?.as_str()?,
    )?;
    if unit_a != unit_b {
        return None;
    }
    let larger = value_a.abs().max(value_b.abs());
    if larger == 0.0 || ((value_a - value_b).abs() / larger) <= 0.10 {
        return None;
    }
    Some(format!(
        "same subject and unit, conflicting values: {value_a} vs {value_b} {unit_a}"
    ))
}

/// Same subject, two clock times that name different moments once their
/// time zones are applied ("3 PM Central" and "4 PM Eastern" agree).
fn clock_mismatch(a: &UnitFacts, b: &UnitFacts) -> Option<String> {
    if a.attrs.get("pattern")?.as_str()? != "clock_time"
        || b.attrs.get("pattern")?.as_str()? != "clock_time"
    {
        return None;
    }
    if a.subject_entity_id? != b.subject_entity_id? {
        return None;
    }
    let ma = parse_clock(a.attrs.get("value")?.as_str()?)?;
    let mb = parse_clock(b.attrs.get("value")?.as_str()?)?;
    let tz = |u: &UnitFacts| {
        u.attrs
            .get("tz")
            .and_then(Value::as_str)
            .and_then(zone_offset_minutes)
    };
    match (tz(a), tz(b)) {
        (Some(oa), Some(ob)) => {
            let utc = |m: i32, o: i32| (m - o).rem_euclid(24 * 60);
            if utc(ma, oa) == utc(mb, ob) {
                None
            } else {
                Some("same subject, the times name different moments".to_string())
            }
        }
        _ if ma != mb => Some("same subject, different times (time zone unknown)".to_string()),
        _ => None,
    }
}

/// Minutes after midnight for "3 PM", "3:30pm", "15:00".
pub fn parse_clock(raw: &str) -> Option<i32> {
    let s = raw.trim().to_lowercase().replace('.', "");
    let (body, pm, am) = if let Some(b) = s.strip_suffix("pm") {
        (b.trim().to_string(), true, false)
    } else if let Some(b) = s.strip_suffix("am") {
        (b.trim().to_string(), false, true)
    } else {
        (s.clone(), false, false)
    };
    let mut parts = body.split(':');
    let h: i32 = parts.next()?.trim().parse().ok()?;
    let m: i32 = parts
        .next()
        .map(|m| m.trim().parse().ok())
        .unwrap_or(Some(0))?;
    if !(0..60).contains(&m) || h > 23 || ((pm || am) && !(1..=12).contains(&h)) {
        return None;
    }
    let h = match (pm, am) {
        (true, _) if h != 12 => h + 12,
        (_, true) if h == 12 => 0,
        _ => h,
    };
    Some(h * 60 + m)
}

/// Offsets for zone names whose relative offsets hold all year. US zones
/// shift together for daylight saving, so comparing two of them is exact;
/// comparing a US zone with UTC is not, so UTC is only paired with itself.
pub fn zone_offset_minutes(zone: &str) -> Option<i32> {
    let z = zone.trim().to_lowercase();
    let us = |name: &str, off: i32| -> Option<i32> {
        let abbrev = [
            format!("{}t", &name[..1]),
            format!("{}st", &name[..1]),
            format!("{}dt", &name[..1]),
        ];
        (z == name || z == format!("{name} time") || abbrev.contains(&z)).then_some(off)
    };
    us("eastern", -5 * 60)
        .or_else(|| us("central", -6 * 60))
        .or_else(|| us("mountain", -7 * 60))
        .or_else(|| us("pacific", -8 * 60))
}

/// One statement is negated and the other is not, over the same content.
fn negation_mismatch(a: &UnitFacts, b: &UnitFacts) -> Option<String> {
    let neg_a = is_negated(&a.statement);
    let neg_b = is_negated(&b.statement);
    if neg_a == neg_b {
        return None;
    }
    let overlap = jaccard(&content_tokens(a), &content_tokens(b));
    if overlap < 0.5 {
        return None;
    }
    Some(format!(
        "one statement negates the other over the same content (overlap {overlap:.2})"
    ))
}

/// Same (subject, functional relation) asserted with different objects.
fn exclusive_assignment(a: &UnitFacts, b: &UnitFacts) -> Option<String> {
    for (src_a, rel_a, tgt_a) in &a.assignments {
        if !FUNCTIONAL_RELATIONS.contains(&rel_a.as_str()) {
            continue;
        }
        for (src_b, rel_b, tgt_b) in &b.assignments {
            if src_a == src_b && rel_a == rel_b && tgt_a != tgt_b {
                return Some(format!(
                    "same subject holds functional relation '{rel_a}' to two different entities"
                ));
            }
        }
    }
    None
}

/// The two statements use opposing predicates over overlapping content.
fn antonym_predicate(a: &UnitFacts, b: &UnitFacts) -> Option<String> {
    let tokens_a = all_tokens(&a.statement);
    let tokens_b = all_tokens(&b.statement);
    for (x, y) in ANTONYM_PAIRS {
        let forward = tokens_a.contains(&x.to_string()) && tokens_b.contains(&y.to_string());
        let backward = tokens_a.contains(&y.to_string()) && tokens_b.contains(&x.to_string());
        if forward || backward {
            let overlap = jaccard(&content_tokens(a), &content_tokens(b));
            if overlap >= 0.3 {
                return Some(format!(
                    "opposing predicates '{x}'/'{y}' over similar content (overlap {overlap:.2})"
                ));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

pub(crate) fn all_tokens(statement: &str) -> Vec<String> {
    statement
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '$' && c != '.')
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Tokens minus stopwords and negators — the "content" both similarity and
/// the negation rule compare, so "I use X" and "I no longer use X" align.
fn content_tokens(unit: &UnitFacts) -> Vec<String> {
    all_tokens(&unit.statement)
        .into_iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()) && !NEGATORS.contains(&t.as_str()))
        .filter(|t| t != "longer" && t != "anymore")
        .collect()
}

fn is_negated(statement: &str) -> bool {
    let lower = statement.to_lowercase();
    if lower.contains("no longer") || lower.contains("anymore") || lower.contains("n't ") {
        return true;
    }
    all_tokens(&lower)
        .iter()
        .any(|t| NEGATORS.contains(&t.as_str()))
}

pub(crate) fn jaccard(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let set_a: std::collections::HashSet<&String> = a.iter().collect();
    let set_b: std::collections::HashSet<&String> = b.iter().collect();
    let intersection = set_a.intersection(&set_b).count() as f32;
    let union = set_a.union(&set_b).count() as f32;
    intersection / union
}

fn normalize_unit(unit: &str) -> String {
    let u = unit
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match u.as_str() {
        "dollars" | "usd" | "bucks" => "$".to_string(),
        other => other.to_string(),
    }
}

fn parse_number(raw: &str) -> Option<f64> {
    raw.trim_start_matches(['$', '€', '£'])
        .replace(',', "")
        .parse()
        .ok()
}

/// Magnitude words that may trail a value ("1.2 M", "3 million").
const MAGNITUDES: &[(&str, f64)] = &[
    ("k", 1e3),
    ("K", 1e3),
    ("thousand", 1e3),
    ("M", 1e6),
    ("mm", 1e6),
    ("million", 1e6),
    ("B", 1e9),
    ("bn", 1e9),
    ("billion", 1e9),
];

/// Normalize a (value, unit) pair to (amount, canonical unit): magnitude
/// suffixes are applied and a currency symbol on the value becomes the unit,
/// so "$1.2" + "M" and "$1,200,000" + "" are both (1200000, "$").
pub fn normalize_quantity(value: &str, unit: &str) -> Option<(f64, String)> {
    let currency = value
        .chars()
        .next()
        .filter(|c| ['$', '€', '£'].contains(c))
        .map(|c| c.to_string());
    let mut amount = parse_number(value)?;
    let mut rest: Vec<&str> = unit.split_whitespace().collect();
    if let Some(first) = rest.first() {
        if let Some((_, m)) = MAGNITUDES.iter().find(|(w, _)| w == first) {
            amount *= m;
            rest.remove(0);
        }
    }
    let mut canonical = normalize_unit(&rest.join(" "));
    if let Some(c) = currency {
        let c = normalize_unit(&c);
        canonical = if canonical.is_empty() || canonical == c {
            c
        } else {
            format!("{c} {canonical}")
        };
    }
    Some((amount, canonical))
}

/// Both windows known and non-overlapping: sequenced facts, likely both true.
fn windows_disjoint(a: &UnitFacts, b: &UnitFacts) -> bool {
    match (a.valid_from, a.valid_to, b.valid_from, b.valid_to) {
        (Some(_), Some(a_to), Some(b_from), _) if a_to <= b_from => true,
        (Some(a_from), _, Some(_), Some(b_to)) if b_to <= a_from => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn unit(statement: &str, attrs: Value) -> UnitFacts {
        UnitFacts {
            id: Uuid::new_v4(),
            statement: statement.to_string(),
            attrs,
            subject_entity_id: None,
            valid_from: None,
            valid_to: None,
            assignments: vec![],
        }
    }

    fn numeric_unit(statement: &str, value: &str, measure: &str, subject: Uuid) -> UnitFacts {
        let mut u = unit(
            statement,
            json!({"pattern": "numeric", "value": value, "unit": measure}),
        );
        u.subject_entity_id = Some(subject);
        u
    }

    #[test]
    fn numeric_mismatch_fires_on_conflicting_values() {
        let subject = Uuid::new_v4();
        let a = numeric_unit(
            "My VPS budget is $50 per month",
            "$50",
            "per month",
            subject,
        );
        let b = numeric_unit(
            "My VPS budget is $75 per month",
            "$75",
            "per month",
            subject,
        );
        let conflict = score_pair(&a, &b, None).expect("should conflict");
        assert_eq!(conflict.method, "rule:numeric-mismatch");
        assert!(conflict.score >= 0.65, "score was {}", conflict.score);
    }

    #[test]
    fn numeric_mismatch_is_not_diluted_by_the_differing_values() {
        // Short statements share few words besides the values that differ.
        let subject = Uuid::new_v4();
        let a = numeric_unit("Our rent is $1200 per month", "$1200", "per month", subject);
        let b = numeric_unit("Our rent is $1500 per month", "$1500", "per month", subject);
        let conflict = score_pair(&a, &b, None).expect("should conflict");
        assert!(conflict.score >= 0.65, "score was {}", conflict.score);
    }

    #[test]
    fn numeric_agreement_and_small_delta_do_not_fire() {
        let subject = Uuid::new_v4();
        let a = numeric_unit("The build takes 100 seconds", "100", "seconds", subject);
        let same = numeric_unit("The build takes 100 seconds now", "100", "seconds", subject);
        let close = numeric_unit("The build takes 105 seconds", "105", "seconds", subject);
        assert!(score_pair(&a, &same, None).is_none());
        assert!(
            score_pair(&a, &close, None).is_none(),
            "within 10% tolerance"
        );
    }

    #[test]
    fn numeric_requires_same_subject_and_unit() {
        let a = numeric_unit(
            "VPS budget is $50 per month",
            "$50",
            "per month",
            Uuid::new_v4(),
        );
        let b = numeric_unit(
            "Laptop budget is $900 per month",
            "$900",
            "per month",
            Uuid::new_v4(),
        );
        assert!(score_pair(&a, &b, None).is_none(), "different subjects");
        let s = Uuid::new_v4();
        let c = numeric_unit("Backup size is 20 GB", "20", "GB", s);
        let d = numeric_unit("Backup takes 40 seconds", "40", "seconds", s);
        assert!(
            score_pair(&c, &d, None).is_none(),
            "different units of measure"
        );
    }

    #[test]
    fn negation_mismatch_fires_and_survives_threshold() {
        let a = unit("I use PostgreSQL for storage", json!({}));
        let b = unit("I no longer use PostgreSQL for storage", json!({}));
        let conflict = score_pair(&a, &b, None).expect("should conflict");
        assert_eq!(conflict.method, "rule:negation");
        assert!(conflict.score >= 0.65, "score was {}", conflict.score);
    }

    #[test]
    fn negation_requires_content_overlap() {
        let a = unit("I use PostgreSQL for storage", json!({}));
        let b = unit("I never eat mushrooms on pizza", json!({}));
        assert!(score_pair(&a, &b, None).is_none());
    }

    #[test]
    fn exclusive_assignment_fires_for_functional_relations() {
        let me = Uuid::new_v4();
        let berlin = Uuid::new_v4();
        let tokyo = Uuid::new_v4();
        let mut a = unit("I live in Berlin", json!({}));
        a.assignments = vec![(me, "lives_in".to_string(), berlin)];
        let mut b = unit("I live in Tokyo", json!({}));
        b.assignments = vec![(me, "lives_in".to_string(), tokyo)];
        let conflict = score_pair(&a, &b, None).expect("should conflict");
        assert_eq!(conflict.method, "rule:exclusive-assignment");

        // Non-functional relations may fan out freely.
        let pg = Uuid::new_v4();
        let redis = Uuid::new_v4();
        let mut c = unit("I use PostgreSQL", json!({}));
        c.assignments = vec![(me, "uses".to_string(), pg)];
        let mut d = unit("I use Redis", json!({}));
        d.assignments = vec![(me, "uses".to_string(), redis)];
        assert!(score_pair(&c, &d, None).is_none());
    }

    #[test]
    fn antonym_predicate_fires_over_similar_content() {
        let a = unit("Telemetry is always enabled for the daemon", json!({}));
        let b = unit("Telemetry is disabled for the daemon", json!({}));
        let conflict = score_pair(&a, &b, None).expect("should conflict");
        assert_eq!(conflict.method, "rule:antonym");
    }

    #[test]
    fn temporal_disjoint_windows_dampen_score() {
        let subject = Uuid::new_v4();
        let mut a = numeric_unit(
            "My rent is 900 euros per month",
            "900",
            "per month",
            subject,
        );
        let mut b = numeric_unit(
            "My rent is 1200 euros per month",
            "1200",
            "per month",
            subject,
        );
        let baseline = score_pair(&a, &b, None).unwrap().score;

        a.valid_from = Some(chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap());
        a.valid_to = Some(chrono::Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
        b.valid_from = Some(chrono::Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap());
        let dampened = score_pair(&a, &b, None).unwrap().score;
        assert!((dampened - baseline * 0.5).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_overrides_jaccard_when_present() {
        let a = unit("I use PostgreSQL daily", json!({}));
        let b = unit("I do not use PostgreSQL daily", json!({}));
        let low = score_pair(&a, &b, Some(0.0)).unwrap().score;
        let high = score_pair(&a, &b, Some(1.0)).unwrap().score;
        assert!(high > low);
        assert!((high - 0.75).abs() < 0.001); // base 0.75 * (0.6 + 0.4*1.0)
    }

    #[test]
    fn similarity_alone_is_never_a_contradiction() {
        let a = unit("Gather stores data in PostgreSQL", json!({}));
        let b = unit("Gather keeps data in PostgreSQL", json!({}));
        assert!(score_pair(&a, &b, Some(0.99)).is_none());
    }
}
