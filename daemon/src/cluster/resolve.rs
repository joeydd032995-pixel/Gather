//! Entity resolution pass: plan with `safety::identity`, then make the
//! database match the plan.
//!
//! Every pass plans over *base records* — live entities and the ones merged
//! into them — so the result depends only on the records and evidence, not
//! on the order they arrived: a merge made when two names looked alike is
//! withdrawn if a later name turns the pair into a chain. Merges a person
//! made or accepted are fixed. Each merge, parked pair and blocked pair
//! leaves a certificate.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::pair_key;
use crate::config::Config;
use crate::decide::live::LiveThresholds;
use crate::decide::{best_score, MergeSignals};
use crate::entities::merge::unmerge_for_supersession_in;
use crate::entities::merge_entities_in;
use crate::entities::similarity::{
    features_similarity, name_similarity, names_comparable, NameFeatures,
};
use crate::safety::certificate::{Decision, InferenceCertificate, Predicate};
use crate::safety::identity::{
    choose_survivor, plan, EntityRecord, IdentityConfig, IdentityPlan, Link, PairEvidence,
};
use crate::safety::{store, Outcome, ReasonCode};

/// How many live entities one pass considers (pairwise work is its square).
const LIVE_CAP: i64 = 2_000;
/// Source artifacts kept per entity for provenance (a deterministic subset).
const SOURCES_PER_ENTITY: i32 = 20;

#[derive(Debug, Default)]
pub struct ResolveStats {
    pub merged: usize,
    pub parked: usize,
    pub withdrawn: usize,
}

async fn load_records(pool: &PgPool) -> anyhow::Result<Vec<EntityRecord>> {
    let rows = sqlx::query(
        "WITH live AS (SELECT id FROM entities WHERE merged_into_entity_id IS NULL \
                       ORDER BY name, id LIMIT $1) \
         SELECT e.id, e.name, e.kind::text AS kind, e.metadata, \
                coalesce(e.merged_into_entity_id, e.id) AS head, \
                (SELECT a.actor FROM entity_merge_audit a \
                 WHERE a.loser_entity_id = e.id AND a.action = 'merge' AND a.undone_at IS NULL \
                 ORDER BY a.created_at DESC LIMIT 1) AS merged_by \
         FROM entities e \
         WHERE e.id IN (SELECT id FROM live) OR e.merged_into_entity_id IN (SELECT id FROM live)",
    )
    .bind(LIVE_CAP)
    .fetch_all(pool)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
    let sources: HashMap<Uuid, Vec<Uuid>> = sqlx::query_as::<_, (Uuid, Vec<Uuid>)>(
        "SELECT x.entity_id, \
                (array_agg(DISTINCT p.artifact_id ORDER BY p.artifact_id))[1:$2] \
         FROM ( \
           SELECT subject_entity_id AS entity_id, id AS unit_id FROM atomic_units \
           WHERE subject_entity_id = ANY($1) AND status <> 'retracted' \
           UNION \
           SELECT source_entity_id, atomic_unit_id FROM relationships \
           WHERE source_entity_id = ANY($1) AND atomic_unit_id IS NOT NULL AND status = 'active' \
           UNION \
           SELECT target_entity_id, atomic_unit_id FROM relationships \
           WHERE target_entity_id = ANY($1) AND atomic_unit_id IS NOT NULL AND status = 'active' \
         ) x \
         JOIN atomic_unit_provenance p ON p.atomic_unit_id = x.unit_id \
         JOIN artifacts a ON a.id = p.artifact_id AND a.retracted_at IS NULL \
         GROUP BY x.entity_id",
    )
    .bind(&ids)
    .bind(SOURCES_PER_ENTITY)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    Ok(rows
        .iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            let head: Uuid = r.get("head");
            let merged_by: Option<String> = r.get("merged_by");
            let metadata: Value = r.get("metadata");
            let context: BTreeMap<String, String> = metadata
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| match v {
                            Value::String(s) => Some((k.clone(), s.clone())),
                            Value::Number(n) => Some((k.clone(), n.to_string())),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            EntityRecord {
                id,
                name: r.get("name"),
                kind: r.get("kind"),
                context,
                head,
                link: if head == id {
                    Link::Root
                } else if merged_by.as_deref() == Some("auto") {
                    Link::Auto
                } else {
                    Link::User
                },
                sources: sources.get(&id).cloned().unwrap_or_default(),
            }
        })
        .collect())
}

