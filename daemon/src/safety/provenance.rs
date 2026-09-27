//! Provenance independence: copies are not corroboration.
//!
//! An email, the note it was pasted into, a summary of that note, its
//! markdown export and the re-ingested export are five artifacts but one
//! source. Artifacts are grouped into *source families* by declared
//! derivations, version chains and identical content fingerprints; only
//! distinct families count as independent support, so N derived copies have
//! exactly the effect of one original. Independence is never assumed from a
//! missing link in the other direction: families only split where there is
//! no evidence they share an origin.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use super::certificate::{
    ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, Predicate, RuleId,
};
use super::reason::ReasonCode;
use crate::cluster::UnionFind;

pub const RULE_CANONICALIZE: RuleId = RuleId {
    id: "claim.canonicalize_exact",
    version: 1,
};
pub const RULE_CORROBORATION: RuleId = RuleId {
    id: "claim.corroboration",
    version: 1,
};

/// Evidence one claim rests on: an artifact and, when known, a fingerprint
/// of the chunk the claim was read from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceRef {
    pub artifact: Uuid,
    pub fingerprint: Option<String>,
}

/// A declared or recorded link: `child` was derived from `parent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Derivation {
    pub child: Uuid,
    pub parent: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Family {
    /// The original: the member no other member was derived from (smallest
    /// id when that is not unique).
    pub root: Uuid,
    pub members: Vec<Uuid>,
}

/// Group `sources` into families. Derivations may mention artifacts outside
/// `sources`; they still connect what they link.
pub fn families(sources: &[SourceRef], derivations: &[Derivation]) -> Vec<Family> {
    let mut ids: BTreeSet<Uuid> = sources.iter().map(|s| s.artifact).collect();
    for d in derivations {
        ids.insert(d.child);
        ids.insert(d.parent);
    }
    let ids: Vec<Uuid> = ids.into_iter().collect();
    let index: BTreeMap<Uuid, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut uf = UnionFind::new(ids.len());
    for d in derivations {
        uf.union(index[&d.child], index[&d.parent]);
    }
    let mut by_fp: BTreeMap<&str, Uuid> = BTreeMap::new();
    for s in sources {
        if let Some(fp) = s.fingerprint.as_deref() {
            match by_fp.get(fp) {
                Some(&other) => uf.union(index[&other], index[&s.artifact]),
                None => {
                    by_fp.insert(fp, s.artifact);
                }
            }
        }
    }
    let children: BTreeSet<Uuid> = derivations.iter().map(|d| d.child).collect();
    let wanted: BTreeSet<Uuid> = sources.iter().map(|s| s.artifact).collect();
    let mut groups: BTreeMap<usize, Vec<Uuid>> = BTreeMap::new();
    for (i, id) in ids.iter().enumerate() {
        groups.entry(uf.find(i)).or_default().push(*id);
    }
    let mut out: Vec<Family> = groups
        .into_values()
        .filter(|m| m.iter().any(|id| wanted.contains(id)))
        .map(|all| {
            let root = all
                .iter()
                .copied()
                .find(|id| !children.contains(id))
                .unwrap_or(all[0]);
            let members: Vec<Uuid> = all.into_iter().filter(|id| wanted.contains(id)).collect();
            Family { root, members }
        })
        .collect();
    out.sort_by_key(|f| f.root);
    out
}

/// Confidence from `independent` agreeing sources of base confidence `base`
/// (noisy-OR, capped): one family gives exactly `base`, copies add nothing.
pub fn corroborated_confidence(base: f32, independent: usize) -> f32 {
    let k = independent.max(1) as i32;
    (1.0 - (1.0 - base.clamp(0.0, 1.0)).powi(k)).min(0.99)
}

#[derive(Debug, Clone, Serialize)]
pub struct Support {
    pub artifacts: usize,
    pub families: Vec<Family>,
    pub independent_sources: usize,
    pub base_confidence: f32,
    pub effective_confidence: f32,
}

pub fn support(base: f32, sources: &[SourceRef], derivations: &[Derivation]) -> Support {
    let fams = families(sources, derivations);
    let artifacts: BTreeSet<Uuid> = sources.iter().map(|s| s.artifact).collect();
    Support {
        artifacts: artifacts.len(),
        independent_sources: fams.len(),
        effective_confidence: corroborated_confidence(base, fams.len()),
        base_confidence: base,
        families: fams,
    }
}

