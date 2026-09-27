//! Identity safety: which entity records may be merged automatically.
//!
//! Similarity is not identity. A≈B and B≈C never imply A≈C, so a group is
//! merged automatically only when **every** pair of its members has direct
//! qualifying evidence and no safety predicate fails for any of them. The
//! planner is pure and set-based: the same records and evidence produce the
//! same plan in any input order, and it plans over *base records* (entities
//! already merged away included), so a merge made early can be withdrawn
//! when later evidence shows it was part of a chain. Only merges a person
//! made (or accepted) are treated as fixed.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{json, Value};
use uuid::Uuid;

use super::certificate::{
    set_key, ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, Predicate,
    RuleId,
};
use super::reason::ReasonCode;
use crate::cluster::{survivor_key, UnionFind};
use crate::decide::{auto_basis, best_score, merge_decision, Band, MergeBasis, MergeGate};
use crate::decide::{MergeSignals, MergeThresholds};

pub const RULE_AUTO_MERGE: RuleId = RuleId {
    id: "entity.auto_merge",
    version: 2,
};
pub const RULE_PAIR_GATE: RuleId = RuleId {
    id: "entity.pair_gate",
    version: 1,
};

/// How a record is attached to its current head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// A live entity (its own head).
    Root,
    /// Merged automatically: must be re-justified on every pass.
    Auto,
    /// Merged by a person (manual merge or accepted from the tray): fixed.
    User,
}

#[derive(Debug, Clone)]
pub struct EntityRecord {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    /// Disambiguating context from `entities.metadata` (employer, location,
    /// sense, active_from/active_to ...), string-valued.
    pub context: BTreeMap<String, String>,
    pub head: Uuid,
    pub link: Link,
    /// Artifacts whose units mention this entity.
    pub sources: Vec<Uuid>,
}

#[derive(Debug, Clone)]
pub struct PairEvidence {
    pub a: Uuid,
    pub b: Uuid,
    pub signals: MergeSignals,
    pub method: String,
}

#[derive(Debug, Clone, Copy)]
pub struct IdentityConfig {
    pub thresholds: MergeThresholds,
    pub max_component: usize,
    /// Candidate degree at which an item whose neighbours don't match each
    /// other is treated as a hub, not an identity.
    pub hub_degree: usize,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            thresholds: MergeThresholds::conservative(),
            max_component: 50,
            hub_degree: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlannedMerge {
    /// Every base record in the group, sorted.
    pub members: Vec<Uuid>,
    /// Strongest admitting edge per record, for the tuner.
    pub basis: BTreeMap<Uuid, MergeBasis>,
    pub cohesion: f32,
    pub certificate: InferenceCertificate,
}

#[derive(Debug, Clone)]
pub struct ReviewPair {
    pub a: Uuid,
    pub b: Uuid,
    pub signals: MergeSignals,
    pub method: String,
    pub chained: bool,
    pub certificate: InferenceCertificate,
}

#[derive(Debug, Clone)]
pub struct ReviewComponent {
    /// Record the tray entry is keyed on.
    pub anchor: Uuid,
    pub members: Vec<Uuid>,
    /// 'oversized-component' | 'generic-identifier'
    pub reason: &'static str,
    pub certificate: InferenceCertificate,
}

#[derive(Debug, Clone, Default)]
pub struct IdentityPlan {
    pub merges: Vec<PlannedMerge>,
    pub review_pairs: Vec<ReviewPair>,
    pub review_components: Vec<ReviewComponent>,
    pub blocked: Vec<InferenceCertificate>,
}

impl IdentityPlan {
    /// The planned partition: each merge group, sorted; singletons omitted.
    pub fn groups(&self) -> Vec<Vec<Uuid>> {
        let mut g: Vec<Vec<Uuid>> = self.merges.iter().map(|m| m.members.clone()).collect();
        g.sort();
        g
    }

