//! Inference certificates: the durable record of one consequential conclusion.
//!
//! A certificate answers what Gather concluded, from which direct evidence,
//! under which named and versioned rule, with which scope/time assumptions and
//! configuration, and — when it did not act — exactly which predicate failed.
//! Rules build certificates as plain values (no I/O); `safety::store`
//! persists them.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::reason::ReasonCode;

/// What kind of conclusion a certificate is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConclusionKind {
    EntityMerge,
    PhotoDuplicateGroup,
    Contradiction,
    ClaimCanonicalization,
    FactSupersession,
    ExtractionRevision,
    /// A user decision recorded as evidence (split, not-duplicate, source removal).
    UserDecision,
}

impl ConclusionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConclusionKind::EntityMerge => "entity_merge",
            ConclusionKind::PhotoDuplicateGroup => "photo_duplicate_group",
            ConclusionKind::Contradiction => "contradiction",
            ConclusionKind::ClaimCanonicalization => "claim_canonicalization",
            ConclusionKind::FactSupersession => "fact_supersession",
            ConclusionKind::ExtractionRevision => "extraction_revision",
            ConclusionKind::UserDecision => "user_decision",
        }
    }
}

/// The decision a rule made. `UserDecision` marks a person's explicit choice,
/// which is evidence rather than an inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AutoApplied,
    NeedsReview,
    Blocked,
    UserDecision,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::AutoApplied => "auto_applied",
            Decision::NeedsReview => "needs_review",
            Decision::Blocked => "blocked",
            Decision::UserDecision => "user_decision",
        }
    }
}

/// Where a certificate stands now: its original decision, or withdrawn later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    AutoApplied,
    NeedsReview,
    Blocked,
    UserDecision,
    Superseded,
    Retracted,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::AutoApplied => "auto_applied",
            Outcome::NeedsReview => "needs_review",
            Outcome::Blocked => "blocked",
            Outcome::UserDecision => "user_decision",
            Outcome::Superseded => "superseded",
            Outcome::Retracted => "retracted",
        }
    }

    pub fn from_decision(d: Decision) -> Self {
        match d {
            Decision::AutoApplied => Outcome::AutoApplied,
            Decision::NeedsReview => Outcome::NeedsReview,
            Decision::Blocked => Outcome::Blocked,
            Decision::UserDecision => Outcome::UserDecision,
        }
    }
}

/// How a piece of evidence came to be known. Inferred evidence is never
/// presented as if a source had asserted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    /// Directly present in a source artifact (a quote, a photo's pixels).
    Asserted,
    /// Structured information extracted from one source (a unit, an entity name).
    Extracted,
    /// A conclusion produced from one or more inputs (a score, a merge).
    Inferred,
    /// An explicit user confirmation.
    UserConfirmed,
    /// An explicit user rejection ("different", "not a duplicate").
    Rejected,
    /// A plausible conclusion a safety predicate stopped.
    Blocked,
}

/// One direct input to a conclusion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub id: Uuid,
    /// 'entity' | 'unit' | 'image' | 'artifact' | 'pair' | 'certificate' | ...
    pub kind: String,
    pub class: EvidenceClass,
    /// Scores or other facts about this input (kept small).
    #[serde(default)]
    pub detail: Value,
}

/// A named safety predicate and whether it held.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate {
    pub name: String,
    pub passed: bool,
    /// The code reported when the predicate fails.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub code: Option<ReasonCode>,
    #[serde(default)]
    pub detail: Value,
}

impl Predicate {
    pub fn pass(name: &str, detail: Value) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            code: None,
            detail,
        }
    }
    pub fn fail(name: &str, code: ReasonCode, detail: Value) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            code: Some(code),
            detail,
        }
    }
}

/// A rule's identity: every automated conclusion names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleId {
    pub id: &'static str,
    pub version: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceCertificate {
    /// Assigned on persistence; `None` while the certificate is a plain value.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub kind: ConclusionKind,
    /// Deterministic identity of the conclusion (e.g. "entity-merge:<ids>").
    pub conclusion_key: String,
    /// The row that materializes the conclusion, once it exists.
    #[serde(default)]
    pub conclusion_id: Option<Uuid>,
    /// Entities / images / units the conclusion is about.
    pub subject_ids: Vec<Uuid>,
    pub rule_id: String,
    pub rule_version: i32,
    pub decision: Decision,
    pub evidence_class: EvidenceClass,
    pub inputs: Vec<EvidenceRef>,
    pub source_artifact_ids: Vec<Uuid>,
    pub source_family_ids: Vec<Uuid>,
    #[serde(default)]
    pub model_version: Option<String>,
    pub config: Value,
    pub scope: Value,
    pub temporal: Value,
    pub predicates: Vec<Predicate>,
    /// Plain-language summary for people.
    pub explanation: String,
}