async fn load_evidence(
    pool: &PgPool,
    records: &[EntityRecord],
    threshold: f32,
) -> anyhow::Result<Vec<PairEvidence>> {
    let mut out = Vec::new();
    let features: Vec<NameFeatures> = records.iter().map(|r| NameFeatures::new(&r.name)).collect();
    for (i, a) in records.iter().enumerate() {
        for (j, b) in records.iter().enumerate().skip(i + 1) {
            if !names_comparable(&a.name, &b.name) {
                continue;
            }
            let text = features_similarity(&features[i], &features[j]);
            if text >= threshold {
                out.push(PairEvidence {
                    a: a.id,
                    b: b.id,
                    signals: MergeSignals {
                        cosine: None,
                        text: Some(text),
                    },
                    method: "rule:name-similarity".into(),
                });
            }
        }
    }
    let ids: Vec<Uuid> = records.iter().map(|r| r.id).collect();
    let names: HashMap<Uuid, &str> = records.iter().map(|r| (r.id, r.name.as_str())).collect();
    // The name gate runs here rather than in SQL, so page through the closest
    // pairs until the budget is filled with *comparable* ones: noise ranked
    // above real duplicates must not use it up.
    const PAGE: i64 = 5_000;
    const SCAN_CAP: i64 = 50_000;
    let max_distance = (1.0 - f64::from(threshold)).clamp(0.0, 1.0);
    let mut kept = 0usize;
    let mut offset = 0i64;
    loop {
        let page: Vec<(Uuid, Uuid, f32)> = sqlx::query_as(
            "WITH e AS MATERIALIZED ( \
                 SELECT id, embedding FROM entities \
                 WHERE id = ANY($1) AND embedding IS NOT NULL \
             ) \
             SELECT a.id, b.id, (1 - (a.embedding <=> b.embedding))::float4 \
             FROM e a JOIN e b ON a.id < b.id \
             WHERE (a.embedding <=> b.embedding) <= $2 \
             ORDER BY a.embedding <=> b.embedding, a.id, b.id LIMIT $3 OFFSET $4",
        )
        .bind(&ids)
        .bind(max_distance)
        .bind(PAGE)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        let fetched = page.len() as i64;
        for (a, b, c) in page {
            if c < threshold || !names_comparable(names[&a], names[&b]) {
                continue;
            }
            kept += 1;
            out.push(PairEvidence {
                a,
                b,
                signals: MergeSignals {
                    cosine: Some(c),
                    text: Some(name_similarity(names[&a], names[&b])),
                },
                method: "embedding:cosine".into(),
            });
        }
        offset += fetched;
        if fetched < PAGE || kept >= PAGE as usize || offset >= SCAN_CAP {
            break;
        }
    }
    Ok(out)
}

async fn load_cannot_links(pool: &PgPool) -> anyhow::Result<Vec<(Uuid, Uuid)>> {
    Ok(sqlx::query_as(
        "SELECT winner_entity_id, loser_entity_id FROM entity_merge_audit WHERE action = 'dismiss'",
    )
    .fetch_all(pool)
    .await?)
}

/// Park an item in the tray and write its certificate.
async fn park(
    pool: &PgPool,
    target: Uuid,
    reason: &str,
    signals: Value,
    cert: &InferenceCertificate,
) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    let cert_id = store::record(&mut tx, cert).await?;
    let mut signals = signals;
    signals["certificate"] = json!(cert_id);
    signals["reasons"] = json!(cert
        .reason_codes()
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>());
    let n = sqlx::query(
        "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
         VALUES ('entity', $1, $2, $3) \
         ON CONFLICT (target_kind, target_id) WHERE state = 'open' DO UPDATE \
           SET reason = EXCLUDED.reason, signals = EXCLUDED.signals \
           WHERE review_queue.reason IS DISTINCT FROM EXCLUDED.reason \
              OR review_queue.signals IS DISTINCT FROM EXCLUDED.signals",
    )
    .bind(target)
    .bind(reason)
    .bind(signals)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
}

fn pair_signals(a: Uuid, b: Uuid, s: &MergeSignals, method: &str, chained: bool) -> Value {
    json!({
        "a": a,
        "b": b,
        "score": best_score(s),
        "cosine": s.cosine,
        "text": s.text,
        "method": method,
        "chained": chained,
    })
}

