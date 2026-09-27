//! Temporal semantics: when a claim was said, observed, and true.
//!
//! Four times are kept apart: `asserted_at` (when the source made the
//! statement), `observed_at` (when the event it reports happened),
//! `valid_from`/`valid_to` (the interval the claim says it holds) and
//! `ingested_at` (when Gather processed the source). Two claims can only
//! contradict if the periods they describe overlap; a later statement of
//! current state *succeeds* an earlier one rather than contradicting it; and
//! when the periods can't be aligned the answer is "unknown", never a
//! confident conflict.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// Everything known about a claim's time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeScope {
    pub asserted_at: Option<DateTime<Utc>>,
    pub observed_at: Option<DateTime<Utc>>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub ingested_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tense {
    Past,
    Present,
    Future,
    Unknown,
}

/// The period a claim describes, as far as it can be read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Interval {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    /// How the interval was derived.
    pub basis: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "relation")]
pub enum TimeRelation {
    /// Both periods include a common moment: the claims can conflict.
    Overlapping,
    /// The periods are known and do not overlap.
    Disjoint,
    /// Both describe current state, said far enough apart that the later one
    /// is a new state: `newer` is 0 or 1 (the index of the later claim).
    Succession { newer: usize },
    /// Not enough to tell.
    Unknown,
}

/// Knobs for the time interpretation.
#[derive(Debug, Clone, Copy)]
pub struct TemporalPolicy {
    /// Present-tense claims asserted within this window describe the same moment.
    pub same_moment: Duration,
    /// Present-tense claims asserted at least this far apart are read as a
    /// change of state. Between the two windows the relation is unknown.
    pub min_succession: Duration,
}

impl Default for TemporalPolicy {
    fn default() -> Self {
        Self {
            same_moment: Duration::hours(24),
            min_succession: Duration::days(30),
        }
    }
}

fn year_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // A preposition is required so quantities ("takes 2000 seconds")
        // are never read as years.
        Regex::new(r"(?i)\b(?:in|during|throughout|since|as of)\s+((?:19|20)\d{2})\b").unwrap()
    })
}

/// Tense of a statement from its verbs and time words.
pub fn tense(statement: &str) -> Tense {
    let s = format!(" {} ", statement.to_lowercase());
    let has = |w: &[&str]| w.iter().any(|x| s.contains(x));
    if has(&[
        " will ",
        " going to ",
        " plan to ",
        " planning to ",
        " next ",
    ]) {
        return Tense::Future;
    }
    if has(&[
        " now ",
        " currently ",
        " today ",
        " these days ",
        " at present ",
    ]) {
        return Tense::Present;
    }
    if has(&[
        " was ",
        " were ",
        " used to ",
        " had ",
        " moved ",
        " lived ",
        " worked ",
        " paused ",
        " stopped ",
        " previously ",
        " formerly ",
        " last year ",
        " ago ",
    ]) {
        return Tense::Past;
    }
    if has(&[
        " is ",
        " are ",
        " am ",
        " live ",
        " lives ",
        " work ",
        " works ",
        " use ",
        " uses ",
        " have ",
        " has ",
        " prefer ",
        " no longer ",
    ]) {
        return Tense::Present;
    }
    Tense::Unknown
}

/// A year named in the statement, as a calendar-year interval.
pub fn explicit_year(statement: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let caps = year_re().captures(statement)?;
    let year: i32 = caps.get(1)?.as_str().parse().ok()?;
    let start = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).single()?;
    let end = Utc.with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0).single()?;
    Some((start, end))
}

/// Read the period a claim describes.
pub fn interval(statement: &str, t: &TimeScope) -> Interval {
    if t.valid_to.is_some() {
        return Interval {
            start: t.valid_from,
            end: t.valid_to,
            basis: "explicit_validity",
        };
    }
    if let Some((start, end)) = explicit_year(statement) {
        // "Rent is $1,500 in 2026" still ends when the year does only if the
        // claim is about that year; a present claim "since 2024" is open.
        let open = statement.to_lowercase().contains("since");
        return Interval {
            start: Some(start),
            end: if open { None } else { Some(end) },
            basis: "named_year",
        };
    }
    if let Some(observed) = t.observed_at {
        return Interval {
            start: Some(observed),
            end: None,
            basis: "observed_at",
        };
    }
    match tense(statement) {
        Tense::Past => Interval {
            start: None,
            end: t.asserted_at,
            basis: "past_tense_before_assertion",
        },
        Tense::Present => Interval {
            start: t.asserted_at,
            end: None,
            basis: "present_tense_at_assertion",
        },
        Tense::Future | Tense::Unknown => Interval {
            start: None,
            end: None,
            basis: "unknown",
        },
    }
}

/// How two claims relate in time.
pub fn relate(
    a: (&str, &TimeScope),
    b: (&str, &TimeScope),
    policy: &TemporalPolicy,
) -> (TimeRelation, Value) {
    let ia = interval(a.0, a.1);
    let ib = interval(b.0, b.1);
    let detail = |rel: &TimeRelation| {
        json!({
            "relation": rel,
            "a": {"interval": ia, "tense": tense(a.0), "asserted_at": a.1.asserted_at},
            "b": {"interval": ib, "tense": tense(b.0), "asserted_at": b.1.asserted_at},
        })
    };
    let rel = relate_intervals(&ia, &ib, policy);
    (rel, detail(&rel))
}

