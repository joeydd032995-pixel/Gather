//! Property and metamorphic tests for the semantic safety rules. Seeded,
//! deterministic generators (no external RNG), pure rules, no database.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use gather_daemon::decide::{merge_decision, Band, MergeSignals, MergeThresholds};
use gather_daemon::safety::certificate::Decision;
use gather_daemon::safety::drift::{compare, ExtractedClaim};
use gather_daemon::safety::eval::SplitMix;
use gather_daemon::safety::identity::{
    self, partition, simulate_apply, EntityRecord, IdentityConfig, Link, PairEvidence,
};
use gather_daemon::safety::photo::{self, PhotoInput};
use gather_daemon::safety::provenance::{support, Derivation, SourceRef};
use gather_daemon::safety::retraction::{propagate, CertNode};

const CASES: u64 = 200;
const SCORES: [f32; 6] = [0.45, 0.62, 0.8, 0.9199, 0.92, 0.97];
const KINDS: [&str; 3] = ["other", "person", "organization"];

fn id(n: usize) -> Uuid {
    Uuid::from_u128(n as u128 + 1)
}

/// A random entity world: records, scored pairs and user cannot-links.
fn world(rng: &mut SplitMix) -> (Vec<EntityRecord>, Vec<PairEvidence>, Vec<(Uuid, Uuid)>) {
    let n = 4 + rng.below(9);
    let records: Vec<EntityRecord> = (0..n)
        .map(|i| EntityRecord {
            id: id(i),
            name: format!("Entity {i}"),
            kind: KINDS[if rng.below(4) == 0 { rng.below(3) } else { 0 }].to_string(),
            context: BTreeMap::new(),
            head: id(i),
            link: Link::Root,
            sources: if rng.below(10) == 0 {
                vec![]
            } else {
                vec![Uuid::from_u128(1_000 + i as u128)]
            },
        })
        .collect();
    let mut pairs = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            if rng.below(3) == 0 {
                let text = SCORES[rng.below(SCORES.len())];
                let cosine = (rng.below(3) == 0).then(|| SCORES[rng.below(SCORES.len())]);
                pairs.push(PairEvidence {
                    a: id(i),
                    b: id(j),
                    signals: MergeSignals {
                        cosine,
                        text: Some(text),
                    },
                    method: "test".into(),
                });
            }
        }
    }
    let cannot: Vec<(Uuid, Uuid)> = (0..rng.below(3))
        .map(|_| (id(rng.below(n)), id(rng.below(n))))
        .filter(|(a, b)| a != b)
        .collect();
    (records, pairs, cannot)
}

fn auto_pairs(pairs: &[PairEvidence]) -> BTreeSet<(Uuid, Uuid)> {
    let t = MergeThresholds::conservative();
    pairs
        .iter()
        .filter(|p| merge_decision(&p.signals, &t) == Band::Auto)
        .map(|p| (p.a.min(p.b), p.a.max(p.b)))
        .collect()
}

fn run(
    records: &[EntityRecord],
    pairs: &[PairEvidence],
    cannot: &[(Uuid, Uuid)],
) -> Vec<Vec<Uuid>> {
    let cfg = IdentityConfig::default();
    partition(&simulate_apply(
        records,
        &identity::plan(records, pairs, cannot, &cfg),
    ))
}

/// Every merged pair has its own direct Auto evidence, never contradicts a
/// user rejection, and never joins two different explicit types.
fn assert_safe_groups(
    groups: &[Vec<Uuid>],
    records: &[EntityRecord],
    pairs: &[PairEvidence],
    cannot: &[(Uuid, Uuid)],
) {
    let auto = auto_pairs(pairs);
    let rejected: BTreeSet<(Uuid, Uuid)> =
        cannot.iter().map(|&(a, b)| (a.min(b), a.max(b))).collect();
    let kind: BTreeMap<Uuid, &str> = records.iter().map(|r| (r.id, r.kind.as_str())).collect();
    for g in groups {
        for (i, &x) in g.iter().enumerate() {
            for &y in &g[i + 1..] {
                let k = (x.min(y), x.max(y));
                assert!(auto.contains(&k), "merged without direct evidence: {k:?}");
                assert!(!rejected.contains(&k), "merged a user-rejected pair");
                assert_eq!(kind[&x], kind[&y], "merged different kinds");
            }
        }
    }
}