/// Withdraw automatic merges that the plan no longer supports. Merges into
/// one survivor unwind newest-first, so reaching an older one means undoing
/// the newer ones too (they are re-merged below if still planned); a
/// person's merge is never undone, and anything behind it is reported.
async fn detach(
    pool: &PgPool,
    records: &[EntityRecord],
    plan: &IdentityPlan,
    stats: &mut ResolveStats,
) -> anyhow::Result<()> {
    let group_of: HashMap<Uuid, usize> = plan
        .merges
        .iter()
        .enumerate()
        .flat_map(|(i, m)| m.members.iter().map(move |&id| (id, i)))
        .collect();
    let mut must: BTreeMap<Uuid, BTreeSet<Uuid>> = BTreeMap::new();
    for r in records {
        let together = matches!(
            (group_of.get(&r.id), group_of.get(&r.head)),
            (Some(x), Some(y)) if x == y
        );
        if r.link == Link::Auto && !together {
            must.entry(r.head).or_default().insert(r.id);
        }
    }
    for (head, losers) in must {
        let merges: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT loser_entity_id, actor FROM entity_merge_audit \
             WHERE winner_entity_id = $1 AND action = 'merge' AND undone_at IS NULL \
             ORDER BY created_at DESC",
        )
        .bind(head)
        .fetch_all(pool)
        .await?;
        let mut remaining = losers.clone();
        for (loser, actor) in merges {
            if remaining.is_empty() {
                break;
            }
            if actor != "auto" {
                break;
            }
            let mut tx = pool.begin().await?;
            match unmerge_for_supersession_in(
                &mut tx,
                loser,
                "withdrawn: later evidence showed this merge was part of a chain".into(),
            )
            .await
            {
                Ok(_) => {
                    tx.commit().await?;
                    remaining.remove(&loser);
                    stats.withdrawn += 1;
                    metrics::counter!("gather_entity_unmerges_total").increment(1);
                }
                Err(e) => {
                    tx.rollback().await?;
                    tracing::warn!(entity = %loser, error = %e, "could not withdraw a merge");
                    break;
                }
            }
        }
        // Merges that could not be unwound automatically: say so.
        for loser in remaining {
            let cert = InferenceCertificate::new(
                crate::safety::ConclusionKind::EntityMerge,
                crate::safety::identity::RULE_AUTO_MERGE,
                format!("entity-withdraw:{head}:{loser}"),
            )
            .with_subjects([head, loser])
            .predicate(Predicate::fail(
                "withdrawable",
                ReasonCode::RetractionRequired,
                json!({"reason": "a later merge by you (or a newer one) sits on top of it"}),
            ))
            .decide(Decision::NeedsReview);
            stats.parked += park(
                pool,
                pair_key(head, loser),
                "withdraw-merge",
                json!({"a": head, "b": loser}),
                &cert,
            )
            .await? as usize;
        }
    }
    Ok(())
}

/// Close tray items that ask about a pair no longer worth asking about
/// (code-comment fragments, filler, opposites): ones parked before names were
/// checked would otherwise sit in the tray until someone dismissed each by
/// hand.
async fn dismiss_noise_pairs(pool: &PgPool) -> anyhow::Result<u64> {
    // Names come straight from `entities`: the pass itself only loads the
    // first LIVE_CAP, and a parked pair outside that window must still close.
    let open: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT q.id, a.name, b.name FROM review_queue q \
         JOIN entities a ON a.id::text = q.signals->>'a' \
         JOIN entities b ON b.id::text = q.signals->>'b' \
         WHERE q.state = 'open' AND q.target_kind = 'entity' AND q.reason = 'merge-band'",
    )
    .fetch_all(pool)
    .await?;
    let noise: Vec<Uuid> = open
        .into_iter()
        .filter(|(_, a, b)| !names_comparable(a, b))
        .map(|(id, _, _)| id)
        .collect();
    if noise.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query(
        "UPDATE review_queue SET state = 'dismissed', \
                signals = signals || '{\"dismissed_by\": \"name-gate\"}'::jsonb \
         WHERE id = ANY($1) AND state = 'open'",
    )
    .bind(&noise)
    .execute(pool)
    .await?
    .rows_affected();
    tracing::info!(
        dismissed = done,
        "closed tray items about pairs that are not names"
    );
    Ok(done)
}