fn relate_intervals(a: &Interval, b: &Interval, policy: &TemporalPolicy) -> TimeRelation {
    let present = |i: &Interval| i.basis == "present_tense_at_assertion";
    // Two statements of current state.
    if present(a) && present(b) {
        let (Some(sa), Some(sb)) = (a.start, b.start) else {
            return TimeRelation::Unknown;
        };
        let gap = (sa - sb).abs();
        if gap <= policy.same_moment {
            return TimeRelation::Overlapping;
        }
        if gap >= policy.min_succession {
            return TimeRelation::Succession {
                newer: usize::from(sb > sa),
            };
        }
        return TimeRelation::Unknown;
    }
    // Known ends and starts that separate the periods.
    if let (Some(a_end), Some(b_start)) = (a.end, b.start) {
        if a_end <= b_start {
            return TimeRelation::Disjoint;
        }
    }
    if let (Some(b_end), Some(a_start)) = (b.end, a.start) {
        if b_end <= a_start {
            return TimeRelation::Disjoint;
        }
    }
    // Both fully bounded and not separated: they overlap.
    if a.start.is_some() && a.end.is_some() && b.start.is_some() && b.end.is_some() {
        return TimeRelation::Overlapping;
    }
    // A bounded period and an open one starting inside it.
    let inside = |p: &Interval, q: &Interval| match (p.start, p.end, q.start) {
        (Some(s), Some(e), Some(qs)) => qs >= s && qs < e,
        _ => false,
    };
    if inside(a, b) || inside(b, a) {
        return TimeRelation::Overlapping;
    }
    TimeRelation::Unknown
}

/// Which claim describes the later state, when they are sequenced (disjoint
/// or in succession). `None` when neither is clearly later.
pub fn later_of(a: (&str, &TimeScope), b: (&str, &TimeScope), rel: &TimeRelation) -> Option<usize> {
    match rel {
        TimeRelation::Succession { newer } => Some(*newer),
        TimeRelation::Disjoint => {
            let ia = interval(a.0, a.1);
            let ib = interval(b.0, b.1);
            match (ia.end, ib.start, ib.end, ia.start) {
                (Some(ae), Some(bs), _, _) if ae <= bs => Some(1),
                (_, _, Some(be), Some(as_)) if be <= as_ => Some(0),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Start of the later claim's period: where the earlier one stops holding.
pub fn succession_boundary(stmt: &str, t: &TimeScope) -> Option<DateTime<Utc>> {
    interval(stmt, t).start.or(t.asserted_at)
}

/// Keep the year component handy for fixtures and explanations.
pub fn year_of(t: DateTime<Utc>) -> i32 {
    t.year()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32) -> Option<DateTime<Utc>> {
        Utc.with_ymd_and_hms(y, m, 1, 12, 0, 0).single()
    }

    fn said(y: i32, m: u32) -> TimeScope {
        TimeScope {
            asserted_at: at(y, m),
            ..TimeScope::default()
        }
    }

    #[test]
    fn named_years_that_do_not_overlap_are_disjoint() {
        let (rel, _) = relate(
            ("I live in Chicago during 2023", &said(2023, 6)),
            ("I live in St. Louis during 2026", &said(2026, 6)),
            &TemporalPolicy::default(),
        );
        assert_eq!(rel, TimeRelation::Disjoint);
    }

    #[test]
    fn past_rent_and_current_rent_are_sequenced() {
        let a = ("Rent was $1,200 in 2024", said(2026, 3));
        let b = ("Rent is $1,500 now", said(2026, 3));
        let (rel, _) = relate((a.0, &a.1), (b.0, &b.1), &TemporalPolicy::default());
        assert_eq!(rel, TimeRelation::Disjoint);
        assert_eq!(later_of((a.0, &a.1), (b.0, &b.1), &rel), Some(1));
    }

    #[test]
    fn paused_then_active_is_a_change_not_a_conflict() {
        let a = ("The project was paused", said(2025, 1));
        let b = ("The project is active", said(2025, 6));
        let (rel, _) = relate((a.0, &a.1), (b.0, &b.1), &TemporalPolicy::default());
        assert_eq!(rel, TimeRelation::Disjoint);
    }

    #[test]
    fn present_claims_far_apart_succeed_close_ones_overlap() {
        let p = TemporalPolicy::default();
        let (far, _) = relate(
            ("I live in Chicago", &said(2023, 1)),
            ("I live in St. Louis", &said(2026, 1)),
            &p,
        );
        assert_eq!(far, TimeRelation::Succession { newer: 1 });
        let same = said(2026, 1);
        let (close, _) = relate(
            ("I live in Chicago", &same),
            ("I live in St. Louis", &same),
            &p,
        );
        assert_eq!(close, TimeRelation::Overlapping);
        let two_weeks_later = TimeScope {
            asserted_at: said(2026, 1).asserted_at.map(|t| t + Duration::days(14)),
            ..TimeScope::default()
        };
        let (mid, _) = relate(
            ("I live in Chicago", &said(2026, 1)),
            ("I live in St. Louis", &two_weeks_later),
            &p,
        );
        assert_eq!(mid, TimeRelation::Unknown);
    }

    #[test]
    fn missing_times_are_unknown() {
        let none = TimeScope::default();
        let (rel, _) = relate(
            ("I live in Chicago", &none),
            ("I live in Tokyo", &none),
            &TemporalPolicy::default(),
        );
        assert_eq!(rel, TimeRelation::Unknown);
    }
}
