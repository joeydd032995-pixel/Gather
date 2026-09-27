//! Photo duplicate-group safety.
//!
//! Near-duplicate grouping keeps the existing rule — every member of a group
//! is within the distance of every other — and adds what makes it safe to run
//! unattended: a canonical order (so the split of a chain never depends on
//! the order photos arrived or rows came back), user "not a duplicate"
//! decisions as hard cannot-links, hub detection for low-information images
//! that resemble many unrelated shots, and a certificate for every group.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use uuid::Uuid;

use super::certificate::{
    set_key, ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, Predicate,
    RuleId,
};
use super::reason::ReasonCode;
use crate::photo::phash::{hamming, near_pairs, split_cliques};

pub const RULE_PHOTO_GROUP: RuleId = RuleId {
    id: "photo.duplicate_group",
    version: 2,
};

#[derive(Debug, Clone)]
pub struct PhotoInput {
    pub id: Uuid,
    pub phash: u64,
    /// Content-derived tie-break (the artifact's content hash), so identical
    /// hashes order the same way in every run.
    pub tiebreak: String,
    pub source_artifact: Uuid,
}

#[derive(Debug, Clone)]
pub struct PhotoGroup {
    /// Members in canonical order.
    pub members: Vec<Uuid>,
    pub certificate: InferenceCertificate,
}

#[derive(Debug, Clone, Default)]
pub struct PhotoPlan {
    pub groups: Vec<PhotoGroup>,
    /// Chains that were split, and hubs left out: review-routed.
    pub review: Vec<InferenceCertificate>,
    /// Pairs a person said are not duplicates.
    pub blocked: Vec<InferenceCertificate>,
}

impl PhotoPlan {
    pub fn group_sets(&self) -> Vec<Vec<Uuid>> {
        let mut g: Vec<Vec<Uuid>> = self
            .groups
            .iter()
            .map(|g| {
                let mut m = g.members.clone();
                m.sort();
                m
            })
            .collect();
        g.sort();
        g
    }
}