pub async fn entity_resolution_pass(
    pool: &PgPool,
    config: &Config,
) -> anyhow::Result<ResolveStats> {
    let mut stats = ResolveStats::default();
    let records = load_records(pool).await?;
    if records.len() < 2 {
        return Ok(stats);
    }
    let evidence = load_evidence(pool, &records, config.cluster_threshold).await?;
    let cannot = load_cannot_links(pool).await?;
    let identity = IdentityConfig {
        thresholds: LiveThresholds::load(pool, config).await?.merge,
        max_component: config.cluster_max_component,
        hub_degree: config.safety_hub_degree,
    };
    let plan = plan(&records, &evidence, &cannot, &identity);

    detach(pool, &records, &plan, &mut stats).await?;

    // Merge each planned group under one survivor.
    let mut merged_away: Vec<Uuid> = Vec::new();
    for m in &plan.merges {
        let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
            "SELECT e.id, coalesce(e.merged_into_entity_id, e.id), h.name, h.kind::text \
             FROM entities e JOIN entities h ON h.id = coalesce(e.merged_into_entity_id, e.id) \
             WHERE e.id = ANY($1)",
        )
        .bind(&m.members)
        .fetch_all(pool)
        .await?;
        let heads: BTreeMap<Uuid, EntityRecord> = rows
            .iter()
            .map(|(_, h, name, kind)| {
                (
                    *h,
                    EntityRecord {
                        id: *h,
                        name: name.clone(),
                        kind: kind.clone(),
                        context: BTreeMap::new(),
                        head: *h,
                        link: Link::Root,
                        sources: vec![],
                    },
                )
            })
            .collect();
        let mut cert = m.certificate.clone();
        // A member still attached to a head outside the group (a withdrawal
        // that could not be done automatically) blocks the merge; that case
        // is already in the tray.
        if heads.keys().any(|h| !m.members.contains(h)) {
            continue;
        }
        if heads.len() < 2 {
            // Already one entity: keep its certificate current.
            let mut conn = pool.acquire().await?;
            store::record(&mut conn, &cert).await?;
            continue;
        }
        let survivor = choose_survivor(heads.values()).expect("heads");
        let names: Vec<String> = heads.values().map(|h| h.name.clone()).collect();
        let cluster_id: Uuid = sqlx::query_scalar(
            "INSERT INTO clusters (kind, label, cohesion, size) \
             VALUES ('entity', $1, $2, $3) RETURNING id",
        )
        .bind(&heads[&survivor].name)
        .bind(m.cohesion)
        .bind(m.members.len() as i32)
        .fetch_one(pool)
        .await?;
        cert.conclusion_id = Some(cluster_id);
        let mut tx = pool.begin().await?;
        for &member in &m.members {
            sqlx::query(
                "INSERT INTO cluster_members (cluster_id, member_kind, member_id, sim) \
                 VALUES ($1, 'entity', $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(cluster_id)
            .bind(member)
            .bind(m.cohesion)
            .execute(&mut *tx)
            .await?;
        }
        for &loser in heads.keys().filter(|&&h| h != survivor) {
            let basis = m.basis.get(&loser).copied();
            merge_entities_in(
                &mut tx,
                survivor,
                loser,
                Some(format!("auto-merged: {}", names.join(" / "))),
                Some("auto".to_string()),
                basis,
            )
            .await?;
            merged_away.push(loser);
            stats.merged += 1;
            metrics::counter!("gather_entity_merges_total").increment(1);
        }
        store::record(&mut tx, &cert).await?;
        tx.commit().await?;
    }

    for p in &plan.review_pairs {
        stats.parked += park(
            pool,
            pair_key(p.a, p.b),
            "merge-band",
            pair_signals(p.a, p.b, &p.signals, &p.method, p.chained),
            &p.certificate,
        )
        .await? as usize;
    }
    for c in &plan.review_components {
        stats.parked += park(
            pool,
            c.anchor,
            c.reason,
            json!({ "members": c.members }),
            &c.certificate,
        )
        .await? as usize;
    }
    if !plan.blocked.is_empty() {
        let mut tx = pool.begin().await?;
        for cert in &plan.blocked {
            store::record(&mut tx, cert).await?;
        }
        tx.commit().await?;
    }

    // Automatic merges that were planned last time and no longer are: their
    // certificates are superseded by this pass's review/blocked ones.
    let planned: BTreeSet<String> = plan
        .merges
        .iter()
        .map(|m| m.certificate.conclusion_key.clone())
        .collect();
    let ids: Vec<Uuid> = records.iter().map(|r| r.id).collect();
    let mut conn = pool.acquire().await?;
    let stale: Vec<Uuid> =
        store::live_for_subjects(&mut conn, "entity_merge", &ids, Some("auto_applied"))
            .await?
            .into_iter()
            .filter(|(_, key, _)| key.starts_with("entity-merge:") && !planned.contains(key))
            .map(|(id, _, _)| id)
            .collect();
    store::withdraw(
        &mut conn,
        &stale,
        Outcome::Superseded,
        "later evidence no longer supports this merge",
        None,
    )
    .await?;

    dismiss_noise_pairs(pool).await?;

    // A pair held earlier may have just been merged: close stale tray items.
    if !merged_away.is_empty() {
        sqlx::query(
            "UPDATE review_queue SET state = 'dismissed' \
             WHERE state = 'open' AND target_kind = 'entity' AND reason = 'merge-band' \
               AND (signals->>'a' = ANY($1) OR signals->>'b' = ANY($1))",
        )
        .bind(merged_away.iter().map(Uuid::to_string).collect::<Vec<_>>())
        .execute(pool)
        .await?;
    }
    Ok(stats)
}