/// Certificates for a re-assertion: the claim was folded into an existing
/// proposition (canonicalization), and the new source either counts as
/// independent corroboration or — because it shares a family with an
/// existing source — does not.
pub fn reassertion_certificates(
    unit: Uuid,
    statement: &str,
    existing: &[SourceRef],
    new_source: &SourceRef,
    derivations: &[Derivation],
    model_version: Option<String>,
) -> (InferenceCertificate, InferenceCertificate) {
    let mut all: Vec<SourceRef> = existing.to_vec();
    all.push(new_source.clone());
    let fams = families(&all, derivations);
    let family_of = |a: Uuid| fams.iter().find(|f| f.members.contains(&a)).map(|f| f.root);
    let new_family = family_of(new_source.artifact);
    let shared = existing
        .iter()
        .any(|s| family_of(s.artifact) == new_family && s.artifact != new_source.artifact)
        || existing.iter().any(|s| s.artifact == new_source.artifact);
    let artifacts: Vec<Uuid> = all.iter().map(|s| s.artifact).collect();
    let roots: Vec<Uuid> = fams.iter().map(|f| f.root).collect();
    let reading = super::modality::classify(statement);

    let mut canonical = InferenceCertificate::new(
        ConclusionKind::ClaimCanonicalization,
        RULE_CANONICALIZE,
        format!("claim-canonical:{unit}"),
    )
    .with_subjects([unit])
    .with_sources(artifacts.clone())
    .with_families(roots.clone())
    .input(EvidenceRef {
        id: new_source.artifact,
        kind: "artifact".into(),
        class: EvidenceClass::Asserted,
        detail: json!({"fingerprint": new_source.fingerprint}),
    })
    .predicate(Predicate::pass(
        "same_normalized_statement",
        json!({"statement": statement}),
    ))
    .predicate(Predicate::pass(
        "modality_preserved",
        json!({"modality": reading.modality.as_str(), "negated": reading.negated}),
    ));
    canonical.model_version = model_version.clone();
    let canonical = canonical
        .decide(Decision::AutoApplied)
        .explain("The same statement was found again and recorded as another source.");

    let mut corroboration = InferenceCertificate::new(
        ConclusionKind::ClaimCanonicalization,
        RULE_CORROBORATION,
        format!("corroboration:{unit}:{}", new_source.artifact),
    )
    .with_subjects([unit])
    .with_sources(artifacts)
    .with_families(roots)
    .input(EvidenceRef {
        id: new_source.artifact,
        kind: "artifact".into(),
        class: EvidenceClass::Asserted,
        detail: json!({"family": new_family}),
    });
    corroboration.model_version = model_version;
    corroboration.scope = json!({
        "independent_sources": fams.len(),
        "artifacts": all.len(),
    });
    let corroboration = if shared {
        corroboration
            .predicate(Predicate::fail(
                "independent_source",
                ReasonCode::SourceNotIndependent,
                json!({"family": new_family}),
            ))
            .predicate(Predicate::fail(
                "not_a_derivation",
                ReasonCode::DerivedSourceDuplication,
                json!({}),
            ))
            .decide(Decision::Blocked)
    } else {
        corroboration
            .predicate(Predicate::pass(
                "independent_source",
                json!({"family": new_family}),
            ))
            .decide(Decision::AutoApplied)
            .explain("Confidence increased: an independent source says the same thing.")
    };
    (canonical, corroboration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(n: u128, fp: Option<&str>) -> SourceRef {
        SourceRef {
            artifact: Uuid::from_u128(n),
            fingerprint: fp.map(String::from),
        }
    }

    fn derived(child: u128, parent: u128) -> Derivation {
        Derivation {
            child: Uuid::from_u128(child),
            parent: Uuid::from_u128(parent),
        }
    }

    #[test]
    fn a_derivation_chain_is_one_family() {
        // email -> note -> summary -> export -> re-ingested export
        let sources: Vec<SourceRef> = (1..=5).map(|n| src(n, None)).collect();
        let links = [derived(2, 1), derived(3, 2), derived(4, 3), derived(5, 4)];
        let s = support(0.6, &sources, &links);
        assert_eq!(s.independent_sources, 1);
        assert_eq!(s.families[0].root, Uuid::from_u128(1));
        assert!((s.effective_confidence - 0.6).abs() < 1e-6);
    }

    #[test]
    fn independent_authors_corroborate() {
        let s = support(0.6, &[src(1, Some("x")), src(2, Some("y"))], &[]);
        assert_eq!(s.independent_sources, 2);
        assert!(s.effective_confidence > 0.6);
    }

    #[test]
    fn identical_chunks_are_copies() {
        let s = support(0.6, &[src(1, Some("same")), src(2, Some("same"))], &[]);
        assert_eq!(s.independent_sources, 1);
    }

    #[test]
    fn reassertion_from_a_copy_does_not_corroborate() {
        let (_, c) = reassertion_certificates(
            Uuid::from_u128(9),
            "I use Postgres",
            &[src(1, None)],
            &src(2, None),
            &[derived(2, 1)],
            None,
        );
        assert_eq!(c.decision, Decision::Blocked);
        assert!(c.reason_codes().contains(&ReasonCode::SourceNotIndependent));
        let (_, c) = reassertion_certificates(
            Uuid::from_u128(9),
            "I use Postgres",
            &[src(1, None)],
            &src(3, None),
            &[],
            None,
        );
        assert_eq!(c.decision, Decision::AutoApplied);
    }
}