// 1. Permutation invariance.
#[test]
fn permutation_invariance() {
    let mut rng = SplitMix::new(1);
    for _ in 0..CASES {
        let (records, pairs, cannot) = world(&mut rng);
        let base = run(&records, &pairs, &cannot);
        for _ in 0..4 {
            let (mut r, mut p, mut c) = (records.clone(), pairs.clone(), cannot.clone());
            rng.shuffle(&mut r);
            rng.shuffle(&mut p);
            rng.shuffle(&mut c);
            // Swapping a pair's endpoints must not matter either.
            for e in p.iter_mut().filter(|_| rng.below(2) == 0) {
                std::mem::swap(&mut e.a, &mut e.b);
            }
            assert_eq!(run(&r, &p, &c), base);
        }
    }
    let mut rng = SplitMix::new(2);
    for _ in 0..CASES {
        let photos = photo_world(&mut rng);
        let base = photo::plan(&photos, 4, &[], 3).group_sets();
        let mut p = photos.clone();
        rng.shuffle(&mut p);
        assert_eq!(photo::plan(&p, 4, &[], 3).group_sets(), base);
    }
}

fn photo_world(rng: &mut SplitMix) -> Vec<PhotoInput> {
    let centres: Vec<u64> = (0..1 + rng.below(3)).map(|_| rng.next_u64()).collect();
    (0..3 + rng.below(8))
        .map(|i| {
            let c = centres[rng.below(centres.len())];
            let mut h = c;
            for _ in 0..rng.below(4) {
                h ^= 1 << rng.below(64);
            }
            PhotoInput {
                id: id(i),
                phash: h,
                tiebreak: format!("{i:03}"),
                source_artifact: Uuid::from_u128(500 + i as u128),
            }
        })
        .collect()
}

// 2. Idempotence: applying a plan and planning again changes nothing.
#[test]
fn idempotence() {
    let mut rng = SplitMix::new(3);
    let cfg = IdentityConfig::default();
    for _ in 0..CASES {
        let (records, pairs, cannot) = world(&mut rng);
        let once = simulate_apply(&records, &identity::plan(&records, &pairs, &cannot, &cfg));
        let twice = simulate_apply(&once, &identity::plan(&once, &pairs, &cannot, &cfg));
        assert_eq!(partition(&once), partition(&twice));
        // Duplicate evidence (the same pair reported twice) is not stronger.
        let mut doubled = pairs.clone();
        doubled.extend(pairs.iter().cloned());
        assert_eq!(run(&records, &doubled, &cannot), partition(&once));
    }
}

// 3. Bridge non-amplification: adding a bridging record (strong or weak
//    links to two groups) never joins items lacking direct evidence.
#[test]
fn bridge_non_amplification() {
    let mut rng = SplitMix::new(4);
    for _ in 0..CASES {
        let (mut records, mut pairs, cannot) = world(&mut rng);
        let before = run(&records, &pairs, &cannot);
        let bridge = id(records.len());
        records.push(EntityRecord {
            id: bridge,
            name: "Bridge".into(),
            kind: "other".into(),
            context: BTreeMap::new(),
            head: bridge,
            link: Link::Root,
            sources: vec![Uuid::from_u128(9_999)],
        });
        let n = records.len() - 1;
        for _ in 0..2 + rng.below(3) {
            pairs.push(PairEvidence {
                a: bridge,
                b: id(rng.below(n)),
                signals: MergeSignals {
                    cosine: None,
                    text: Some(if rng.below(2) == 0 { 0.97 } else { 0.7 }),
                },
                method: "bridge".into(),
            });
        }
        let after = run(&records, &pairs, &cannot);
        assert_safe_groups(&after, &records, &pairs, &cannot);
        // Two items apart before the bridge are apart after it unless they
        // match each other directly.
        let auto = auto_pairs(&pairs);
        let group_of = |groups: &[Vec<Uuid>], x: Uuid| groups.iter().position(|g| g.contains(&x));
        for g in &after {
            for (i, &x) in g.iter().enumerate() {
                for &y in &g[i + 1..] {
                    let together_before = group_of(&before, x).is_some()
                        && group_of(&before, x) == group_of(&before, y);
                    assert!(together_before || auto.contains(&(x.min(y), x.max(y))));
                }
            }
        }
    }
}

