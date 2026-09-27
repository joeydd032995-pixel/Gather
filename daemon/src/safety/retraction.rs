//! Retraction propagation over certificates (pure).
//!
//! When evidence is withdrawn — a source deleted, a unit rejected, a merge
//! split, a pair marked "not a duplicate" — every conclusion whose
//! certificate can no longer satisfy its rule must be withdrawn too, and so
//! must conclusions built on those conclusions. A certificate needs *all* of
//! its direct inputs, and *at least one* of its source artifacts.

use std::collections::BTreeSet;

use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CertNode {
    pub id: Uuid,
    /// Direct inputs; losing any one invalidates the conclusion.
    pub inputs: Vec<Uuid>,
    /// Source artifacts; losing all of them invalidates the conclusion.
    pub sources: Vec<Uuid>,
    /// The row the certificate materialized, which later certificates may
    /// use as an input.
    pub conclusion: Option<Uuid>,
}

/// Ids of the certificates that must be retracted, to a fixpoint.
pub fn propagate(nodes: &[CertNode], withdrawn: &BTreeSet<Uuid>) -> BTreeSet<Uuid> {
    let mut gone: BTreeSet<Uuid> = withdrawn.clone();
    let mut retracted: BTreeSet<Uuid> = BTreeSet::new();
    loop {
        let mut changed = false;
        for n in nodes {
            if retracted.contains(&n.id) {
                continue;
            }
            let lost_input = n.inputs.iter().any(|i| gone.contains(i));
            let lost_sources = !n.sources.is_empty() && n.sources.iter().all(|s| gone.contains(s));
            if lost_input || lost_sources {
                retracted.insert(n.id);
                gone.insert(n.id);
                if let Some(c) = n.conclusion {
                    gone.insert(c);
                }
                changed = true;
            }
        }
        if !changed {
            return retracted;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn retraction_follows_derived_conclusions() {
        // cert 10 (conclusion 100) rests on unit 1; cert 11 rests on 100.
        let nodes = [
            CertNode {
                id: id(10),
                inputs: vec![id(1)],
                sources: vec![id(50)],
                conclusion: Some(id(100)),
            },
            CertNode {
                id: id(11),
                inputs: vec![id(100), id(2)],
                sources: vec![id(51)],
                conclusion: None,
            },
            CertNode {
                id: id(12),
                inputs: vec![id(2)],
                sources: vec![id(50), id(51)],
                conclusion: None,
            },
        ];
        let out = propagate(&nodes, &BTreeSet::from([id(1)]));
        assert_eq!(out, BTreeSet::from([id(10), id(11)]));
        // Losing one of two sources keeps a conclusion; losing both drops it.
        assert!(propagate(&nodes, &BTreeSet::from([id(50)])).contains(&id(10)));
        assert!(!propagate(&nodes, &BTreeSet::from([id(50)])).contains(&id(12)));
        assert!(propagate(&nodes, &BTreeSet::from([id(50), id(51)])).contains(&id(12)));
    }
}
