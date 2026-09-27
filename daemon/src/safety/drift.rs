//! Model / extractor drift: two versions reading the same source.
//!
//! A new extractor version never silently rewrites what an older one
//! extracted. Its output is compared claim by claim with the old version's
//! for the same source anchor; agreement and pure additions are recorded,
//! while anything that would change historical meaning — a flipped
//! negation, a different modality or value, a claim the new model no longer
//! finds — is recorded as disagreement and routed to review with both
//! versions named.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::certificate::{
    ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, Predicate, RuleId,
};
use super::modality::classify;
use super::reason::ReasonCode;

pub const RULE_REVISION: RuleId = RuleId {
    id: "extraction.revision",
    version: 1,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedClaim {
    /// Stable id of the claim (unit id, or a fixture id).
    pub id: Uuid,
    /// The source span it came from (e.g. "artifact:segment:3").
    pub anchor: String,
    pub subject: Option<String>,
    pub statement: String,
    #[serde(default)]
    pub value: Option<String>,
    pub model_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Unchanged,
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftItem {
    pub anchor: String,
    pub subject: Option<String>,
    pub change: Change,
    /// Which aspects differ: 'polarity', 'modality', 'value', 'wording'.
    pub fields: Vec<&'static str>,
    pub old: Option<ExtractedClaim>,
    pub new: Option<ExtractedClaim>,
    pub high_impact: bool,
    #[serde(skip)]
    pub certificate: InferenceCertificate,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DriftReport {
    pub items: Vec<DriftItem>,
}

impl DriftReport {
    pub fn disagreements(&self) -> usize {
        self.items.iter().filter(|i| i.high_impact).count()
    }
}

fn differences(old: &ExtractedClaim, new: &ExtractedClaim) -> Vec<&'static str> {
    let (a, b) = (classify(&old.statement), classify(&new.statement));
    let mut f = Vec::new();
    if a.negated != b.negated {
        f.push("polarity");
    }
    if a.modality != b.modality {
        f.push("modality");
    }
    if old.value != new.value {
        f.push("value");
    }
    if crate::extract::persist::normalize_statement(&old.statement)
        != crate::extract::persist::normalize_statement(&new.statement)
    {
        f.push("wording");
    }
    f
}

/// Compare two versions' claims. Claims pair up by (anchor, subject).
pub fn compare(old: &[ExtractedClaim], new: &[ExtractedClaim]) -> DriftReport {
    type Key = (String, Option<String>);
    let key = |c: &ExtractedClaim| -> Key { (c.anchor.clone(), c.subject.clone()) };
    let mut slots: BTreeMap<Key, (Vec<&ExtractedClaim>, Vec<&ExtractedClaim>)> = BTreeMap::new();
    for c in old {
        slots.entry(key(c)).or_default().0.push(c);
    }
    for c in new {
        slots.entry(key(c)).or_default().1.push(c);
    }
    let mut report = DriftReport::default();
    for ((anchor, subject), (mut olds, mut news)) in slots {
        olds.sort_by(|a, b| a.statement.cmp(&b.statement));
        news.sort_by(|a, b| a.statement.cmp(&b.statement));
        let n = olds.len().max(news.len());
        for i in 0..n {
            let (o, nw) = (olds.get(i).copied(), news.get(i).copied());
            let (change, fields) = match (o, nw) {
                (Some(o), Some(nw)) => {
                    let f = differences(o, nw);
                    if f.is_empty() {
                        (Change::Unchanged, f)
                    } else {
                        (Change::Changed, f)
                    }
                }
                (None, Some(_)) => (Change::Added, vec![]),
                (Some(_), None) => (Change::Removed, vec![]),
                (None, None) => continue,
            };
            let high_impact = change == Change::Removed
                || fields
                    .iter()
                    .any(|f| matches!(*f, "polarity" | "modality" | "value"));
            let mut cert = InferenceCertificate::new(
                ConclusionKind::ExtractionRevision,
                RULE_REVISION,
                format!("revision:{anchor}:{}", subject.clone().unwrap_or_default()),
            )
            .with_subjects(o.iter().chain(nw.iter()).map(|c| c.id));
            for c in o.iter().chain(nw.iter()) {
                cert = cert.input(EvidenceRef {
                    id: c.id,
                    kind: "claim".into(),
                    class: EvidenceClass::Extracted,
                    detail: json!({"model": c.model_version, "statement": c.statement}),
                });
            }
            cert.model_version = nw.or(o).map(|c| c.model_version.clone());
            cert.scope = json!({
                "old_model": o.map(|c| c.model_version.clone()),
                "new_model": nw.map(|c| c.model_version.clone()),
                "change": change,
                "fields": fields,
            });
            let cert = if high_impact {
                cert.predicate(Predicate::fail(
                    "versions_agree",
                    ReasonCode::ModelDisagreement,
                    json!({"fields": fields}),
                ))
                .decide(Decision::NeedsReview)
            } else {
                cert.predicate(Predicate::pass("versions_agree", json!({"fields": fields})))
                    .decide(Decision::AutoApplied)
                    .explain("The new extractor version agrees with the old one.")
            };
            report.items.push(DriftItem {
                anchor: anchor.clone(),
                subject: subject.clone(),
                change,
                fields,
                old: o.cloned(),
                new: nw.cloned(),
                high_impact,
                certificate: cert,
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(n: u128, anchor: &str, stmt: &str, model: &str) -> ExtractedClaim {
        ExtractedClaim {
            id: Uuid::from_u128(n),
            anchor: anchor.into(),
            subject: Some("Me".into()),
            statement: stmt.into(),
            value: None,
            model_version: model.into(),
        }
    }

    #[test]
    fn a_flipped_negation_is_a_reviewed_disagreement() {
        let old = [claim(1, "seg:1", "I use Redis", "rules@1")];
        let new = [claim(2, "seg:1", "I do not use Redis", "llm@2")];
        let r = compare(&old, &new);
        assert_eq!(r.disagreements(), 1);
        let item = &r.items[0];
        assert!(item.fields.contains(&"polarity"));
        assert_eq!(item.certificate.decision, Decision::NeedsReview);
        assert_eq!(item.certificate.scope["old_model"], "rules@1");
        assert_eq!(item.certificate.scope["new_model"], "llm@2");
    }

    #[test]
    fn agreement_and_additions_are_not_disagreements() {
        let old = [claim(1, "seg:1", "I use Redis", "v1")];
        let new = [
            claim(2, "seg:1", "I use Redis.", "v2"),
            claim(3, "seg:2", "I prefer tea", "v2"),
        ];
        let r = compare(&old, &new);
        assert_eq!(r.disagreements(), 0);
        assert_eq!(r.items.len(), 2);
    }

    #[test]
    fn a_dropped_claim_is_not_silently_deleted() {
        let r = compare(&[claim(1, "seg:1", "I use Redis", "v1")], &[]);
        assert_eq!(r.items[0].change, Change::Removed);
        assert!(r.items[0].high_impact);
    }
}
