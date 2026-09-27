//! Semantic-safety evaluation: run the fixture corpus through the pure
//! rules and check the global invariants. Deterministic, offline, no
//! database: it is what `gather-semantic-eval` runs in CI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::certificate::{Decision, InferenceCertificate};
use super::contradiction::{alignment_complete, evaluate, Claim, ContradictionContext};
use super::drift::{compare, ExtractedClaim};
use super::identity::{self, EntityRecord, IdentityConfig, Link, PairEvidence};
use super::modality::{classify, proposition_key};
use super::photo::{self, PhotoInput};
use super::provenance::{self, Derivation, SourceRef};
use super::reason::ReasonCode;
use super::retraction::{propagate, CertNode};
use super::temporal::{TemporalPolicy, TimeScope};
use crate::decide::{merge_decision, Band, MergeSignals, MergeThresholds};
use crate::scan::score::{score_pair, UnitFacts};

/// Stable UUID for a fixture id, so every run (and every order) uses the
/// same identifiers.
pub fn fid(id: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("gather-fixture:{id}").as_bytes(),
    )
}

// ---------------------------------------------------------------------------
// Fixture schema

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Fixture {
    pub id: String,
    pub category: String,
    pub description: String,
    pub entities: Vec<FxEntity>,
    pub pairs: Vec<FxPair>,
    pub cannot_links: Vec<[String; 2]>,
    pub existing: Vec<FxExisting>,
    pub photos: Vec<FxPhoto>,
    pub max_distance: Option<u32>,
    pub hub_degree: Option<usize>,
    pub units: Vec<FxUnit>,
    pub part_of: Vec<[String; 2]>,
    pub checks: Vec<FxCheck>,
    pub sources: Vec<FxSource>,
    pub derivations: Vec<[String; 2]>,
    pub base_confidence: Option<f32>,
    pub statements: Vec<FxStatement>,
    pub certificates: Vec<FxCert>,
    pub withdraw: Vec<String>,
    pub old_claims: Vec<FxClaim>,
    pub new_claims: Vec<FxClaim>,
    pub permutations: Option<usize>,
    pub seed: Option<u64>,
    pub expect: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FxEntity {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub context: BTreeMap<String, String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxPair {
    pub a: String,
    pub b: String,
    #[serde(default)]
    pub text: Option<f32>,
    #[serde(default)]
    pub cosine: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxExisting {
    pub id: String,
    pub head: String,
    /// "auto" | "user"
    pub link: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxPhoto {
    pub id: String,
    /// 16 hex digits.
    pub phash: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FxUnit {
    pub id: String,
    pub statement: String,
    pub subject: Option<String>,
    pub attrs: Value,
    pub asserted_at: Option<DateTime<Utc>>,
    pub observed_at: Option<DateTime<Utc>>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub assignments: Vec<[String; 3]>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxCheck {
    pub pair: [String; 2],
    /// "none" | "auto_applied" | "needs_review" | "blocked"
    pub outcome: String,
    #[serde(default)]
    pub reasons_include: Vec<String>,
    /// [older, newer] when a supersession is expected.
    #[serde(default)]
    pub supersedes: Option<[String; 2]>,
    #[serde(default)]
    pub user_rejected: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxSource {
    pub artifact: String,
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxStatement {
    pub text: String,
    pub modality: String,
    #[serde(default)]
    pub negated: bool,
    #[serde(default)]
    pub positive_fact: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxCert {
    pub id: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FxClaim {
    pub id: String,
    pub anchor: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub statement: String,
    #[serde(default)]
    pub value: Option<String>,
    pub model: String,
}

pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<Fixture>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let text = std::fs::read_to_string(&p)?;
        let fx: Fixture =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
        out.push(fx);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Report

#[derive(Debug, Clone, Serialize, Default)]
pub struct ScenarioResult {
    pub id: String,
    pub category: String,
    pub passed: bool,
    pub failures: Vec<String>,
    /// Deterministic summary of the semantic outcome (fixture ids, decisions).
    pub outcome: Value,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Report {
    pub total_scenarios: usize,
    pub passed: usize,
    pub failed: usize,
    pub invariant_failures: Vec<String>,
    pub false_auto_actions: usize,
    pub blocked_by_reason: BTreeMap<String, usize>,
    pub review_by_reason: BTreeMap<String, usize>,
    pub retraction_propagation_failures: usize,
    pub missing_certificates: usize,
    pub model_disagreements: usize,
    pub runtime_ms: u128,
    /// sha256 over every scenario's outcome digest, in id order.
    pub digest: String,
    pub scenarios: Vec<ScenarioResult>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.failed == 0 && self.invariant_failures.is_empty()
    }
}

/// Accumulates counts and invariant failures across scenarios.
#[derive(Default)]
struct Tally {
    invariant_failures: Vec<String>,
    false_auto: usize,
    blocked: BTreeMap<String, usize>,
    review: BTreeMap<String, usize>,
    retraction_failures: usize,
    missing_certificates: usize,
    model_disagreements: usize,
}

impl Tally {
    fn count(&mut self, cert: &InferenceCertificate) {
        let map = match cert.decision {
            Decision::Blocked => &mut self.blocked,
            Decision::NeedsReview => &mut self.review,
            _ => return,
        };
        for c in cert.reason_codes() {
            *map.entry(c.as_str().to_string()).or_default() += 1;
        }
    }

    /// Every automatic conclusion needs a certificate naming its rule and at
    /// least one source artifact.
    fn check_certificate(&mut self, scenario: &str, cert: &InferenceCertificate) {
        if cert.decision == Decision::AutoApplied
            && (cert.rule_id.is_empty()
                || cert.rule_version < 1
                || cert.source_artifact_ids.is_empty())
        {
            self.missing_certificates += 1;
            self.invariant_failures.push(format!(
                "{scenario}: automatic conclusion {} lacks a complete certificate",
                cert.conclusion_key
            ));
        }
    }
}

fn digest_of(v: &Value) -> String {
    hex::encode(Sha256::digest(
        super::certificate::canonical_json(v).as_bytes(),
    ))
}

// ---------------------------------------------------------------------------
// Deterministic shuffling (no rand dependency)

pub struct SplitMix(u64);

impl SplitMix {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario runners

struct Names(BTreeMap<Uuid, String>);

impl Names {
    fn from<'a>(ids: impl IntoIterator<Item = &'a String>) -> Self {
        Names(ids.into_iter().map(|s| (fid(s), s.clone())).collect())
    }
    fn name(&self, id: &Uuid) -> String {
        self.0.get(id).cloned().unwrap_or_else(|| id.to_string())
    }
    fn groups(&self, groups: &[Vec<Uuid>]) -> Vec<Vec<String>> {
        let mut g: Vec<Vec<String>> = groups
            .iter()
            .map(|m| {
                let mut v: Vec<String> = m.iter().map(|id| self.name(id)).collect();
                v.sort();
                v
            })
            .collect();
        g.sort();
        g
    }
}

fn entity_records(fx: &Fixture) -> Vec<EntityRecord> {
    let existing: BTreeMap<&str, &FxExisting> =
        fx.existing.iter().map(|e| (e.id.as_str(), e)).collect();
    fx.entities
        .iter()
        .map(|e| {
            let (head, link) = match existing.get(e.id.as_str()) {
                Some(x) => (
                    fid(&x.head),
                    if x.link == "user" {
                        Link::User
                    } else {
                        Link::Auto
                    },
                ),
                None => (fid(&e.id), Link::Root),
            };
            EntityRecord {
                id: fid(&e.id),
                name: e.name.clone(),
                kind: if e.kind.is_empty() {
                    "other".into()
                } else {
                    e.kind.clone()
                },
                context: e.context.clone(),
                head,
                link,
                sources: e.sources.iter().map(|s| fid(s)).collect(),
            }
        })
        .collect()
}

fn entity_pairs(fx: &Fixture) -> Vec<PairEvidence> {
    fx.pairs
        .iter()
        .map(|p| PairEvidence {
            a: fid(&p.a),
            b: fid(&p.b),
            signals: MergeSignals {
                cosine: p.cosine,
                text: p.text,
            },
            method: if p.cosine.is_some() {
                "embedding:cosine".into()
            } else {
                "rule:name-similarity".into()
            },
        })
        .collect()
}

fn cannot(fx: &Fixture) -> Vec<(Uuid, Uuid)> {
    fx.cannot_links
        .iter()
        .map(|[a, b]| (fid(a), fid(b)))
        .collect()
}

fn identity_config(fx: &Fixture) -> IdentityConfig {
    IdentityConfig {
        thresholds: MergeThresholds::conservative(),
        max_component: 50,
        hub_degree: fx.hub_degree.unwrap_or(3),
    }
}

fn expect_list(expect: &Value, key: &str) -> Vec<String> {
    expect
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn expect_groups(expect: &Value, key: &str) -> Option<Vec<Vec<String>>> {
    let arr = expect.get(key)?.as_array()?;
    let mut g: Vec<Vec<String>> = arr
        .iter()
        .map(|m| {
            let mut v: Vec<String> = m
                .as_array()
                .map(|x| {
                    x.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        })
        .collect();
    g.sort();
    Some(g)
}

fn reasons_of<'a>(certs: impl IntoIterator<Item = &'a InferenceCertificate>) -> BTreeSet<String> {
    certs
        .into_iter()
        .flat_map(|c| c.reason_codes())
        .map(|c| c.as_str().to_string())
        .collect()
}

/// Check the pairwise-evidence invariant for a set of planned groups.
fn check_pairwise(
    scenario: &str,
    groups: &[Vec<Uuid>],
    pairs: &[PairEvidence],
    cannot: &[(Uuid, Uuid)],
    tally: &mut Tally,
) {
    let t = MergeThresholds::conservative();
    let auto: BTreeSet<(Uuid, Uuid)> = pairs
        .iter()
        .filter(|p| merge_decision(&p.signals, &t) == Band::Auto)
        .map(|p| (p.a.min(p.b), p.a.max(p.b)))
        .collect();
    let rejected: BTreeSet<(Uuid, Uuid)> =
        cannot.iter().map(|&(a, b)| (a.min(b), a.max(b))).collect();
    for g in groups {
        for (i, &x) in g.iter().enumerate() {
            for &y in &g[i + 1..] {
                let k = (x.min(y), x.max(y));
                if !auto.contains(&k) {
                    tally.false_auto += 1;
                    tally.invariant_failures.push(format!(
                        "{scenario}: auto-merged pair lacks direct qualifying evidence"
                    ));
                }
                if rejected.contains(&k) {
                    tally.false_auto += 1;
                    tally.invariant_failures.push(format!(
                        "{scenario}: auto-merged a pair the user marked as different"
                    ));
                }
            }
        }
    }
}

fn run_identity(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let records = entity_records(fx);
    let pairs = entity_pairs(fx);
    let cannot = cannot(fx);
    let config = identity_config(fx);
    let plan = identity::plan(&records, &pairs, &cannot, &config);
    let names = Names::from(fx.entities.iter().map(|e| &e.id));
    let after = identity::simulate_apply(&records, &plan);
    let partition = identity::partition(&after);
    check_pairwise(&fx.id, &plan.groups(), &pairs, &cannot, tally);
    for c in plan.certificates() {
        tally.count(c);
        tally.check_certificate(&fx.id, c);
    }
    let got = names.groups(&partition);
    if let Some(want) = expect_groups(&fx.expect, "merge_groups") {
        if got != want {
            failures.push(format!("merge groups: got {got:?}, want {want:?}"));
        }
    }
    let reasons = reasons_of(plan.certificates());
    for r in expect_list(&fx.expect, "reasons_include") {
        if !reasons.contains(&r) {
            failures.push(format!("missing reason {r} (got {reasons:?})"));
        }
    }
    if let Some(n) = fx.expect.get("review_items_min").and_then(Value::as_u64) {
        let items = plan.review_pairs.len() + plan.review_components.len();
        if (items as u64) < n {
            failures.push(format!("expected at least {n} review items, got {items}"));
        }
    }
    // Order invariance within the scenario.
    let mut rng = SplitMix::new(fx.seed.unwrap_or(7));
    for _ in 0..fx.permutations.unwrap_or(12) {
        let mut r = records.clone();
        let mut p = pairs.clone();
        rng.shuffle(&mut r);
        rng.shuffle(&mut p);
        let again = identity::plan(&r, &p, &cannot, &config);
        let part = identity::partition(&identity::simulate_apply(&r, &again));
        if names.groups(&part) != got {
            tally.invariant_failures.push(format!(
                "{}: entity partition depends on input order",
                fx.id
            ));
            failures.push("order-dependent partition".into());
            break;
        }
    }
    json!({
        "partition": got,
        "reasons": reasons,
        "review_items": plan.review_pairs.len() + plan.review_components.len(),
        "blocked": plan.blocked.len(),
    })
}

fn photo_inputs(fx: &Fixture) -> Vec<PhotoInput> {
    fx.photos
        .iter()
        .map(|p| PhotoInput {
            id: fid(&p.id),
            phash: u64::from_str_radix(p.phash.trim_start_matches("0x"), 16).unwrap_or(0),
            tiebreak: p.id.clone(),
            source_artifact: fid(p.source.as_deref().unwrap_or(&p.id)),
        })
        .collect()
}

fn run_photo(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let photos = photo_inputs(fx);
    let cannot = cannot(fx);
    let d = fx.max_distance.unwrap_or(6);
    let hub = fx.hub_degree.unwrap_or(3);
    let plan = photo::plan(&photos, d, &cannot, hub);
    let names = Names::from(fx.photos.iter().map(|p| &p.id));
    let got = names.groups(&plan.group_sets());
    let hashes: BTreeMap<Uuid, u64> = photos.iter().map(|p| (p.id, p.phash)).collect();
    let rejected: BTreeSet<(Uuid, Uuid)> =
        cannot.iter().map(|&(a, b)| (a.min(b), a.max(b))).collect();
    for g in plan.group_sets() {
        for (i, x) in g.iter().enumerate() {
            for y in &g[i + 1..] {
                if crate::photo::phash::hamming(hashes[x], hashes[y]) > d
                    || rejected.contains(&(*x.min(y), *x.max(y)))
                {
                    tally.false_auto += 1;
                    tally.invariant_failures.push(format!(
                        "{}: photo group member lacks direct match with another member",
                        fx.id
                    ));
                }
            }
        }
    }
    let certs: Vec<&InferenceCertificate> = plan
        .groups
        .iter()
        .map(|g| &g.certificate)
        .chain(&plan.review)
        .chain(&plan.blocked)
        .collect();
    for c in &certs {
        tally.count(c);
        tally.check_certificate(&fx.id, c);
    }
    if let Some(want) = expect_groups(&fx.expect, "groups") {
        if got != want {
            failures.push(format!("photo groups: got {got:?}, want {want:?}"));
        }
    }
    let reasons = reasons_of(certs.iter().copied());
    for r in expect_list(&fx.expect, "reasons_include") {
        if !reasons.contains(&r) {
            failures.push(format!("missing reason {r} (got {reasons:?})"));
        }
    }
    let mut rng = SplitMix::new(fx.seed.unwrap_or(11));
    for _ in 0..fx.permutations.unwrap_or(12) {
        let mut p = photos.clone();
        rng.shuffle(&mut p);
        if names.groups(&photo::plan(&p, d, &cannot, hub).group_sets()) != got {
            tally
                .invariant_failures
                .push(format!("{}: photo groups depend on input order", fx.id));
            failures.push("order-dependent photo groups".into());
            break;
        }
    }
    json!({"groups": got, "reasons": reasons})
}

fn unit_facts(u: &FxUnit) -> (UnitFacts, TimeScope) {
    (
        UnitFacts {
            id: fid(&u.id),
            statement: u.statement.clone(),
            attrs: if u.attrs.is_null() {
                json!({})
            } else {
                u.attrs.clone()
            },
            subject_entity_id: u.subject.as_deref().map(fid),
            valid_from: u.valid_from,
            valid_to: u.valid_to,
            assignments: u
                .assignments
                .iter()
                .map(|[s, r, t]| (fid(s), r.clone(), fid(t)))
                .collect(),
        },
        TimeScope {
            asserted_at: u.asserted_at,
            observed_at: u.observed_at,
            valid_from: u.valid_from,
            valid_to: u.valid_to,
            ingested_at: None,
        },
    )
}

fn run_claims(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let units: BTreeMap<&str, &FxUnit> = fx.units.iter().map(|u| (u.id.as_str(), u)).collect();
    let mut part_of: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for [c, p] in &fx.part_of {
        part_of.entry(fid(c)).or_default().push(fid(p));
    }
    let mut out = Vec::new();
    for check in &fx.checks {
        let (Some(ua), Some(ub)) = (
            units.get(check.pair[0].as_str()),
            units.get(check.pair[1].as_str()),
        ) else {
            failures.push(format!("unknown unit in check {:?}", check.pair));
            continue;
        };
        let (fa, ta) = unit_facts(ua);
        let (fb, tb) = unit_facts(ub);
        let Some(conflict) = score_pair(&fa, &fb, None) else {
            if check.outcome != "none" {
                failures.push(format!(
                    "{:?}: no structural conflict, expected {}",
                    check.pair, check.outcome
                ));
            }
            out.push(json!({"pair": check.pair, "outcome": "none"}));
            continue;
        };
        let ctx = ContradictionContext {
            part_of: part_of.clone(),
            user_rejected: check.user_rejected,
            user_agreed_compatible: false,
            policy: TemporalPolicy::default(),
            sources_a: ua.sources.iter().map(|s| fid(s)).collect(),
            sources_b: ub.sources.iter().map(|s| fid(s)).collect(),
            model_version: None,
        };
        let e = evaluate(
            Claim {
                facts: &fa,
                time: &ta,
            },
            Claim {
                facts: &fb,
                time: &tb,
            },
            &conflict,
            &ctx,
        );
        let cert = e.decision.certificate();
        tally.count(cert);
        tally.check_certificate(&fx.id, cert);
        if cert.decision != Decision::Blocked {
            let alignment = serde_json::to_value(&e.alignment).unwrap_or(Value::Null);
            if !alignment_complete(&alignment) {
                tally.invariant_failures.push(format!(
                    "{}: a reported contradiction lacks alignment fields",
                    fx.id
                ));
            }
        }
        if check.user_rejected && cert.decision != Decision::Blocked {
            tally.invariant_failures.push(format!(
                "{}: a contradiction the user rejected was reported again",
                fx.id
            ));
        }
        let outcome = cert.decision.as_str();
        if outcome != check.outcome {
            failures.push(format!(
                "{:?}: outcome {outcome}, expected {} ({:?})",
                check.pair,
                check.outcome,
                cert.reason_codes()
            ));
        }
        let reasons: BTreeSet<String> = cert
            .reason_codes()
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        for r in &check.reasons_include {
            if !reasons.contains(r) {
                failures.push(format!(
                    "{:?}: missing reason {r} ({reasons:?})",
                    check.pair
                ));
            }
        }
        let sup = e.supersession.as_ref().map(|s| {
            let names = Names::from(fx.units.iter().map(|u| &u.id));
            [names.name(&s.older), names.name(&s.newer)]
        });
        if let Some(c) = &e.supersession {
            tally.check_certificate(&fx.id, &c.certificate);
        }
        match (&check.supersedes, &sup) {
            (Some(want), Some(got)) if want != got => failures.push(format!(
                "{:?}: supersedes {got:?}, want {want:?}",
                check.pair
            )),
            (Some(want), None) => {
                failures.push(format!("{:?}: expected supersession {want:?}", check.pair))
            }
            (None, Some(got)) => {
                failures.push(format!("{:?}: unexpected supersession {got:?}", check.pair))
            }
            _ => {}
        }
        out.push(json!({
            "pair": check.pair,
            "outcome": outcome,
            "reasons": reasons,
            "supersedes": sup,
        }));
    }
    json!({"checks": out})
}

fn run_provenance(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let sources: Vec<SourceRef> = fx
        .sources
        .iter()
        .map(|s| SourceRef {
            artifact: fid(&s.artifact),
            fingerprint: s.fingerprint.clone(),
        })
        .collect();
    let derivations: Vec<Derivation> = fx
        .derivations
        .iter()
        .map(|[child, parent]| Derivation {
            child: fid(child),
            parent: fid(parent),
        })
        .collect();
    let base = fx.base_confidence.unwrap_or(0.6);
    let s = provenance::support(base, &sources, &derivations);
    if let Some(n) = fx.expect.get("independent_sources").and_then(Value::as_u64) {
        if s.independent_sources as u64 != n {
            failures.push(format!(
                "independent sources {} != {n}",
                s.independent_sources
            ));
        }
    }
    let derived_only = fx
        .expect
        .get("derived_chain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if derived_only && (s.effective_confidence - base).abs() > 1e-6 {
        tally.invariant_failures.push(format!(
            "{}: a derived source chain was counted as independent corroboration",
            fx.id
        ));
    }
    if let Some(true) = fx
        .expect
        .get("confidence_increases")
        .and_then(Value::as_bool)
    {
        if s.effective_confidence <= base {
            failures.push("independent sources did not raise confidence".into());
        }
    }
    // Each re-assertion after the first, as the pipeline sees it.
    for i in 1..sources.len() {
        let (_, c) = provenance::reassertion_certificates(
            fid(&fx.id),
            "claim",
            &sources[..i],
            &sources[i],
            &derivations,
            None,
        );
        tally.count(&c);
        tally.check_certificate(&fx.id, &c);
    }
    let roots: Vec<String> = s
        .families
        .iter()
        .map(|f| {
            fx.sources
                .iter()
                .map(|x| &x.artifact)
                .chain(fx.derivations.iter().flatten())
                .find(|n| fid(n) == f.root)
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    json!({
        "independent_sources": s.independent_sources,
        "effective_confidence": (f64::from(s.effective_confidence) * 1e4).round() / 1e4,
        "roots": roots,
    })
}

fn run_modality(fx: &Fixture, _tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let mut keys = BTreeSet::new();
    let mut out = Vec::new();
    for s in &fx.statements {
        let r = classify(&s.text);
        if r.modality.as_str() != s.modality {
            failures.push(format!(
                "'{}': modality {:?}, want {}",
                s.text, r.modality, s.modality
            ));
        }
        if r.negated != s.negated {
            failures.push(format!(
                "'{}': negated {}, want {}",
                s.text, r.negated, s.negated
            ));
        }
        if r.asserts_positive_fact() != s.positive_fact {
            failures.push(format!(
                "'{}': asserts a present fact = {}, want {}",
                s.text,
                r.asserts_positive_fact(),
                s.positive_fact
            ));
        }
        keys.insert(proposition_key(&s.text));
        out.push(json!({"text": s.text, "modality": r.modality, "negated": r.negated}));
    }
    if let Some(n) = fx
        .expect
        .get("distinct_propositions")
        .and_then(Value::as_u64)
    {
        if keys.len() as u64 != n {
            failures.push(format!("{} distinct propositions, want {n}", keys.len()));
        }
    }
    json!({"readings": out, "distinct": keys.len()})
}

fn run_retraction(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let nodes: Vec<CertNode> = fx
        .certificates
        .iter()
        .map(|c| CertNode {
            id: fid(&c.id),
            inputs: c.inputs.iter().map(|s| fid(s)).collect(),
            sources: c.sources.iter().map(|s| fid(s)).collect(),
            conclusion: c.conclusion.as_deref().map(fid),
        })
        .collect();
    let withdrawn: BTreeSet<Uuid> = fx.withdraw.iter().map(|s| fid(s)).collect();
    let out = propagate(&nodes, &withdrawn);
    let names = Names::from(fx.certificates.iter().map(|c| &c.id));
    let mut got: Vec<String> = out.iter().map(|id| names.name(id)).collect();
    got.sort();
    let mut want = expect_list(&fx.expect, "retracted");
    want.sort();
    if got != want {
        tally.retraction_failures += 1;
        tally.invariant_failures.push(format!(
            "{}: retraction propagation mismatch (got {got:?}, want {want:?})",
            fx.id
        ));
        failures.push("retraction propagation".into());
    }
    // No surviving certificate may still rest on withdrawn evidence.
    for n in nodes.iter().filter(|n| !out.contains(&n.id)) {
        if n.inputs.iter().any(|i| withdrawn.contains(i)) {
            tally.retraction_failures += 1;
            tally.invariant_failures.push(format!(
                "{}: a retracted input still authorizes a conclusion",
                fx.id
            ));
        }
    }
    json!({"retracted": got})
}

fn run_drift(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let conv = |c: &FxClaim| ExtractedClaim {
        id: fid(&c.id),
        anchor: c.anchor.clone(),
        subject: c.subject.clone(),
        statement: c.statement.clone(),
        value: c.value.clone(),
        model_version: c.model.clone(),
    };
    let old: Vec<ExtractedClaim> = fx.old_claims.iter().map(conv).collect();
    let new: Vec<ExtractedClaim> = fx.new_claims.iter().map(conv).collect();
    let r = compare(&old, &new);
    tally.model_disagreements += r.disagreements();
    for item in &r.items {
        tally.count(&item.certificate);
        // Every changed claim names both versions.
        if item.high_impact
            && (item.certificate.scope.get("old_model").is_none()
                || item.certificate.scope.get("new_model").is_none())
        {
            tally.invariant_failures.push(format!(
                "{}: a changed conclusion hides its model version",
                fx.id
            ));
        }
        if item.high_impact && item.certificate.decision == Decision::AutoApplied {
            tally.false_auto += 1;
            tally.invariant_failures.push(format!(
                "{}: a model disagreement was applied automatically",
                fx.id
            ));
        }
    }
    if let Some(n) = fx.expect.get("disagreements").and_then(Value::as_u64) {
        if r.disagreements() as u64 != n {
            failures.push(format!("{} disagreements, want {n}", r.disagreements()));
        }
    }
    json!({
        "items": r.items.iter().map(|i| json!({
            "anchor": i.anchor, "change": i.change, "fields": i.fields,
            "old_model": i.old.as_ref().map(|c| c.model_version.clone()),
            "new_model": i.new.as_ref().map(|c| c.model_version.clone()),
        })).collect::<Vec<_>>(),
    })
}

/// Batch vs randomized vs incremental arrival, with re-ingestion.
fn run_ingestion_order(fx: &Fixture, tally: &mut Tally, failures: &mut Vec<String>) -> Value {
    let records = entity_records(fx);
    let pairs = entity_pairs(fx);
    let cannot = cannot(fx);
    let config = identity_config(fx);
    let names = Names::from(fx.entities.iter().map(|e| &e.id));
    let batch = identity::partition(&identity::simulate_apply(
        &records,
        &identity::plan(&records, &pairs, &cannot, &config),
    ));
    check_pairwise(&fx.id, &batch, &pairs, &cannot, tally);
    let mut rng = SplitMix::new(fx.seed.unwrap_or(42));
    let runs = fx.permutations.unwrap_or(24);
    let mut entity_ok = true;
    for run in 0..runs {
        let mut order = records.clone();
        rng.shuffle(&mut order);
        // Arrive in random batches; each pass plans over everything so far
        // and applies to the current state; some records are re-ingested.
        let mut state: Vec<EntityRecord> = Vec::new();
        let mut i = 0;
        while i < order.len() {
            let take = 1 + rng.below(3);
            for r in order.iter().skip(i).take(take) {
                if !state.iter().any(|s| s.id == r.id) {
                    state.push(r.clone());
                }
            }
            i += take;
            let known: BTreeSet<Uuid> = state.iter().map(|r| r.id).collect();
            let visible: Vec<PairEvidence> = pairs
                .iter()
                .filter(|p| known.contains(&p.a) && known.contains(&p.b))
                .cloned()
                .collect();
            let plan = identity::plan(&state, &visible, &cannot, &config);
            state = identity::simulate_apply(&state, &plan);
            // Re-running a pass over data already seen (re-ingestion of the
            // same artifacts adds no records) must change nothing.
            if run % 2 == 0 {
                let plan = identity::plan(&state, &visible, &cannot, &config);
                state = identity::simulate_apply(&state, &plan);
            }
        }
        // Idempotence: one more pass changes nothing.
        let plan = identity::plan(&state, &pairs, &cannot, &config);
        let settled = identity::simulate_apply(&state, &plan);
        if identity::partition(&settled) != identity::partition(&state) {
            tally
                .invariant_failures
                .push(format!("{}: a repeated pass changed the result", fx.id));
            entity_ok = false;
        }
        if names.groups(&identity::partition(&state)) != names.groups(&batch) {
            tally.invariant_failures.push(format!(
                "{}: incremental/permuted ingestion changed the entity partition",
                fx.id
            ));
            entity_ok = false;
            break;
        }
    }
    if !entity_ok {
        failures.push("entity partition not order-invariant".into());
    }

    let photos = photo_inputs(fx);
    let d = fx.max_distance.unwrap_or(6);
    let photo_names = Names::from(fx.photos.iter().map(|p| &p.id));
    let photo_batch = photo_names.groups(&photo::plan(&photos, d, &cannot, 3).group_sets());
    for _ in 0..runs {
        let mut p = photos.clone();
        rng.shuffle(&mut p);
        // Re-ingesting a photo is a no-op (same artifact), so duplicates are
        // dropped before planning, as content-hash dedup does.
        let dup = p.get(rng.below(p.len().max(1))).cloned();
        p.extend(dup);
        let mut seen = BTreeSet::new();
        p.retain(|x| seen.insert(x.id));
        if photo_names.groups(&photo::plan(&p, d, &cannot, 3).group_sets()) != photo_batch {
            tally
                .invariant_failures
                .push(format!("{}: photo groups changed with order", fx.id));
            failures.push("photo groups not order-invariant".into());
            break;
        }
    }

    // Contradictions: evaluate every check pair in both orders.
    let mut claims = Value::Null;
    if !fx.checks.is_empty() {
        let mut swapped = fx.clone();
        for c in &mut swapped.checks {
            c.pair.swap(0, 1);
        }
        let mut f1 = Vec::new();
        let mut f2 = Vec::new();
        let a = run_claims(fx, &mut Tally::default(), &mut f1);
        let b = run_claims(&swapped, &mut Tally::default(), &mut f2);
        let outcomes = |v: &Value| -> Vec<(String, Value)> {
            v["checks"]
                .as_array()
                .map(|x| {
                    x.iter()
                        .map(|c| (c["outcome"].to_string(), c["reasons"].clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        if outcomes(&a) != outcomes(&b) {
            tally.invariant_failures.push(format!(
                "{}: contradiction outcome depends on pair order",
                fx.id
            ));
            failures.push("contradictions not order-invariant".into());
        }
        failures.extend(f1);
        claims = a;
    }
    json!({
        "entity_partition": names.groups(&batch),
        "photo_groups": photo_batch,
        "claims": claims,
        "runs": runs,
    })
}

pub fn run_fixture(fx: &Fixture, tally_out: &mut Report) -> ScenarioResult {
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    let outcome = match fx.category.as_str() {
        "identity" => run_identity(fx, &mut tally, &mut failures),
        "photo" => run_photo(fx, &mut tally, &mut failures),
        "contradiction" | "temporal" => run_claims(fx, &mut tally, &mut failures),
        "provenance" => run_provenance(fx, &mut tally, &mut failures),
        "modality" => run_modality(fx, &mut tally, &mut failures),
        "retraction" => run_retraction(fx, &mut tally, &mut failures),
        "drift" => run_drift(fx, &mut tally, &mut failures),
        "ingestion_order" => run_ingestion_order(fx, &mut tally, &mut failures),
        other => {
            failures.push(format!("unknown category '{other}'"));
            Value::Null
        }
    };
    tally_out
        .invariant_failures
        .extend(tally.invariant_failures.iter().cloned());
    tally_out.false_auto_actions += tally.false_auto;
    tally_out.retraction_propagation_failures += tally.retraction_failures;
    tally_out.missing_certificates += tally.missing_certificates;
    tally_out.model_disagreements += tally.model_disagreements;
    for (k, v) in tally.blocked {
        *tally_out.blocked_by_reason.entry(k).or_default() += v;
    }
    for (k, v) in tally.review {
        *tally_out.review_by_reason.entry(k).or_default() += v;
    }
    let passed = failures.is_empty() && tally.invariant_failures.is_empty();
    ScenarioResult {
        id: fx.id.clone(),
        category: fx.category.clone(),
        passed,
        failures,
        digest: digest_of(&outcome),
        outcome,
    }
}

/// Run the whole corpus.
pub fn run(fixtures: &[Fixture]) -> Report {
    let started = Instant::now();
    let mut report = Report::default();
    let mut sorted: Vec<&Fixture> = fixtures.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    for fx in sorted {
        let r = run_fixture(fx, &mut report);
        report.scenarios.push(r);
    }
    report.total_scenarios = report.scenarios.len();
    report.passed = report.scenarios.iter().filter(|s| s.passed).count();
    report.failed = report.total_scenarios - report.passed;
    let all: Vec<&str> = report.scenarios.iter().map(|s| s.digest.as_str()).collect();
    report.digest = hex::encode(Sha256::digest(all.join("\n").as_bytes()));
    report.runtime_ms = started.elapsed().as_millis();
    // Every reason code the corpus reports must be a known one.
    for k in report
        .blocked_by_reason
        .keys()
        .chain(report.review_by_reason.keys())
    {
        if ReasonCode::parse(k).is_none() {
            report
                .invariant_failures
                .push(format!("unknown reason code {k}"));
        }
    }
    report
}

/// Human-readable summary.
pub fn render(report: &Report) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "semantic safety: {}/{} scenarios passed, {} invariant failures, {} false auto actions\n",
        report.passed,
        report.total_scenarios,
        report.invariant_failures.len(),
        report.false_auto_actions
    ));
    s.push_str(&format!(
        "missing certificates: {}, retraction failures: {}, model disagreements: {}, runtime: {} ms\n",
        report.missing_certificates,
        report.retraction_propagation_failures,
        report.model_disagreements,
        report.runtime_ms
    ));
    s.push_str(&format!(
        "blocked by reason: {:?}\n",
        report.blocked_by_reason
    ));
    s.push_str(&format!(
        "review by reason:  {:?}\n",
        report.review_by_reason
    ));
    for sc in &report.scenarios {
        s.push_str(&format!(
            "  [{}] {:<40} {}\n",
            if sc.passed { "ok" } else { "FAIL" },
            sc.id,
            sc.category
        ));
        for f in &sc.failures {
            s.push_str(&format!("        - {f}\n"));
        }
    }
    for f in &report.invariant_failures {
        s.push_str(&format!("  invariant: {f}\n"));
    }
    s.push_str(&format!("digest: {}\n", report.digest));
    s
}
