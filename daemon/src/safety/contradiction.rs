//! Contradiction safety: a conflict is reported only when the two claims are
//! aligned on every dimension that matters.
//!
//! The structural scorer (`scan::score`) says *why* two claims might clash.
//! This rule then checks the seven alignment dimensions — subject,
//! predicate, unit, value, scope, granularity and time — and decides:
//! aligned → a contradiction; anything unresolved → a contradiction flagged
//! for review; a known reason they are compatible (different periods, a city
//! inside a state, different scopes, different modality, the user already
//! said "not a conflict") → no contradiction, with a blocked certificate
//! saying why. A later statement of current state *supersedes* the earlier
//! one instead of contradicting it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::certificate::{
    ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, InferenceDecision,
    Predicate, RuleId,
};
use super::modality::classify;
use super::reason::ReasonCode;
use super::temporal::{
    interval, later_of, relate, succession_boundary, TemporalPolicy, TimeRelation, TimeScope,
};
use crate::scan::score::{normalize_quantity, zone_offset_minutes, Conflict, UnitFacts};

pub const RULE_CONTRADICTION: RuleId = RuleId {
    id: "contradiction.aligned_conflict",
    version: 1,
};
pub const RULE_SUPERSEDE: RuleId = RuleId {
    id: "fact.supersede_by_succession",
    version: 1,
};

/// The seven dimensions every reported contradiction is aligned on.
pub const DIMENSIONS: [&str; 7] = [
    "subject",
    "predicate",
    "unit",
    "value",
    "scope",
    "granularity",
    "time",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DimStatus {
    /// Both claims agree on this dimension.
    Aligned,
    /// The dimension is where the claims clash (the conflicting value).
    Differs,
    /// The claims are about different things on this dimension.
    Mismatch,
    /// Not enough information.
    Unknown,
    /// The dimension doesn't apply to this kind of claim.
    NotApplicable,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dim {
    pub status: DimStatus,
    pub detail: Value,
}

pub type Alignment = BTreeMap<&'static str, Dim>;

/// Does an alignment record carry every required dimension?
pub fn alignment_complete(v: &Value) -> bool {
    DIMENSIONS
        .iter()
        .all(|d| v.get(d).and_then(|x| x.get("status")).is_some())
}

/// Facts the rule needs beyond the two claims.
#[derive(Debug, Clone, Default)]
pub struct ContradictionContext {
    /// child → parents over `located_in` / `part_of` edges.
    pub part_of: BTreeMap<Uuid, Vec<Uuid>>,
    /// A person already resolved this pair as "both valid" or "not a conflict".
    pub user_rejected: bool,
    /// A person agreed with the reason this pair was explained away. It is
    /// never reported, but a supersession that reason implies still applies.
    pub user_agreed_compatible: bool,
    pub policy: TemporalPolicy,
    /// Artifacts behind each claim (for the certificate).
    pub sources_a: Vec<Uuid>,
    pub sources_b: Vec<Uuid>,
    pub model_version: Option<String>,
}

/// A claim: the scorer's facts plus its time.
pub struct Claim<'a> {
    pub facts: &'a UnitFacts,
    pub time: &'a TimeScope,
}

#[derive(Debug, Clone)]
pub struct Supersession {
    pub older: Uuid,
    pub newer: Uuid,
    pub boundary: Option<DateTime<Utc>>,
    pub certificate: InferenceCertificate,
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub decision: InferenceDecision,
    pub alignment: Alignment,
    pub supersession: Option<Supersession>,
}

fn dim(status: DimStatus, detail: Value) -> Dim {
    Dim { status, detail }
}

fn attr<'a>(u: &'a UnitFacts, key: &str) -> Option<&'a str> {
    u.attrs
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Is `x` inside `y` (or `y` inside `x`) through part-of edges?
pub fn contained(part_of: &BTreeMap<Uuid, Vec<Uuid>>, x: Uuid, y: Uuid) -> bool {
    let reach = |from: Uuid, to: Uuid| {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([from]);
        while let Some(n) = queue.pop_front() {
            if n == to {
                return true;
            }
            if seen.insert(n) && seen.len() < 256 {
                queue.extend(part_of.get(&n).into_iter().flatten().copied());
            }
        }
        false
    };
    reach(x, y) || reach(y, x)
}