// 4. N derived copies ≠ N independent roots.
#[test]
fn independent_evidence_constraint() {
    for n in 1..8usize {
        let copies: Vec<SourceRef> = (0..n)
            .map(|i| SourceRef {
                artifact: id(i),
                fingerprint: None,
            })
            .collect();
        let chain: Vec<Derivation> = (1..n)
            .map(|i| Derivation {
                child: id(i),
                parent: id(i - 1),
            })
            .collect();
        let derived = support(0.6, &copies, &chain);
        let single = support(0.6, &copies[..1], &[]);
        assert_eq!(derived.independent_sources, 1);
        assert!((derived.effective_confidence - single.effective_confidence).abs() < 1e-6);
        let roots = support(0.6, &copies, &[]);
        assert_eq!(roots.independent_sources, n);
        if n > 1 {
            assert!(roots.effective_confidence > derived.effective_confidence);
        }
        // Identical chunks are copies even without a declared link.
        let same: Vec<SourceRef> = (0..n)
            .map(|i| SourceRef {
                artifact: id(i),
                fingerprint: Some("same text".into()),
            })
            .collect();
        assert_eq!(support(0.6, &same, &[]).independent_sources, 1);
    }
}

// 5. Retraction propagation reaches a fixpoint: nothing left rests on
//    withdrawn evidence, and nothing was withdrawn that still has support.
#[test]
fn retraction_propagation() {
    let mut rng = SplitMix::new(5);
    for _ in 0..CASES {
        let evidence: Vec<Uuid> = (0..10).map(|i| Uuid::from_u128(10_000 + i)).collect();
        let n = 3 + rng.below(10);
        let mut nodes: Vec<CertNode> = Vec::new();
        for i in 0..n {
            let mut inputs: Vec<Uuid> = (0..1 + rng.below(3))
                .map(|_| evidence[rng.below(evidence.len())])
                .collect();
            if i > 0 && rng.below(2) == 0 {
                // Depend on an earlier conclusion.
                inputs.push(Uuid::from_u128(20_000 + rng.below(i) as u128));
            }
            nodes.push(CertNode {
                id: Uuid::from_u128(30_000 + i as u128),
                inputs,
                sources: (0..rng.below(3))
                    .map(|_| evidence[rng.below(evidence.len())])
                    .collect(),
                conclusion: Some(Uuid::from_u128(20_000 + i as u128)),
            });
        }
        let withdrawn: BTreeSet<Uuid> = (0..1 + rng.below(3))
            .map(|_| evidence[rng.below(evidence.len())])
            .collect();
        let out = propagate(&nodes, &withdrawn);
        let mut gone = withdrawn.clone();
        for nd in nodes.iter().filter(|nd| out.contains(&nd.id)) {
            gone.insert(nd.id);
            gone.extend(nd.conclusion);
        }
        for nd in &nodes {
            let should = nd.inputs.iter().any(|i| gone.contains(i))
                || (!nd.sources.is_empty() && nd.sources.iter().all(|s| gone.contains(s)));
            assert_eq!(out.contains(&nd.id), should, "node {:?}", nd.id);
        }
    }
}

// 6. User-decision protection: no automatic group contains a pair the user
//    rejected, directly or through a person's merge.
#[test]
fn user_decision_protection() {
    let mut rng = SplitMix::new(6);
    for _ in 0..CASES {
        let (mut records, pairs, mut cannot) = world(&mut rng);
        // A user merge of two records, and a rejection against one of them.
        let (a, b) = (rng.below(records.len()), rng.below(records.len()));
        if a != b {
            records[b].head = records[a].id;
            records[b].link = Link::User;
            let c = rng.below(records.len());
            if c != a && c != b {
                cannot.push((records[b].id, records[c].id));
            }
        }
        let groups = run(&records, &pairs, &cannot);
        let unit = |x: Uuid| {
            let r = records.iter().find(|r| r.id == x).unwrap();
            if r.link == Link::User {
                r.head
            } else {
                r.id
            }
        };
        let rejected: BTreeSet<(Uuid, Uuid)> = cannot
            .iter()
            .map(|&(x, y)| (unit(x).min(unit(y)), unit(x).max(unit(y))))
            .collect();
        for g in &groups {
            for (i, &x) in g.iter().enumerate() {
                for &y in &g[i + 1..] {
                    let k = (unit(x).min(unit(y)), unit(x).max(unit(y)));
                    assert!(k.0 == k.1 || !rejected.contains(&k));
                }
            }
        }
    }
}