    pub fn certificates(&self) -> impl Iterator<Item = &InferenceCertificate> {
        self.merges
            .iter()
            .map(|m| &m.certificate)
            .chain(self.review_pairs.iter().map(|p| &p.certificate))
            .chain(self.review_components.iter().map(|c| &c.certificate))
            .chain(self.blocked.iter())
    }
}

/// Names too generic to identify one thing on their own.
const GENERIC_NAMES: &[&str] = &[
    "project",
    "projects",
    "notes",
    "note",
    "meeting",
    "meetings",
    "document",
    "documents",
    "doc",
    "file",
    "files",
    "todo",
    "task",
    "tasks",
    "idea",
    "ideas",
    "misc",
    "untitled",
    "draft",
    "screenshot",
    "image",
    "photo",
    "test",
    "data",
    "stuff",
    "thing",
    "team",
    "app",
    "untitled document",
    "new document",
    "new note",
];

pub fn is_generic_name(name: &str) -> bool {
    let n = name
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    GENERIC_NAMES.contains(&n.as_str())
}

/// Context keys whose disagreement means "different thing" (blocked) versus
/// "possibly different" (review).
const SENSE_KEYS: &[&str] = &["sense", "disambiguator"];
const CONTEXT_KEYS: &[&str] = &["employer", "organization", "location", "role"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Auto,
    Hold,
    Drop,
    Blocked,
}

struct PairEval {
    a: Uuid,
    b: Uuid,
    signals: MergeSignals,
    method: String,
    verdict: Verdict,
    predicates: Vec<Predicate>,
}

fn year_bounds(ctx: &BTreeMap<String, String>) -> Option<(i32, i32)> {
    let year = |k: &str| ctx.get(k).and_then(|v| v.get(..4)?.parse::<i32>().ok());
    match (year("active_from"), year("active_to")) {
        (None, None) => None,
        (f, t) => Some((f.unwrap_or(i32::MIN), t.unwrap_or(i32::MAX))),
    }
}

/// Type and context predicates for one candidate pair.
fn pair_predicates(ra: &EntityRecord, rb: &EntityRecord) -> Vec<Predicate> {
    let mut out = Vec::new();
    if ra.kind != rb.kind {
        if ra.kind != "other" && rb.kind != "other" {
            out.push(Predicate::fail(
                "types_compatible",
                ReasonCode::EntityTypeMismatch,
                json!({"a": ra.kind, "b": rb.kind}),
            ));
        } else {
            out.push(Predicate::fail(
                "types_confirmed",
                ReasonCode::EntityTypeUncertain,
                json!({"a": ra.kind, "b": rb.kind}),
            ));
        }
    }
    for key in SENSE_KEYS.iter().chain(CONTEXT_KEYS) {
        if let (Some(x), Some(y)) = (ra.context.get(*key), rb.context.get(*key)) {
            if !x.eq_ignore_ascii_case(y) {
                let name = if SENSE_KEYS.contains(key) {
                    "sense_compatible"
                } else {
                    "context_compatible"
                };
                out.push(Predicate::fail(
                    name,
                    ReasonCode::ContextScopeMismatch,
                    json!({"key": key, "a": x, "b": y}),
                ));
            }
        }
    }
    if let (Some((af, at)), Some((bf, bt))) = (year_bounds(&ra.context), year_bounds(&rb.context)) {
        if at < bf || bt < af {
            out.push(Predicate::fail(
                "active_periods_overlap",
                ReasonCode::TimeWindowsNonOverlapping,
                json!({"a": [af, at], "b": [bf, bt]}),
            ));
        }
    }
    out
}

fn is_blocking(p: &Predicate) -> bool {
    !p.passed
        && (matches!(
            p.code,
            Some(ReasonCode::EntityTypeMismatch) | Some(ReasonCode::UserRejectionExists)
        ) || p.name == "sense_compatible")
}

fn norm(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Plan entity resolution over `records` (live and merged-away) from pairwise
/// `pairs`, honouring user `cannot_links`. Pure and order-independent.
pub fn plan(
    records: &[EntityRecord],
    pairs: &[PairEvidence],
    cannot_links: &[(Uuid, Uuid)],
    config: &IdentityConfig,
) -> IdentityPlan {
    let by_id: BTreeMap<Uuid, &EntityRecord> = records.iter().map(|r| (r.id, r)).collect();
    // A person's merge fixes membership; everything else stands on its own.
    let unit_of = |id: Uuid| -> Uuid {
        let r = by_id[&id];
        if r.link == Link::User {
            r.head
        } else {
            r.id
        }
    };

    // Combine duplicate evidence for one pair (text and embedding passes).
    let mut combined: BTreeMap<(Uuid, Uuid), (MergeSignals, BTreeSet<String>)> = BTreeMap::new();
    for p in pairs {
        if p.a == p.b || !by_id.contains_key(&p.a) || !by_id.contains_key(&p.b) {
            continue;
        }
        let entry = combined
            .entry(norm(p.a, p.b))
            .or_insert((MergeSignals::default(), BTreeSet::new()));
        entry.0.cosine = max_opt(entry.0.cosine, p.signals.cosine);
        entry.0.text = max_opt(entry.0.text, p.signals.text);
        entry.1.insert(p.method.clone());
    }

    let cannot: BTreeSet<(Uuid, Uuid)> = cannot_links
        .iter()
        .filter(|(a, b)| by_id.contains_key(a) && by_id.contains_key(b))
        .map(|&(a, b)| norm(unit_of(a), unit_of(b)))
        .filter(|(a, b)| a != b)
        .collect();

    let mut plan = IdentityPlan::default();

    // 1. Pair verdicts.
    let mut evals: Vec<PairEval> = Vec::new();
    for (&(a, b), (signals, methods)) in &combined {
        if unit_of(a) == unit_of(b) {
            continue;
        }
        let band = merge_decision(signals, &config.thresholds);
        let mut predicates = pair_predicates(by_id[&a], by_id[&b]);
        if cannot.contains(&norm(unit_of(a), unit_of(b))) {
            predicates.push(Predicate::fail(
                "no_user_rejection",
                ReasonCode::UserRejectionExists,
                json!({}),
            ));
        }
        let verdict = if band == Band::Drop {
            Verdict::Drop
        } else if predicates.iter().any(is_blocking) {
            Verdict::Blocked
        } else if !predicates.is_empty() {
            Verdict::Hold
        } else if band == Band::Auto {
            Verdict::Auto
        } else {
            Verdict::Hold
        };
        evals.push(PairEval {
            a,
            b,
            signals: *signals,
            method: methods.iter().cloned().collect::<Vec<_>>().join("+"),
            verdict,
            predicates,
        });
    }

    // 2. Generic names and hubs lose identity authority.
    // Degree counts only Auto-strength neighbours — the links that could
    // merge things; the hold band is too noisy to call anything a hub.
    // Whether those neighbours resemble each other counts any link.
    let mut neighbours: BTreeMap<Uuid, BTreeSet<Uuid>> = BTreeMap::new();
    let mut linked: BTreeSet<(Uuid, Uuid)> = BTreeSet::new();
    for e in &evals {
        if e.verdict == Verdict::Auto {
            neighbours.entry(e.a).or_default().insert(e.b);
            neighbours.entry(e.b).or_default().insert(e.a);
        }
        if matches!(e.verdict, Verdict::Auto | Verdict::Hold) {
            linked.insert((e.a, e.b));
        }
    }
    let mut hubs: BTreeSet<Uuid> = BTreeSet::new();
    for (&id, ns) in &neighbours {
        if ns.len() < config.hub_degree.max(2) {
            continue;
        }
        let ns: Vec<Uuid> = ns.iter().copied().collect();
        let total = ns.len() * (ns.len() - 1) / 2;
        let joined = (0..ns.len())
            .flat_map(|i| (i + 1..ns.len()).map(move |j| (i, j)))
            .filter(|&(i, j)| linked.contains(&norm(ns[i], ns[j])))
            .count();
        if (joined as f32) < 0.5 * total as f32 {
            hubs.insert(id);
        }
    }
    for e in &mut evals {
        for id in [e.a, e.b] {
            if is_generic_name(&by_id[&id].name) && e.verdict == Verdict::Auto {
                e.verdict = Verdict::Hold;
                e.predicates.push(Predicate::fail(
                    "discriminative_name",
                    ReasonCode::GenericIdentifier,
                    json!({"entity": id, "name": by_id[&id].name}),
                ));
            }
        }
    }
    for &hub in &hubs {
        let mut members = vec![hub];
        let mut inputs = Vec::new();
        for e in evals.iter_mut().filter(|e| e.a == hub || e.b == hub) {
            if !matches!(e.verdict, Verdict::Auto | Verdict::Hold) {
                continue;
            }
            let other = if e.a == hub { e.b } else { e.a };
            members.push(other);
            inputs.push(pair_input(e.a, e.b, &e.signals, &e.method));
            e.verdict = Verdict::Drop; // collapsed into the hub's single review item
        }
        members.sort();
        let mut cert = InferenceCertificate::new(
            ConclusionKind::EntityMerge,
            RULE_AUTO_MERGE,
            format!("entity-hub:{hub}"),
        )
        .with_subjects(members.clone())
        .with_sources(members.iter().flat_map(|m| by_id[m].sources.clone()))
        .predicate(Predicate::fail(
            "bounded_degree",
            ReasonCode::HubDegreeExceeded,
            json!({"entity": hub, "name": by_id[&hub].name, "degree": members.len() - 1}),
        ));
        cert.inputs = inputs;
        cert.config = config_json(config);
        plan.review_components.push(ReviewComponent {
            anchor: hub,
            members,
            reason: "generic-identifier",
            certificate: cert.decide(Decision::NeedsReview),
        });
    }

    // 3. Components of Auto edges over units.
    let units: Vec<Uuid> = {
        let s: BTreeSet<Uuid> = records.iter().map(|r| unit_of(r.id)).collect();
        s.into_iter().collect()
    };
    let unit_index: HashMap<Uuid, usize> = units.iter().enumerate().map(|(i, u)| (*u, i)).collect();
    let mut uf = UnionFind::new(units.len());
    let auto: BTreeMap<(Uuid, Uuid), &PairEval> = evals
        .iter()
        .filter(|e| e.verdict == Verdict::Auto)
        .map(|e| ((e.a, e.b), e))
        .collect();
    for &(a, b) in auto.keys() {
        uf.union(unit_index[&unit_of(a)], unit_index[&unit_of(b)]);
    }
    let mut components: BTreeMap<usize, Vec<Uuid>> = BTreeMap::new();
    for r in records {
        let root = uf.find(unit_index[&unit_of(r.id)]);
        components.entry(root).or_default().push(r.id);
    }

    let mut chained: BTreeSet<(Uuid, Uuid)> = BTreeSet::new();
    for mut members in components.into_values() {
        members.sort();
        let unit_count = members
            .iter()
            .map(|&m| unit_of(m))
            .collect::<BTreeSet<_>>()
            .len();
        if unit_count < 2 {
            continue;
        }
        let in_group: BTreeSet<Uuid> = members.iter().copied().collect();
        let group_edges: Vec<&&PairEval> = auto
            .iter()
            .filter(|((a, b), _)| in_group.contains(a) && in_group.contains(b))
            .map(|(_, e)| e)
            .collect();
        let sources: Vec<Uuid> = members
            .iter()
            .flat_map(|m| by_id[m].sources.clone())
            .collect();

        let mut cert = InferenceCertificate::new(
            ConclusionKind::EntityMerge,
            RULE_AUTO_MERGE,
            set_key("entity-merge", &members),
        )
        .with_subjects(members.clone())
        .with_sources(sources.clone());
        cert.config = config_json(config);
        for &m in &members {
            let r = by_id[&m];
            cert = cert.input(EvidenceRef {
                id: m,
                kind: "entity".into(),
                class: if r.link == Link::User {
                    EvidenceClass::UserConfirmed
                } else {
                    EvidenceClass::Extracted
                },
                detail: json!({"name": r.name, "kind": r.kind}),
            });
        }
        for e in &group_edges {
            cert = cert.input(pair_input(e.a, e.b, &e.signals, &e.method));
        }

        // Every cross-unit pair needs its own Auto edge.
        let mut missing: Vec<(Uuid, Uuid)> = Vec::new();
        let mut rejected: Vec<(Uuid, Uuid)> = Vec::new();
        for (i, &x) in members.iter().enumerate() {
            for &y in &members[i + 1..] {
                if unit_of(x) == unit_of(y) {
                    continue;
                }
                if cannot.contains(&norm(unit_of(x), unit_of(y))) {
                    rejected.push((x, y));
                }
                if !auto.contains_key(&(x, y)) {
                    missing.push((x, y));
                }
            }
        }

        let oversized = members.len() > config.max_component;
        cert = cert.predicate(if oversized {
            Predicate::fail(
                "component_size",
                ReasonCode::ComponentTooLarge,
                json!({"size": members.len(), "max": config.max_component}),
            )
        } else {
            Predicate::pass("component_size", json!({"size": members.len()}))
        });
        cert = cert.predicate(if missing.is_empty() {
            Predicate::pass("pairwise_complete", json!({"pairs": group_edges.len()}))
        } else {
            Predicate::fail(
                "pairwise_complete",
                ReasonCode::PairwiseEvidenceGap,
                json!({"missing": missing.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>()}),
            )
        });
        if !missing.is_empty() {
            cert = cert.predicate(Predicate::fail(
                "not_chained",
                ReasonCode::ChainedSimilarity,
                json!({"bridged_pairs": missing.len()}),
            ));
        }
        cert = cert.predicate(if rejected.is_empty() {
            Predicate::pass("no_user_rejection", Value::Null)
        } else {
            Predicate::fail(
                "no_user_rejection",
                ReasonCode::UserRejectionExists,
                json!({"pairs": rejected.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>()}),
            )
        });
        cert = cert.predicate(if sources.is_empty() {
            Predicate::fail(
                "has_provenance",
                ReasonCode::InsufficientProvenance,
                Value::Null,
            )
        } else {
            Predicate::pass("has_provenance", json!({"artifacts": sources.len()}))
        });

        if oversized {
            plan.review_components.push(ReviewComponent {
                anchor: members[0],
                members: members.clone(),
                reason: "oversized-component",
                certificate: cert.decide(Decision::NeedsReview),
            });
            continue;
        }
        if cert.predicates.iter().any(|p| !p.passed) {
            let cert = cert.decide(Decision::NeedsReview);
            let is_chain = !missing.is_empty();
            for e in &group_edges {
                chained.insert((e.a, e.b));
                plan.review_pairs.push(ReviewPair {
                    a: e.a,
                    b: e.b,
                    signals: e.signals,
                    method: e.method.clone(),
                    chained: is_chain,
                    certificate: cert.clone(),
                });
            }
            continue;
        }

        let mut basis: BTreeMap<Uuid, MergeBasis> = BTreeMap::new();
        let mut sum = 0.0f32;
        for e in &group_edges {
            let b = auto_basis(&e.signals, &config.thresholds).unwrap_or(MergeBasis {
                gate: MergeGate::Single,
                score: best_score(&e.signals).unwrap_or(0.0),
            });
            sum += best_score(&e.signals).unwrap_or(0.0);
            for id in [e.a, e.b] {
                basis
                    .entry(id)
                    .and_modify(|cur| {
                        if b.score > cur.score {
                            *cur = b;
                        }
                    })
                    .or_insert(b);
            }
        }
        let cohesion = if group_edges.is_empty() {
            1.0
        } else {
            sum / group_edges.len() as f32
        };
        plan.merges.push(PlannedMerge {
            members,
            basis,
            cohesion,
            certificate: cert
                .decide(Decision::AutoApplied)
                .explain("Merged automatically: every pair of names matched directly."),
        });
    }

    // 4. Held pairs (and blocked ones, which are recorded but never parked).
    for e in &evals {
        if chained.contains(&(e.a, e.b)) {
            continue;
        }
        let key = format!("entity-pair:{}:{}", e.a, e.b);
        let mut cert = InferenceCertificate::new(ConclusionKind::EntityMerge, RULE_PAIR_GATE, key)
            .with_subjects([e.a, e.b])
            .with_sources(
                by_id[&e.a]
                    .sources
                    .iter()
                    .chain(&by_id[&e.b].sources)
                    .copied(),
            )
            .input(pair_input(e.a, e.b, &e.signals, &e.method));
        cert.config = config_json(config);
        cert.predicates = e.predicates.clone();
        match e.verdict {
            Verdict::Hold => {
                if cert.predicates.is_empty() {
                    cert = cert.predicate(Predicate::fail(
                        "auto_threshold",
                        ReasonCode::PairwiseEvidenceGap,
                        json!({"score": best_score(&e.signals),
                               "auto_single": config.thresholds.auto_single}),
                    ));
                }
                plan.review_pairs.push(ReviewPair {
                    a: e.a,
                    b: e.b,
                    signals: e.signals,
                    method: e.method.clone(),
                    chained: false,
                    certificate: cert.decide(Decision::NeedsReview),
                });
            }
            Verdict::Blocked => plan.blocked.push(cert.decide(Decision::Blocked)),
            Verdict::Auto | Verdict::Drop => {}
        }
    }
    plan
}

fn max_opt(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn pair_input(a: Uuid, b: Uuid, s: &MergeSignals, method: &str) -> EvidenceRef {
    EvidenceRef {
        id: crate::cluster::pair_key(a, b),
        kind: "entity_pair".into(),
        class: EvidenceClass::Inferred,
        detail: json!({
            "a": a, "b": b,
            "text": s.text.map(round4),
            "cosine": s.cosine.map(round4),
            "method": method,
        }),
    }
}

fn round4(x: f32) -> f64 {
    (f64::from(x) * 10_000.0).round() / 10_000.0
}

fn config_json(c: &IdentityConfig) -> Value {
    json!({
        "auto_single": round4(c.thresholds.auto_single),
        "agree_cosine": round4(c.thresholds.agree_cosine),
        "agree_text": round4(c.thresholds.agree_text),
        "review_floor": round4(c.thresholds.review_floor),
        "max_component": c.max_component,
        "hub_degree": c.hub_degree,
    })
}

/// The survivor among candidate heads: most specific kind, then longest
/// name, then smallest id — a total order, so the choice never depends on
/// the order candidates were found in.
pub fn choose_survivor<'a>(candidates: impl IntoIterator<Item = &'a EntityRecord>) -> Option<Uuid> {
    candidates
        .into_iter()
        .max_by(|x, y| {
            survivor_key(&x.kind, &x.name)
                .cmp(&survivor_key(&y.kind, &y.name))
                .then(y.id.cmp(&x.id))
        })
        .map(|r| r.id)
}

/// Apply a plan to an in-memory state, exactly as the clustering worker
/// applies it to the database: automatic members that are no longer in their
/// head's planned group are detached, then each planned group is brought
/// under one survivor. Used by the evaluation harness and property tests.
pub fn simulate_apply(records: &[EntityRecord], plan: &IdentityPlan) -> Vec<EntityRecord> {
    let mut state: BTreeMap<Uuid, EntityRecord> =
        records.iter().map(|r| (r.id, r.clone())).collect();
    let group_of: HashMap<Uuid, usize> = plan
        .merges
        .iter()
        .enumerate()
        .flat_map(|(i, m)| m.members.iter().map(move |&id| (id, i)))
        .collect();
    // Detach.
    let ids: Vec<Uuid> = state.keys().copied().collect();
    for id in &ids {
        let r = &state[id];
        let together = matches!(
            (group_of.get(id), group_of.get(&r.head)),
            (Some(x), Some(y)) if x == y
        );
        if r.link == Link::Auto && !together {
            let r = state.get_mut(id).unwrap();
            r.head = r.id;
            r.link = Link::Root;
        }
    }
    // Merge.
    for m in &plan.merges {
        let heads: BTreeSet<Uuid> = m.members.iter().map(|id| state[id].head).collect();
        let survivor = choose_survivor(heads.iter().map(|h| &state[h])).expect("non-empty");
        for id in &m.members {
            let r = state.get_mut(id).unwrap();
            if r.id == survivor {
                r.head = survivor;
                r.link = Link::Root;
            } else if r.head != survivor {
                r.head = survivor;
                if r.link != Link::User {
                    r.link = Link::Auto;
                }
            }
        }
    }
    state.into_values().collect()
}

/// Partition of records by head, as sorted groups of 2+.
pub fn partition(records: &[EntityRecord]) -> Vec<Vec<Uuid>> {
    let mut by_head: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for r in records {
        by_head.entry(r.head).or_default().push(r.id);
    }
    let mut groups: Vec<Vec<Uuid>> = by_head
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort();
            g
        })
        .collect();
    groups.sort();
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: u128, name: &str, kind: &str) -> EntityRecord {
        let id = Uuid::from_u128(n);
        EntityRecord {
            id,
            name: name.into(),
            kind: kind.into(),
            context: BTreeMap::new(),
            head: id,
            link: Link::Root,
            sources: vec![Uuid::from_u128(1000 + n)],
        }
    }

    fn pair(a: u128, b: u128, text: f32) -> PairEvidence {
        PairEvidence {
            a: Uuid::from_u128(a),
            b: Uuid::from_u128(b),
            signals: MergeSignals {
                cosine: None,
                text: Some(text),
            },
            method: "rule:name-similarity".into(),
        }
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn pairs_merge_and_chains_do_not() {
        let records = [
            rec(1, "Acme", "other"),
            rec(2, "Acme.", "other"),
            rec(3, "A", "other"),
            rec(4, "B", "other"),
            rec(5, "C", "other"),
        ];
        let pairs = [pair(1, 2, 0.95), pair(3, 4, 0.95), pair(4, 5, 0.95)];
        let p = plan(&records, &pairs, &[], &IdentityConfig::default());
        assert_eq!(p.groups(), vec![vec![id(1), id(2)]]);
        assert_eq!(p.review_pairs.len(), 2);
        assert!(p.review_pairs.iter().all(|r| r.chained));
        let codes = p.review_pairs[0].certificate.reason_codes();
        assert!(codes.contains(&ReasonCode::ChainedSimilarity));
        assert!(codes.contains(&ReasonCode::PairwiseEvidenceGap));
    }

    #[test]
    fn an_early_merge_is_withdrawn_when_a_chain_appears() {
        // State after an earlier pass: B was auto-merged into A.
        let mut records = vec![rec(1, "A", "other"), rec(2, "B", "other")];
        records[1].head = id(1);
        records[1].link = Link::Auto;
        records.push(rec(3, "C", "other"));
        let pairs = [pair(1, 2, 0.95), pair(2, 3, 0.95)];
        let p = plan(&records, &pairs, &[], &IdentityConfig::default());
        assert!(p.merges.is_empty());
        let after = simulate_apply(&records, &p);
        assert!(partition(&after).is_empty(), "the chained merge is undone");
    }

    #[test]
    fn a_user_merge_is_fixed_and_needs_every_pair_for_newcomers() {
        let mut records = vec![rec(1, "A", "other"), rec(2, "B", "other")];
        records[1].head = id(1);
        records[1].link = Link::User;
        records.push(rec(3, "C", "other"));
        let pairs = [pair(1, 3, 0.95)];
        let p = plan(&records, &pairs, &[], &IdentityConfig::default());
        assert!(p.merges.is_empty(), "C matches A but not B");
        let after = simulate_apply(&records, &p);
        assert_eq!(partition(&after), vec![vec![id(1), id(2)]]);
    }

    #[test]
    fn user_rejection_through_a_merged_member_blocks() {
        let mut records = vec![rec(1, "A", "other"), rec(2, "B", "other")];
        records[1].head = id(1);
        records[1].link = Link::User;
        records.push(rec(3, "C", "other"));
        let pairs = [pair(1, 3, 0.95), pair(2, 3, 0.95)];
        let p = plan(
            &records,
            &pairs,
            &[(id(2), id(3))],
            &IdentityConfig::default(),
        );
        assert!(p.merges.is_empty());
        assert!(p
            .blocked
            .iter()
            .any(|c| c.reason_codes().contains(&ReasonCode::UserRejectionExists)));
    }

    #[test]
    fn types_and_context_gate_merges() {
        let mut records = vec![
            rec(1, "Apple", "organization"),
            rec(2, "apple", "other"),
            rec(3, "Jordan", "person"),
            rec(4, "Jordan", "location"),
            rec(5, "Sam Lee", "person"),
            rec(6, "Sam Lee.", "person"),
        ];
        records[4]
            .context
            .insert("employer".into(), "Initech".into());
        records[5]
            .context
            .insert("employer".into(), "Globex".into());
        let pairs = [pair(1, 2, 1.0), pair(3, 4, 1.0), pair(5, 6, 0.97)];
        let p = plan(&records, &pairs, &[], &IdentityConfig::default());
        assert!(p.merges.is_empty());
        assert!(p
            .blocked
            .iter()
            .any(|c| c.reason_codes().contains(&ReasonCode::EntityTypeMismatch)));
        let held: Vec<Vec<ReasonCode>> = p
            .review_pairs
            .iter()
            .map(|r| r.certificate.reason_codes())
            .collect();
        assert!(held
            .iter()
            .any(|c| c.contains(&ReasonCode::EntityTypeUncertain)));
        assert!(held
            .iter()
            .any(|c| c.contains(&ReasonCode::ContextScopeMismatch)));
    }

    #[test]
    fn a_hub_does_not_join_its_neighbours() {
        let records = [
            rec(1, "John", "other"),
            rec(2, "John Smith", "other"),
            rec(3, "John Doe", "other"),
            rec(4, "John Lee", "other"),
        ];
        let pairs = [pair(1, 2, 0.95), pair(1, 3, 0.95), pair(1, 4, 0.95)];
        let p = plan(&records, &pairs, &[], &IdentityConfig::default());
        assert!(p.merges.is_empty());
        assert_eq!(p.review_components.len(), 1);
        assert_eq!(p.review_components[0].reason, "generic-identifier");
        assert!(p.review_pairs.is_empty(), "collapsed into one item");
    }

    #[test]
    fn missing_provenance_routes_to_review() {
        let mut records = vec![rec(1, "Acme", "other"), rec(2, "Acme.", "other")];
        records[0].sources.clear();
        records[1].sources.clear();
        let p = plan(
            &records,
            &[pair(1, 2, 0.95)],
            &[],
            &IdentityConfig::default(),
        );
        assert!(p.merges.is_empty());
        assert!(p.review_pairs[0]
            .certificate
            .reason_codes()
            .contains(&ReasonCode::InsufficientProvenance));
    }

    #[test]
    fn survivor_is_a_total_order() {
        let a = rec(1, "Postgres", "other");
        let b = rec(2, "PostgreSQL", "other");
        let c = rec(3, "PG", "tool");
        assert_eq!(choose_survivor([&a, &b, &c]), Some(id(3)));
        assert_eq!(choose_survivor([&a, &b]), Some(id(2)));
    }
}