fn subject_dim(a: &UnitFacts, b: &UnitFacts) -> Dim {
    match (a.subject_entity_id, b.subject_entity_id) {
        (Some(x), Some(y)) if x == y => dim(DimStatus::Aligned, json!({"entity": x})),
        (Some(x), Some(y)) => dim(DimStatus::Mismatch, json!({"a": x, "b": y})),
        _ => dim(DimStatus::Unknown, json!({})),
    }
}

fn keyed_dim(a: &UnitFacts, b: &UnitFacts, keys: &[&'static str]) -> Dim {
    fn pick<'u>(u: &'u UnitFacts, keys: &[&'static str]) -> Option<(&'static str, &'u str)> {
        keys.iter().find_map(|k| attr(u, k).map(|v| (*k, v)))
    }
    match (pick(a, keys), pick(b, keys)) {
        (None, None) => dim(DimStatus::Aligned, json!({"a": null, "b": null})),
        (Some((_, x)), Some((_, y))) if x.eq_ignore_ascii_case(y) => {
            dim(DimStatus::Aligned, json!({"a": x, "b": y}))
        }
        (Some((_, x)), Some((_, y))) => dim(DimStatus::Mismatch, json!({"a": x, "b": y})),
        (x, y) => dim(
            DimStatus::Unknown,
            json!({"a": x.map(|v| v.1), "b": y.map(|v| v.1)}),
        ),
    }
}

/// Evaluate one structurally flagged pair.
pub fn evaluate(a: Claim, b: Claim, conflict: &Conflict, ctx: &ContradictionContext) -> Evaluation {
    let (ua, ub) = (a.facts, b.facts);
    let mut al: Alignment = BTreeMap::new();

    al.insert("subject", subject_dim(ua, ub));

    // Predicate: the rule's own relation, plus matching modality.
    let (ma, mb) = (classify(&ua.statement), classify(&ub.statement));
    let modal_same = ma.modality == mb.modality;
    let ambiguous = ma.ambiguous || mb.ambiguous;
    al.insert(
        "predicate",
        dim(
            if !modal_same {
                DimStatus::Mismatch
            } else if ambiguous {
                DimStatus::Unknown
            } else {
                DimStatus::Aligned
            },
            json!({
                "rule": conflict.method,
                "modality": [ma.modality.as_str(), mb.modality.as_str()],
                "negated": [ma.negated, mb.negated],
                "ambiguous": ambiguous,
            }),
        ),
    );

    // Unit and value.
    match conflict.method {
        "rule:numeric-mismatch" => {
            let q = |u: &UnitFacts| {
                normalize_quantity(
                    attr(u, "value").unwrap_or(""),
                    attr(u, "unit").unwrap_or(""),
                )
            };
            let (qa, qb) = (q(ua), q(ub));
            al.insert(
                "unit",
                match (&qa, &qb) {
                    (Some((_, x)), Some((_, y))) if x == y => {
                        dim(DimStatus::Aligned, json!({"unit": x}))
                    }
                    _ => dim(DimStatus::Unknown, json!({"a": qa, "b": qb})),
                },
            );
            al.insert(
                "value",
                dim(
                    DimStatus::Differs,
                    json!({"a": qa.map(|x| x.0), "b": qb.map(|x| x.0)}),
                ),
            );
        }
        "rule:time-mismatch" => {
            fn tz(u: &UnitFacts) -> Option<(&str, Option<i32>)> {
                attr(u, "tz").map(|z| (z, zone_offset_minutes(z)))
            }
            let (za, zb) = (tz(ua), tz(ub));
            let known = matches!((&za, &zb), (Some((_, Some(_))), Some((_, Some(_)))));
            al.insert(
                "unit",
                dim(
                    if known {
                        DimStatus::Aligned
                    } else {
                        DimStatus::Unknown
                    },
                    json!({"tz": [za.map(|z| z.0), zb.map(|z| z.0)]}),
                ),
            );
            al.insert(
                "value",
                dim(
                    DimStatus::Differs,
                    json!({"a": attr(ua, "value"), "b": attr(ub, "value")}),
                ),
            );
        }
        "rule:exclusive-assignment" => {
            al.insert("unit", dim(DimStatus::NotApplicable, Value::Null));
            al.insert(
                "value",
                dim(
                    DimStatus::Differs,
                    json!({"a": ua.assignments.iter().map(|x| x.2).collect::<Vec<_>>(),
                           "b": ub.assignments.iter().map(|x| x.2).collect::<Vec<_>>()}),
                ),
            );
        }
        _ => {
            al.insert("unit", dim(DimStatus::NotApplicable, Value::Null));
            al.insert(
                "value",
                dim(
                    DimStatus::Differs,
                    json!({"polarity": [ma.negated, mb.negated]}),
                ),
            );
        }
    }

    al.insert("scope", keyed_dim(ua, ub, &["scope", "metric"]));

    // Granularity: an explicit level, or one target inside the other.
    let mut gran = keyed_dim(ua, ub, &["granularity"]);
    if conflict.method == "rule:exclusive-assignment" {
        let nested = ua.assignments.iter().any(|(sa, ra, ta)| {
            ub.assignments.iter().any(|(sb, rb, tb)| {
                sa == sb && ra == rb && ta != tb && contained(&ctx.part_of, *ta, *tb)
            })
        });
        if nested {
            gran = dim(DimStatus::Mismatch, json!({"nested": true}));
        }
    }
    al.insert("granularity", gran);

    let (rel, time_detail) = relate(
        (&ua.statement, a.time),
        (&ub.statement, b.time),
        &ctx.policy,
    );
    al.insert(
        "time",
        dim(
            match rel {
                TimeRelation::Overlapping => DimStatus::Aligned,
                TimeRelation::Disjoint | TimeRelation::Succession { .. } => DimStatus::Mismatch,
                TimeRelation::Unknown => DimStatus::Unknown,
            },
            time_detail.clone(),
        ),
    );

    // Predicates, fail-closed.
    let mut preds: Vec<Predicate> = Vec::new();
    let check = |name: &str, ok: bool, code: ReasonCode, detail: Value| {
        if ok {
            Predicate::pass(name, detail)
        } else {
            Predicate::fail(name, code, detail)
        }
    };
    preds.push(check(
        "no_user_rejection",
        !ctx.user_rejected && !ctx.user_agreed_compatible,
        ReasonCode::UserRejectionExists,
        Value::Null,
    ));
    let s = &al["subject"];
    preds.push(match s.status {
        DimStatus::Aligned => Predicate::pass("same_subject", s.detail.clone()),
        DimStatus::Mismatch => Predicate::fail(
            "same_subject",
            ReasonCode::ContextScopeMismatch,
            s.detail.clone(),
        ),
        _ => Predicate::fail(
            "same_subject",
            ReasonCode::ContextScopeUnknown,
            s.detail.clone(),
        ),
    });
    preds.push(check(
        "same_modality",
        modal_same,
        ReasonCode::ModalityMismatch,
        al["predicate"].detail.clone(),
    ));
    preds.push(check(
        "polarity_readable",
        !ambiguous,
        ReasonCode::NegationAmbiguity,
        Value::Null,
    ));
    preds.push(check(
        "units_normalized",
        matches!(
            al["unit"].status,
            DimStatus::Aligned | DimStatus::NotApplicable
        ),
        ReasonCode::UnitNormalizationRequired,
        al["unit"].detail.clone(),
    ));
    let sc = &al["scope"];
    preds.push(match sc.status {
        DimStatus::Aligned => Predicate::pass("same_scope", sc.detail.clone()),
        DimStatus::Mismatch => Predicate::fail(
            "same_scope",
            ReasonCode::ContextScopeMismatch,
            sc.detail.clone(),
        ),
        _ => Predicate::fail(
            "same_scope",
            ReasonCode::ContextScopeUnknown,
            sc.detail.clone(),
        ),
    });
    let g = &al["granularity"];
    preds.push(match g.status {
        DimStatus::Aligned => Predicate::pass("same_granularity", g.detail.clone()),
        DimStatus::Unknown => Predicate::fail(
            "same_granularity",
            ReasonCode::ContextScopeUnknown,
            g.detail.clone(),
        ),
        _ => Predicate::fail(
            "same_granularity",
            ReasonCode::GranularityMismatch,
            g.detail.clone(),
        ),
    });
    preds.push(match rel {
        TimeRelation::Overlapping => Predicate::pass("same_time", time_detail.clone()),
        TimeRelation::Disjoint => Predicate::fail(
            "same_time",
            ReasonCode::TimeWindowsNonOverlapping,
            time_detail.clone(),
        ),
        TimeRelation::Succession { .. } => Predicate::fail(
            "same_time",
            ReasonCode::TemporalSuccession,
            time_detail.clone(),
        ),
        TimeRelation::Unknown => Predicate::fail(
            "same_time",
            ReasonCode::TimeScopeUnknown,
            time_detail.clone(),
        ),
    });

    // Known reasons the claims are compatible block; missing information
    // routes to review; full alignment reports the contradiction.
    let blocking = [
        ReasonCode::UserRejectionExists,
        ReasonCode::ContextScopeMismatch,
        ReasonCode::ModalityMismatch,
        ReasonCode::GranularityMismatch,
        ReasonCode::TimeWindowsNonOverlapping,
        ReasonCode::TemporalSuccession,
    ];
    let decision = super::certificate::decide_from_predicates(&preds, &blocking);

    let (lo, hi) = if ua.id < ub.id { (ua, ub) } else { (ub, ua) };
    let alignment_json = serde_json::to_value(&al).unwrap_or(Value::Null);
    let mut cert = InferenceCertificate::new(
        ConclusionKind::Contradiction,
        RULE_CONTRADICTION,
        format!("contradiction:{}:{}", lo.id, hi.id),
    )
    .with_subjects([ua.id, ub.id])
    .with_sources(ctx.sources_a.iter().chain(&ctx.sources_b).copied())
    .input(unit_input(ua, conflict))
    .input(unit_input(ub, conflict));
    cert.predicates = preds;
    cert.scope = json!({"alignment": alignment_json});
    cert.temporal = time_detail;
    cert.model_version = ctx.model_version.clone();
    cert.config = json!({
        "rule": conflict.method,
        "structural_score": (f64::from(conflict.score) * 10_000.0).round() / 10_000.0,
        "same_moment_hours": ctx.policy.same_moment.num_hours(),
        "min_succession_days": ctx.policy.min_succession.num_days(),
    });
    let cert = match decision {
        Decision::AutoApplied => cert
            .decide(decision)
            .explain(format!("Contradiction: {}", conflict.explanation)),
        Decision::NeedsReview => {
            let c = cert.decide(decision);
            let why = c.explanation.clone();
            c.explain(format!(
                "Possible contradiction, needs your judgment: {why}"
            ))
        }
        _ => {
            let c = cert.decide(decision);
            let why = c.explanation.clone();
            c.explain(format!("Not marked contradictory: {why}"))
        }
    };

    // A later statement of current state supersedes the earlier one.
    let supersession = later_of((&ua.statement, a.time), (&ub.statement, b.time), &rel)
        .filter(|_| !ctx.user_rejected && modal_same && !ambiguous)
        .filter(|_| al["subject"].status == DimStatus::Aligned)
        // Only the same state, measured the same way, can replace itself.
        .filter(|_| {
            al["scope"].status == DimStatus::Aligned
                && al["granularity"].status == DimStatus::Aligned
                && matches!(
                    al["unit"].status,
                    DimStatus::Aligned | DimStatus::NotApplicable
                )
        })
        .filter(|_| {
            matches!(
                conflict.method,
                "rule:numeric-mismatch"
                    | "rule:exclusive-assignment"
                    | "rule:negation"
                    | "rule:antonym"
            )
        })
        .and_then(|newer_idx| {
            let (older, newer, newer_claim) = if newer_idx == 1 {
                (ua, ub, &b)
            } else {
                (ub, ua, &a)
            };
            // Only a claim that still holds (an open period) supersedes.
            if interval(&newer.statement, newer_claim.time).end.is_some() {
                return None;
            }
            let boundary = succession_boundary(&newer.statement, newer_claim.time);
            let mut c = InferenceCertificate::new(
                ConclusionKind::FactSupersession,
                RULE_SUPERSEDE,
                format!("supersede:{}:{}", older.id, newer.id),
            )
            .with_subjects([older.id, newer.id])
            .with_sources(ctx.sources_a.iter().chain(&ctx.sources_b).copied())
            .input(unit_input(older, conflict))
            .input(unit_input(newer, conflict))
            .predicate(Predicate::pass(
                "same_subject",
                al["subject"].detail.clone(),
            ))
            .predicate(Predicate::pass(
                "same_state",
                json!({"rule": conflict.method}),
            ))
            .predicate(Predicate::pass(
                "sequenced_in_time",
                al["time"].detail.clone(),
            ))
            .predicate(Predicate::pass(
                "newer_is_current",
                json!({"from": boundary}),
            ));
            c.temporal = al["time"].detail.clone();
            c.model_version = ctx.model_version.clone();
            Some(Supersession {
                older: older.id,
                newer: newer.id,
                boundary,
                certificate: c.decide(Decision::AutoApplied).explain(
                    "The newer statement describes the current state; the older one is kept \
                     as history.",
                ),
            })
        });

    Evaluation {
        decision: InferenceDecision::from_certificate(cert),
        alignment: al,
        supersession,
    }
}

fn unit_input(u: &UnitFacts, conflict: &Conflict) -> EvidenceRef {
    EvidenceRef {
        id: u.id,
        kind: "unit".into(),
        class: EvidenceClass::Extracted,
        detail: json!({"statement": u.statement, "rule": conflict.method}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::score::score_pair;
    use chrono::TimeZone;

    fn unit(n: u128, statement: &str, subject: Option<u128>, attrs: Value) -> UnitFacts {
        UnitFacts {
            id: Uuid::from_u128(n),
            statement: statement.into(),
            attrs,
            subject_entity_id: subject.map(Uuid::from_u128),
            valid_from: None,
            valid_to: None,
            assignments: vec![],
        }
    }

    fn said(y: i32) -> TimeScope {
        TimeScope {
            asserted_at: Utc.with_ymd_and_hms(y, 3, 1, 0, 0, 0).single(),
            ..TimeScope::default()
        }
    }

    fn run(a: &UnitFacts, ta: &TimeScope, b: &UnitFacts, tb: &TimeScope) -> Option<Evaluation> {
        let conflict = score_pair(a, b, None)?;
        Some(evaluate(
            Claim { facts: a, time: ta },
            Claim { facts: b, time: tb },
            &conflict,
            &ContradictionContext::default(),
        ))
    }

    #[test]
    fn aligned_numeric_conflict_is_reported_with_all_dimensions() {
        let a = unit(
            1,
            "My budget is $50 per month",
            Some(9),
            json!({"pattern":"numeric","value":"$50","unit":"per month"}),
        );
        let b = unit(
            2,
            "My budget is $75 per month",
            Some(9),
            json!({"pattern":"numeric","value":"$75","unit":"per month"}),
        );
        let t = said(2026);
        let e = run(&a, &t, &b, &t).unwrap();
        assert!(e.decision.is_auto());
        let v = serde_json::to_value(&e.alignment).unwrap();
        assert!(alignment_complete(&v));
    }

    #[test]
    fn equivalent_amounts_do_not_conflict() {
        let a = unit(
            1,
            "Revenue was $1.2M",
            Some(9),
            json!({"pattern":"numeric","value":"$1.2","unit":"M"}),
        );
        let b = unit(
            2,
            "Revenue was $1,200,000",
            Some(9),
            json!({"pattern":"numeric","value":"$1,200,000","unit":""}),
        );
        assert!(score_pair(&a, &b, None).is_none());
    }

    #[test]
    fn different_scopes_block() {
        let a = unit(
            1,
            "Model accuracy is 90% on validation",
            Some(9),
            json!({"pattern":"numeric","value":"90","unit":"%","scope":"validation"}),
        );
        let b = unit(
            2,
            "Model accuracy is 70% in production",
            Some(9),
            json!({"pattern":"numeric","value":"70","unit":"%","scope":"production"}),
        );
        let t = said(2026);
        let e = run(&a, &t, &b, &t).unwrap();
        assert!(matches!(e.decision, InferenceDecision::Blocked(_)));
        assert!(e
            .decision
            .certificate()
            .reason_codes()
            .contains(&ReasonCode::ContextScopeMismatch));
    }

    #[test]
    fn unknown_time_is_review_not_conflict() {
        let a = unit(
            1,
            "My budget is $50 per month",
            Some(9),
            json!({"pattern":"numeric","value":"$50","unit":"per month"}),
        );
        let b = unit(
            2,
            "My budget is $75 per month",
            Some(9),
            json!({"pattern":"numeric","value":"$75","unit":"per month"}),
        );
        let t = TimeScope::default();
        let e = run(&a, &t, &b, &t).unwrap();
        assert!(matches!(e.decision, InferenceDecision::NeedsReview(_)));
        assert!(e
            .decision
            .certificate()
            .reason_codes()
            .contains(&ReasonCode::TimeScopeUnknown));
    }

    #[test]
    fn a_later_current_state_supersedes() {
        let me = Uuid::from_u128(9);
        let mut a = unit(1, "I live in Chicago", Some(9), json!({}));
        a.assignments = vec![(me, "lives_in".into(), Uuid::from_u128(20))];
        let mut b = unit(2, "I live in St. Louis", Some(9), json!({}));
        b.assignments = vec![(me, "lives_in".into(), Uuid::from_u128(21))];
        let e = run(&a, &said(2023), &b, &said(2026)).unwrap();
        assert!(matches!(e.decision, InferenceDecision::Blocked(_)));
        let s = e.supersession.expect("supersession");
        assert_eq!((s.older, s.newer), (a.id, b.id));
    }

    #[test]
    fn a_different_scope_never_supersedes() {
        let a = unit(
            1,
            "Model accuracy is 90% on validation",
            Some(9),
            json!({"pattern":"numeric","value":"90","unit":"%","scope":"validation"}),
        );
        let b = unit(
            2,
            "Model accuracy is 70% in production",
            Some(9),
            json!({"pattern":"numeric","value":"70","unit":"%","scope":"production"}),
        );
        let e = run(&a, &said(2023), &b, &said(2026)).unwrap();
        assert!(matches!(e.decision, InferenceDecision::Blocked(_)));
        assert!(e.supersession.is_none());
    }

    #[test]
    fn a_city_inside_a_state_is_not_a_conflict() {
        let me = Uuid::from_u128(9);
        let (missouri, overland) = (Uuid::from_u128(30), Uuid::from_u128(31));
        let mut a = unit(1, "I work in Missouri", Some(9), json!({}));
        a.assignments = vec![(me, "works_at".into(), missouri)];
        let mut b = unit(2, "I work in Overland", Some(9), json!({}));
        b.assignments = vec![(me, "works_at".into(), overland)];
        let conflict = score_pair(&a, &b, None).unwrap();
        let t = said(2026);
        let ctx = ContradictionContext {
            part_of: BTreeMap::from([(overland, vec![missouri])]),
            ..ContradictionContext::default()
        };
        let e = evaluate(
            Claim {
                facts: &a,
                time: &t,
            },
            Claim {
                facts: &b,
                time: &t,
            },
            &conflict,
            &ctx,
        );
        assert!(e
            .decision
            .certificate()
            .reason_codes()
            .contains(&ReasonCode::GranularityMismatch));
        assert!(!e.decision.is_auto());
    }

    #[test]
    fn clock_times_in_two_zones_agree() {
        let a = unit(
            1,
            "The meeting is at 3 PM Central",
            Some(9),
            json!({"pattern":"clock_time","value":"3 PM","tz":"Central"}),
        );
        let b = unit(
            2,
            "The meeting is at 4 PM Eastern",
            Some(9),
            json!({"pattern":"clock_time","value":"4 PM","tz":"Eastern"}),
        );
        assert!(score_pair(&a, &b, None).is_none());
        let c = unit(
            3,
            "The meeting is at 5 PM Eastern",
            Some(9),
            json!({"pattern":"clock_time","value":"5 PM","tz":"Eastern"}),
        );
        assert!(score_pair(&a, &c, None).is_some());
        let d = unit(
            4,
            "The meeting is at 5 PM",
            Some(9),
            json!({"pattern":"clock_time","value":"5 PM"}),
        );
        let t = said(2026);
        let e = run(&a, &t, &d, &t).unwrap();
        assert!(e
            .decision
            .certificate()
            .reason_codes()
            .contains(&ReasonCode::UnitNormalizationRequired));
    }
}