// 7. Provenance completeness: every automatic merge and photo group has a
//    certificate naming its rule and at least one source artifact.
#[test]
fn provenance_completeness() {
    let mut rng = SplitMix::new(7);
    let cfg = IdentityConfig::default();
    for _ in 0..CASES {
        let (records, pairs, cannot) = world(&mut rng);
        let plan = identity::plan(&records, &pairs, &cannot, &cfg);
        for m in &plan.merges {
            let c = &m.certificate;
            assert_eq!(c.decision, Decision::AutoApplied);
            assert!(!c.rule_id.is_empty() && c.rule_version >= 1);
            assert!(
                !c.source_artifact_ids.is_empty(),
                "auto merge without a source"
            );
        }
        let photos = photo_world(&mut rng);
        for g in photo::plan(&photos, 4, &[], 3).groups {
            assert!(!g.certificate.source_artifact_ids.is_empty());
        }
    }
}

// 8. No invalid closure: a group is a clique of direct evidence; photo
//    groups are cliques within the distance.
#[test]
fn no_invalid_type_closure() {
    let mut rng = SplitMix::new(8);
    for _ in 0..CASES {
        let (records, pairs, cannot) = world(&mut rng);
        let groups = run(&records, &pairs, &cannot);
        assert_safe_groups(&groups, &records, &pairs, &cannot);
        let photos = photo_world(&mut rng);
        let hashes: BTreeMap<Uuid, u64> = photos.iter().map(|p| (p.id, p.phash)).collect();
        for g in photo::plan(&photos, 4, &[], 3).group_sets() {
            for (i, x) in g.iter().enumerate() {
                for y in &g[i + 1..] {
                    assert!((hashes[x] ^ hashes[y]).count_ones() <= 4);
                }
            }
        }
    }
}

// 9. Threshold boundary stability: documented, deterministic outcomes at
//    and around each bar.
#[test]
fn threshold_boundary_stability() {
    let t = MergeThresholds::conservative();
    let text = |x: f32| MergeSignals {
        cosine: None,
        text: Some(x),
    };
    let eps = 1e-4;
    assert_eq!(merge_decision(&text(t.auto_single), &t), Band::Auto);
    assert_eq!(merge_decision(&text(t.auto_single - eps), &t), Band::Hold);
    assert_eq!(merge_decision(&text(t.review_floor), &t), Band::Hold);
    assert_eq!(merge_decision(&text(t.review_floor - eps), &t), Band::Drop);
    let both = |c: f32, x: f32| MergeSignals {
        cosine: Some(c),
        text: Some(x),
    };
    assert_eq!(
        merge_decision(&both(t.agree_cosine, t.agree_text), &t),
        Band::Auto
    );
    assert_eq!(
        merge_decision(&both(t.agree_cosine, t.agree_text - eps), &t),
        Band::Hold
    );
    // Monotone and repeatable over a fine sweep.
    let mut prev = Band::Drop;
    for i in 0..=1000 {
        let s = i as f32 / 1000.0;
        let b = merge_decision(&text(s), &t);
        assert_eq!(b, merge_decision(&text(s), &t));
        let rank = |b: Band| match b {
            Band::Drop => 0,
            Band::Hold => 1,
            Band::Auto => 2,
        };
        assert!(rank(b) >= rank(prev));
        prev = b;
    }
}

// 10. Model-version visibility: every changed conclusion between two
//     versions is reported with both versions.
#[test]
fn model_version_visibility() {
    let mut rng = SplitMix::new(10);
    let statements = [
        "I use Redis",
        "I do not use Redis",
        "I plan to use Redis",
        "I prefer tea",
        "I moved to Chicago",
    ];
    for _ in 0..CASES {
        let claims = |rng: &mut SplitMix, model: &str, base: u128| -> Vec<ExtractedClaim> {
            (0..rng.below(5))
                .map(|i| ExtractedClaim {
                    id: Uuid::from_u128(base + i as u128),
                    anchor: format!("seg{}", rng.below(4)),
                    subject: Some("Me".into()),
                    statement: statements[rng.below(statements.len())].into(),
                    value: None,
                    model_version: model.into(),
                })
                .collect()
        };
        let old = claims(&mut rng, "v1", 0);
        let new = claims(&mut rng, "v2", 100);
        let report = compare(&old, &new);
        let seen: usize = report
            .items
            .iter()
            .map(|i| usize::from(i.old.is_some()) + usize::from(i.new.is_some()))
            .sum();
        assert_eq!(seen, old.len() + new.len(), "every claim is accounted for");
        for item in &report.items {
            if item.high_impact {
                assert_eq!(item.certificate.decision, Decision::NeedsReview);
            }
            if let (Some(o), Some(n)) = (&item.old, &item.new) {
                assert_eq!(
                    item.certificate.scope["old_model"],
                    o.model_version.as_str()
                );
                assert_eq!(
                    item.certificate.scope["new_model"],
                    n.model_version.as_str()
                );
            }
        }
    }
}