/// Plan duplicate groups. Pure; the result depends only on the set of photos.
pub fn plan(
    photos: &[PhotoInput],
    max_distance: u32,
    cannot_links: &[(Uuid, Uuid)],
    hub_degree: usize,
) -> PhotoPlan {
    let mut sorted: Vec<&PhotoInput> = photos.iter().collect();
    sorted.sort_by(|a, b| {
        a.phash
            .cmp(&b.phash)
            .then_with(|| a.tiebreak.cmp(&b.tiebreak))
            .then(a.id.cmp(&b.id))
    });
    let hashes: Vec<u64> = sorted.iter().map(|p| p.phash).collect();
    let index: BTreeMap<Uuid, usize> = sorted.iter().enumerate().map(|(i, p)| (p.id, i)).collect();
    let cannot: BTreeSet<(usize, usize)> = cannot_links
        .iter()
        .filter_map(|(a, b)| Some((*index.get(a)?, *index.get(b)?)))
        .map(|(i, j)| (i.min(j), i.max(j)))
        .collect();

    let mut plan = PhotoPlan::default();
    let mut edges = near_pairs(&hashes, max_distance);
    for &(i, j) in &cannot {
        if edges.remove(&(i, j)) {
            plan.blocked.push(
                InferenceCertificate::new(
                    ConclusionKind::PhotoDuplicateGroup,
                    RULE_PHOTO_GROUP,
                    set_key("photo-pair", &[sorted[i].id, sorted[j].id]),
                )
                .with_subjects([sorted[i].id, sorted[j].id])
                .with_sources([sorted[i].source_artifact, sorted[j].source_artifact])
                .input(image_input(sorted[i]))
                .input(image_input(sorted[j]))
                .predicate(Predicate::fail(
                    "no_user_rejection",
                    ReasonCode::UserRejectionExists,
                    json!({"distance": hamming(hashes[i], hashes[j])}),
                ))
                .decide(Decision::Blocked),
            );
        }
    }

    // Hubs: many near neighbours that are not near each other.
    let mut adj: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &(i, j) in &edges {
        adj.entry(i).or_default().insert(j);
        adj.entry(j).or_default().insert(i);
    }
    let mut hubs: BTreeSet<usize> = BTreeSet::new();
    for (&i, ns) in &adj {
        if ns.len() < hub_degree.max(2) {
            continue;
        }
        let ns: Vec<usize> = ns.iter().copied().collect();
        let total = ns.len() * (ns.len() - 1) / 2;
        let joined = (0..ns.len())
            .flat_map(|a| (a + 1..ns.len()).map(move |b| (a, b)))
            .filter(|&(a, b)| edges.contains(&(ns[a].min(ns[b]), ns[a].max(ns[b]))))
            .count();
        if (joined as f32) < 0.5 * total as f32 {
            hubs.insert(i);
        }
    }
    for &h in &hubs {
        let neighbours: Vec<Uuid> = adj[&h].iter().map(|&n| sorted[n].id).collect();
        let mut cert = InferenceCertificate::new(
            ConclusionKind::PhotoDuplicateGroup,
            RULE_PHOTO_GROUP,
            format!("photo-hub:{}", sorted[h].id),
        )
        .with_subjects(std::iter::once(sorted[h].id).chain(neighbours.iter().copied()))
        .with_sources([sorted[h].source_artifact])
        .input(image_input(sorted[h]))
        .predicate(Predicate::fail(
            "bounded_degree",
            ReasonCode::HubDegreeExceeded,
            json!({"degree": neighbours.len()}),
        ))
        .predicate(Predicate::fail(
            "discriminative_image",
            ReasonCode::GenericIdentifier,
            json!({}),
        ));
        cert.config = json!({"max_distance": max_distance, "hub_degree": hub_degree});
        plan.review.push(cert.decide(Decision::NeedsReview));
    }
    edges.retain(|(i, j)| !hubs.contains(i) && !hubs.contains(j));

    // Components before the split, to report chains.
    let groups = split_cliques(sorted.len(), &edges);
    let group_of: BTreeMap<usize, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(g, m)| m.iter().map(move |&i| (i, g)))
        .collect();
    let crossing: Vec<(usize, usize)> = edges
        .iter()
        .copied()
        .filter(|(i, j)| group_of.get(i) != group_of.get(j))
        .collect();
    if !crossing.is_empty() {
        // One review certificate per chained gathering of photos.
        let mut uf = crate::cluster::UnionFind::new(sorted.len());
        for &(i, j) in &edges {
            uf.union(i, j);
        }
        let mut chains: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
        for &(i, j) in &crossing {
            chains.entry(uf.find(i)).or_default().push((i, j));
        }
        for (root, pairs) in chains {
            let members: Vec<Uuid> = (0..sorted.len())
                .filter(|&i| uf.find(i) == root)
                .map(|i| sorted[i].id)
                .collect();
            let mut cert = InferenceCertificate::new(
                ConclusionKind::PhotoDuplicateGroup,
                RULE_PHOTO_GROUP,
                set_key("photo-chain", &members),
            )
            .with_subjects(members.clone())
            .with_sources(
                (0..sorted.len())
                    .filter(|&i| uf.find(i) == root)
                    .map(|i| sorted[i].source_artifact),
            )
            .predicate(Predicate::fail(
                "pairwise_complete",
                ReasonCode::PairwiseEvidenceGap,
                json!({"split_pairs": pairs
                    .iter()
                    .map(|&(i, j)| [sorted[i].id, sorted[j].id])
                    .collect::<Vec<_>>()}),
            ))
            .predicate(Predicate::fail(
                "not_chained",
                ReasonCode::ChainedSimilarity,
                json!({}),
            ));
            cert.config = json!({"max_distance": max_distance});
            plan.review.push(cert.decide(Decision::NeedsReview).explain(
                "Not grouped as one set: some of these photos are only similar through \
                 another photo, so they were kept in separate groups.",
            ));
        }
    }

    for idx in groups {
        let members: Vec<Uuid> = idx.iter().map(|&i| sorted[i].id).collect();
        let mut cert = InferenceCertificate::new(
            ConclusionKind::PhotoDuplicateGroup,
            RULE_PHOTO_GROUP,
            set_key("photo-dup", &members),
        )
        .with_subjects(members.clone())
        .with_sources(idx.iter().map(|&i| sorted[i].source_artifact))
        .predicate(Predicate::pass(
            "pairwise_complete",
            json!({"max_pair_distance": idx
                .iter()
                .flat_map(|&i| idx.iter().map(move |&j| (i, j)))
                .map(|(i, j)| hamming(hashes[i], hashes[j]))
                .max()
                .unwrap_or(0)}),
        ))
        .predicate(Predicate::pass("no_user_rejection", json!({})));
        for &i in &idx {
            cert = cert.input(image_input(sorted[i]));
        }
        cert.config = json!({"max_distance": max_distance, "hub_degree": hub_degree});
        plan.groups.push(PhotoGroup {
            members,
            certificate: cert
                .decide(Decision::AutoApplied)
                .explain("Grouped automatically: every photo is a near copy of every other."),
        });
    }
    plan
}