impl InferenceCertificate {
    /// Start a certificate for `rule`. Callers add evidence and predicates,
    /// then [`InferenceCertificate::decide`].
    pub fn new(kind: ConclusionKind, rule: RuleId, conclusion_key: String) -> Self {
        Self {
            id: None,
            kind,
            conclusion_key,
            conclusion_id: None,
            subject_ids: Vec::new(),
            rule_id: rule.id.to_string(),
            rule_version: rule.version,
            decision: Decision::Blocked,
            evidence_class: EvidenceClass::Blocked,
            inputs: Vec::new(),
            source_artifact_ids: Vec::new(),
            source_family_ids: Vec::new(),
            model_version: None,
            config: Value::Null,
            scope: Value::Null,
            temporal: Value::Null,
            predicates: Vec::new(),
            explanation: String::new(),
        }
    }

    pub fn with_subjects(mut self, ids: impl IntoIterator<Item = Uuid>) -> Self {
        self.subject_ids = sorted_unique(ids);
        self
    }

    pub fn with_sources(mut self, ids: impl IntoIterator<Item = Uuid>) -> Self {
        self.source_artifact_ids = sorted_unique(ids);
        self
    }

    pub fn with_families(mut self, ids: impl IntoIterator<Item = Uuid>) -> Self {
        self.source_family_ids = sorted_unique(ids);
        self
    }

    pub fn input(mut self, e: EvidenceRef) -> Self {
        self.inputs.push(e);
        self
    }

    pub fn predicate(mut self, p: Predicate) -> Self {
        self.predicates.push(p);
        self
    }

    /// Codes of every failed predicate, sorted and unique.
    pub fn reason_codes(&self) -> Vec<ReasonCode> {
        let mut codes: Vec<ReasonCode> = self.predicates.iter().filter_map(|p| p.code).collect();
        codes.sort();
        codes.dedup();
        codes
    }

    /// Set the decision (and the matching evidence class) and a default
    /// explanation built from failed predicates when none was given.
    pub fn decide(mut self, decision: Decision) -> Self {
        self.decision = decision;
        self.evidence_class = match decision {
            Decision::AutoApplied | Decision::NeedsReview => EvidenceClass::Inferred,
            Decision::Blocked => EvidenceClass::Blocked,
            Decision::UserDecision => EvidenceClass::UserConfirmed,
        };
        if self.explanation.is_empty() {
            self.explanation = self
                .reason_codes()
                .iter()
                .map(|c| c.plain_language())
                .collect::<Vec<_>>()
                .join(" ");
        }
        self
    }

    pub fn explain(mut self, text: impl Into<String>) -> Self {
        self.explanation = text.into();
        self
    }

    /// Deterministic digest of what was decided and why, excluding ids and
    /// timestamps assigned at persistence. Two evaluations of the same
    /// evidence under the same rule produce the same digest, which is what
    /// makes re-evaluation idempotent and order-invariance checkable.
    pub fn evidence_digest(&self) -> String {
        let mut inputs: Vec<String> = self
            .inputs
            .iter()
            .map(|e| format!("{}:{}:{}", e.kind, e.id, canonical_json(&e.detail)))
            .collect();
        inputs.sort();
        let mut predicates: Vec<String> = self
            .predicates
            .iter()
            .map(|p| {
                format!(
                    "{}:{}:{}",
                    p.name,
                    p.passed,
                    p.code.map(|c| c.as_str()).unwrap_or("")
                )
            })
            .collect();
        predicates.sort();
        let material = serde_json::json!({
            "kind": self.kind.as_str(),
            "key": self.conclusion_key,
            "rule": self.rule_id,
            "version": self.rule_version,
            "decision": self.decision.as_str(),
            "subjects": self.subject_ids,
            "inputs": inputs,
            "sources": self.source_artifact_ids,
            "predicates": predicates,
            "scope": canonical_json(&self.scope),
            "temporal": canonical_json(&self.temporal),
        });
        hex::encode(Sha256::digest(material.to_string().as_bytes()))
    }
}

/// The result of evaluating a safety rule: the certificate always travels
/// with the decision, so no path can act without one.
#[derive(Debug, Clone, PartialEq)]
pub enum InferenceDecision {
    AutoApply(InferenceCertificate),
    NeedsReview(InferenceCertificate),
    Blocked(InferenceCertificate),
}