fn image_input(p: &PhotoInput) -> EvidenceRef {
    EvidenceRef {
        id: p.id,
        kind: "image".into(),
        class: EvidenceClass::Asserted,
        detail: json!({"phash": format!("{:016x}", p.phash)}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(n: u128, phash: u64) -> PhotoInput {
        PhotoInput {
            id: Uuid::from_u128(n),
            phash,
            tiebreak: format!("{n:04}"),
            source_artifact: Uuid::from_u128(100 + n),
        }
    }

    const A: u64 = 0xF0F0_F0F0_F0F0_F0F0;

    #[test]
    fn chains_split_the_same_way_in_any_order() {
        let b = A ^ 0b111;
        let c = b ^ (0b111 << 20);
        let photos = vec![photo(1, A), photo(2, b), photo(3, c)];
        let forward = plan(&photos, 3, &[], 8);
        let mut reversed = photos.clone();
        reversed.reverse();
        let backward = plan(&reversed, 3, &[], 8);
        assert_eq!(forward.group_sets(), backward.group_sets());
        assert_eq!(forward.group_sets().len(), 1);
        assert!(forward
            .review
            .iter()
            .any(|c| c.reason_codes().contains(&ReasonCode::ChainedSimilarity)));
    }

    #[test]
    fn a_user_rejection_splits_a_group() {
        let photos = vec![photo(1, A), photo(2, A ^ 1), photo(3, A ^ 2)];
        let p = plan(&photos, 3, &[(Uuid::from_u128(1), Uuid::from_u128(2))], 8);
        for g in p.group_sets() {
            assert!(!(g.contains(&Uuid::from_u128(1)) && g.contains(&Uuid::from_u128(2))));
        }
        assert_eq!(p.blocked.len(), 1);
    }

    #[test]
    fn a_generic_hub_is_left_out() {
        // The hub is 2 bits from each spoke; spokes are 4 bits apart.
        let hub = A;
        let spokes: Vec<u64> = (0..4).map(|k| A ^ (0b11 << (k * 8))).collect();
        let mut photos = vec![photo(1, hub)];
        for (i, s) in spokes.iter().enumerate() {
            photos.push(photo(10 + i as u128, *s));
        }
        let p = plan(&photos, 2, &[], 3);
        assert!(p.groups.is_empty());
        assert!(p
            .review
            .iter()
            .any(|c| c.reason_codes().contains(&ReasonCode::HubDegreeExceeded)));
    }
}