impl InferenceDecision {
    pub fn certificate(&self) -> &InferenceCertificate {
        match self {
            InferenceDecision::AutoApply(c)
            | InferenceDecision::NeedsReview(c)
            | InferenceDecision::Blocked(c) => c,
        }
    }
    pub fn into_certificate(self) -> InferenceCertificate {
        match self {
            InferenceDecision::AutoApply(c)
            | InferenceDecision::NeedsReview(c)
            | InferenceDecision::Blocked(c) => c,
        }
    }
    pub fn is_auto(&self) -> bool {
        matches!(self, InferenceDecision::AutoApply(_))
    }

    /// Wrap a finished certificate according to its decision. A user
    /// decision is not an inference and never comes through here.
    pub fn from_certificate(cert: InferenceCertificate) -> Self {
        match cert.decision {
            Decision::AutoApplied => InferenceDecision::AutoApply(cert),
            Decision::NeedsReview => InferenceDecision::NeedsReview(cert),
            Decision::Blocked | Decision::UserDecision => InferenceDecision::Blocked(cert),
        }
    }
}

/// Fail-closed decision from a set of predicates: any failed predicate whose
/// code is in `blocking` blocks; any other failure routes to review; all
/// passing auto-applies.
pub fn decide_from_predicates(predicates: &[Predicate], blocking: &[ReasonCode]) -> Decision {
    let failed: Vec<ReasonCode> = predicates
        .iter()
        .filter(|p| !p.passed)
        .map(|p| p.code.unwrap_or(ReasonCode::PairwiseEvidenceGap))
        .collect();
    if failed.is_empty() {
        Decision::AutoApplied
    } else if failed.iter().any(|c| blocking.contains(c)) {
        Decision::Blocked
    } else {
        Decision::NeedsReview
    }
}

pub fn sorted_unique(ids: impl IntoIterator<Item = Uuid>) -> Vec<Uuid> {
    let mut v: Vec<Uuid> = ids.into_iter().collect();
    v.sort();
    v.dedup();
    v
}

/// Stable key for a set of ids: sorted, joined.
pub fn set_key(prefix: &str, ids: &[Uuid]) -> String {
    let ids = sorted_unique(ids.iter().copied());
    let joined: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    format!("{prefix}:{}", joined.join(","))
}

/// JSON with object keys sorted, so digests don't depend on map order.
pub fn canonical_json(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let sorted: std::collections::BTreeMap<String, Value> =
                    m.iter().map(|(k, v)| (k.clone(), sort(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(v).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RULE: RuleId = RuleId {
        id: "test.rule",
        version: 1,
    };

    fn cert() -> InferenceCertificate {
        InferenceCertificate::new(ConclusionKind::EntityMerge, RULE, "k".into())
            .with_subjects([Uuid::from_u128(2), Uuid::from_u128(1)])
    }

    #[test]
    fn digest_ignores_input_and_map_order() {
        let a = cert()
            .input(EvidenceRef {
                id: Uuid::from_u128(1),
                kind: "entity".into(),
                class: EvidenceClass::Extracted,
                detail: json!({"x": 1, "y": 2}),
            })
            .input(EvidenceRef {
                id: Uuid::from_u128(2),
                kind: "entity".into(),
                class: EvidenceClass::Extracted,
                detail: json!({}),
            })
            .decide(Decision::AutoApplied);
        let b = cert()
            .input(a.inputs[1].clone())
            .input(EvidenceRef {
                detail: json!({"y": 2, "x": 1}),
                ..a.inputs[0].clone()
            })
            .decide(Decision::AutoApplied);
        assert_eq!(a.evidence_digest(), b.evidence_digest());
        let c = b.clone().decide(Decision::NeedsReview);
        assert_ne!(a.evidence_digest(), c.evidence_digest());
    }

    #[test]
    fn fail_closed_decision() {
        let ok = [Predicate::pass("a", Value::Null)];
        assert_eq!(decide_from_predicates(&ok, &[]), Decision::AutoApplied);
        let review = [
            Predicate::pass("a", Value::Null),
            Predicate::fail("b", ReasonCode::TimeScopeUnknown, Value::Null),
        ];
        assert_eq!(decide_from_predicates(&review, &[]), Decision::NeedsReview);
        assert_eq!(
            decide_from_predicates(&review, &[ReasonCode::TimeScopeUnknown]),
            Decision::Blocked
        );
    }

    #[test]
    fn explanation_defaults_to_plain_language_of_failures() {
        let c = cert()
            .predicate(Predicate::fail(
                "pairwise",
                ReasonCode::ChainedSimilarity,
                Value::Null,
            ))
            .decide(Decision::NeedsReview);
        assert!(c.explanation.contains("third record"));
        assert_eq!(c.reason_codes(), vec![ReasonCode::ChainedSimilarity]);
        assert_eq!(c.subject_ids, vec![Uuid::from_u128(1), Uuid::from_u128(2)]);
    }
}
