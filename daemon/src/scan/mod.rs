//! Contradiction scanner (write-up §6.1).
//!
//! Incremental: each active atomic unit is scanned once (cursor column
//! `contradiction_scanned_at`, migration 0003). Candidates are blocked by
//! shared subject entity and — when embeddings exist — pgvector similarity,
//! then scored by the pure rules in score.rs; pairs at or above the
//! threshold land in `contradictions` with a `detected` audit row. Detection
//! is uniform across source modalities because units are modality-blind.

pub mod score;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use pgvector::Vector;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::config::Config;
use crate::extract::ollama::OllamaClient;
use crate::safety::contradiction::{evaluate, Claim, ContradictionContext};
use crate::safety::temporal::{TemporalPolicy, TimeScope};
use crate::safety::{explained, store, InferenceDecision};
use score::{score_pair, UnitFacts};

/// A unit as the scanner sees it: the scorer's facts plus time and model.
struct Scanned {
    facts: UnitFacts,
    time: TimeScope,
    model: Option<String>,
}

#[derive(Debug, Default)]
struct Outcomes {
    blocked: usize,
    review: usize,
    superseded: usize,
}

#[derive(Debug, Default)]
pub struct ScanStats {
    pub units_scanned: usize,
    pub pairs_scored: usize,
    pub contradictions_found: usize,
    /// Flagged pairs the safety rule found compatible (not reported).
    pub pairs_blocked: usize,
    /// Reported contradictions whose alignment is incomplete (need review).
    pub needs_review: usize,
    /// Older claims superseded by a later statement of current state.
    pub superseded: usize,
}

/// Long-running scanner entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    // The judge needs a chat model; embeddings alone don't judge.
    let ollama = OllamaClient::from_config(&config)
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "scanner: Ollama misconfigured; judging disabled");
            None
        })
        .filter(|client| client.model.is_some());

    let mut interval = tokio::time::interval(Duration::from_secs(config.scan_interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match run_one_scan(&pool, &config, ollama.as_ref()).await {
            Ok(stats) if stats.units_scanned > 0 => {
                tracing::info!(
                    units = stats.units_scanned,
                    pairs = stats.pairs_scored,
                    found = stats.contradictions_found,
                    "contradiction scan complete"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "contradiction scan failed"),
        }
    }
}

/// One scan pass over a batch of unscanned units. Public for tests.
pub async fn run_one_scan(
    pool: &PgPool,
    config: &Config,
    ollama: Option<&OllamaClient>,
) -> anyhow::Result<ScanStats> {
    let started = Instant::now();
    let mut stats = ScanStats::default();

    let pending: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT id FROM atomic_units
        WHERE contradiction_scanned_at IS NULL AND status = 'active'
        ORDER BY created_at
        LIMIT $1
        "#,
    )
    .bind(config.scan_batch)
    .fetch_all(pool)
    .await?;

    let policy = TemporalPolicy {
        same_moment: chrono::Duration::hours(config.safety_same_moment_hours),
        min_succession: chrono::Duration::days(config.safety_succession_days),
    };
    for unit_id in pending {
        let Some(unit) = load_unit(pool, unit_id).await? else {
            continue; // deleted or superseded since the id was listed
        };
        let candidates = load_candidates(pool, &unit.facts, config).await?;

        let mut conflicts: Vec<(Scanned, score::Conflict)> = Vec::new();
        for (candidate, cosine_sim) in candidates {
            stats.pairs_scored += 1;
            let Some(mut conflict) = score_pair(&unit.facts, &candidate.facts, cosine_sim) else {
                continue;
            };
            // Optional local judge, only on structurally flagged pairs.
            if let Some(client) = ollama {
                match client
                    .judge(&unit.facts.statement, &candidate.facts.statement)
                    .await
                {
                    Ok(judgement) => {
                        if judgement.contradicts {
                            conflict.score =
                                (0.5 * conflict.score + 0.5 * judgement.confidence).clamp(0.0, 1.0);
                        } else {
                            conflict.score *= 0.4;
                        }
                        if !judgement.why.is_empty() {
                            conflict.explanation =
                                format!("{} — judge: {}", conflict.explanation, judgement.why);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ollama judge failed; structural score kept")
                    }
                }
            }
            if conflict.score >= config.scan_threshold {
                conflicts.push((candidate, conflict));
            }
        }

        // Claim, persist findings, and stamp the cursor in one transaction.
        // The FOR UPDATE SKIP LOCKED claim makes each unit scanned exactly once
        // even if more than one scanner instance runs: whichever transaction
        // locks the row first stamps it, and the others find it already
        // scanned/locked and skip. Findings are idempotent regardless (ON
        // CONFLICT below), and a crash before commit leaves the row unscanned
        // for a later pass — so the marker never gets set without the work.
        let mut tx = pool.begin().await?;
        // Scanners write the same pairs (certificate, contradiction,
        // supersession) from either side and in any order, which can
        // deadlock two of them. The write phase is short, so concurrent
        // scanners take turns here; scoring above still runs in parallel.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gather.scan.write'))")
            .execute(&mut *tx)
            .await?;
        let claimed: Option<(Uuid,)> = sqlx::query_as(
            r#"
            SELECT id FROM atomic_units
            WHERE id = $1 AND contradiction_scanned_at IS NULL AND status = 'active'
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(unit.facts.id)
        .fetch_optional(&mut *tx)
        .await?;
        if claimed.is_none() {
            continue; // another scanner claimed it, or it was scanned/retired since listing
        }
        let mut outcomes = Outcomes::default();
        for (other, conflict) in &conflicts {
            if record_conflict(&mut tx, &unit, other, conflict, &policy, &mut outcomes).await? {
                stats.contradictions_found += 1;
                metrics::counter!(
                    "gather_contradictions_detected_total",
                    "method" => conflict.method
                )
                .increment(1);
            }
        }
        stats.pairs_blocked += outcomes.blocked;
        stats.needs_review += outcomes.review;
        stats.superseded += outcomes.superseded;
        sqlx::query("UPDATE atomic_units SET contradiction_scanned_at = now() WHERE id = $1")
            .bind(unit.facts.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        stats.units_scanned += 1;
    }

    metrics::histogram!("gather_scan_duration_seconds").record(started.elapsed().as_secs_f64());
    Ok(stats)
}

fn scanned_from(row: &sqlx::postgres::PgRow) -> Scanned {
    Scanned {
        facts: UnitFacts {
            id: row.get("id"),
            statement: row.get("statement"),
            attrs: row.get("attrs"),
            subject_entity_id: row.get("subject_entity_id"),
            valid_from: row.get("valid_from"),
            valid_to: row.get("valid_to"),
            assignments: vec![],
        },
        time: TimeScope {
            asserted_at: row.get("asserted_at"),
            observed_at: row.get("observed_at"),
            valid_from: row.get("valid_from"),
            valid_to: row.get("valid_to"),
            ingested_at: row.get("created_at"),
        },
        model: row.get("model"),
    }
}

async fn load_unit(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<Scanned>> {
    let Some(row) = sqlx::query(
        r#"
        SELECT id, statement, attrs, subject_entity_id, valid_from, valid_to,
               asserted_at, observed_at, created_at,
               coalesce(extraction_model, extraction_method::text) AS model
        FROM atomic_units WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let mut unit = scanned_from(&row);
    unit.facts.assignments = load_assignments(pool, id).await?;
    Ok(Some(unit))
}

async fn load_assignments(
    pool: &PgPool,
    unit_id: Uuid,
) -> anyhow::Result<Vec<(Uuid, String, Uuid)>> {
    Ok(sqlx::query(
        r#"
        SELECT source_entity_id, relation_type, target_entity_id
        FROM relationships WHERE atomic_unit_id = $1 AND status = 'active'
        "#,
    )
    .bind(unit_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.get("source_entity_id"),
            r.get("relation_type"),
            r.get("target_entity_id"),
        )
    })
    .collect())
}

/// Candidate blocking: shared subject entity ∪ embedding neighbors.
/// Returns each candidate plus the cosine similarity when known.
async fn load_candidates(
    pool: &PgPool,
    unit: &UnitFacts,
    config: &Config,
) -> anyhow::Result<Vec<(Scanned, Option<f32>)>> {
    let mut rows = Vec::new();

    if let Some(subject) = unit.subject_entity_id {
        rows.extend(
            sqlx::query(
                r#"
                SELECT c.id, c.statement, c.attrs, c.subject_entity_id,
                       c.valid_from, c.valid_to, c.asserted_at, c.observed_at, c.created_at,
                       coalesce(c.extraction_model, c.extraction_method::text) AS model,
                       CASE WHEN c.embedding IS NOT NULL AND u.embedding IS NOT NULL
                            THEN (1 - (u.embedding <=> c.embedding))::float4 END AS cosine_sim
                FROM atomic_units c
                JOIN atomic_units u ON u.id = $1
                WHERE c.subject_entity_id = $2 AND c.id <> $1 AND c.status = 'active'
                ORDER BY c.created_at DESC, c.id
                LIMIT $3
                "#,
            )
            .bind(unit.id)
            .bind(subject)
            .bind(config.scan_max_candidates)
            .fetch_all(pool)
            .await?,
        );
    }

    let embedding: Option<Vector> =
        sqlx::query_scalar("SELECT embedding FROM atomic_units WHERE id = $1")
            .bind(unit.id)
            .fetch_one(pool)
            .await?;
    if let Some(embedding) = embedding {
        rows.extend(
            sqlx::query(
                r#"
                SELECT c.id, c.statement, c.attrs, c.subject_entity_id,
                       c.valid_from, c.valid_to, c.asserted_at, c.observed_at, c.created_at,
                       coalesce(c.extraction_model, c.extraction_method::text) AS model,
                       (1 - (c.embedding <=> $2))::float4 AS cosine_sim
                FROM atomic_units c
                WHERE c.embedding IS NOT NULL AND c.id <> $1 AND c.status = 'active'
                  AND (c.embedding <=> $2) < 0.35
                ORDER BY c.embedding <=> $2, c.id
                LIMIT $3
                "#,
            )
            .bind(unit.id)
            .bind(embedding)
            .bind(config.scan_max_candidates)
            .fetch_all(pool)
            .await?,
        );
    }

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let id: Uuid = row.get("id");
        if !seen.insert(id) {
            continue;
        }
        let mut c = scanned_from(&row);
        c.facts.assignments = load_assignments(pool, id).await?;
        out.push((c, row.get::<Option<f32>, _>("cosine_sim")));
    }
    Ok(out)
}

/// Live source artifacts behind a unit.
async fn unit_artifacts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    unit: Uuid,
) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT p.artifact_id FROM atomic_unit_provenance p \
         JOIN artifacts a ON a.id = p.artifact_id AND a.retracted_at IS NULL \
         WHERE p.atomic_unit_id = $1 ORDER BY p.artifact_id LIMIT 50",
    )
    .bind(unit)
    .fetch_all(&mut **tx)
    .await?)
}

/// child -> parents over containment edges reachable from `roots`.
async fn containment(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    roots: &[Uuid],
) -> anyhow::Result<BTreeMap<Uuid, Vec<Uuid>>> {
    let edges: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "WITH RECURSIVE up(child, parent, depth) AS ( \
             SELECT source_entity_id, target_entity_id, 1 FROM relationships \
             WHERE source_entity_id = ANY($1) AND status = 'active' \
               AND relation_type IN ('located_in', 'part_of', 'in', 'inside') \
           UNION \
             SELECT r.source_entity_id, r.target_entity_id, u.depth + 1 \
             FROM relationships r JOIN up u ON r.source_entity_id = u.parent \
             WHERE r.status = 'active' AND u.depth < 6 \
               AND r.relation_type IN ('located_in', 'part_of', 'in', 'inside') \
         ) SELECT DISTINCT child, parent FROM up",
    )
    .bind(roots)
    .fetch_all(&mut **tx)
    .await?;
    let mut map: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for (c, p) in edges {
        map.entry(c).or_default().push(p);
    }
    Ok(map)
}

/// Run the safety rule on one flagged pair and act on its decision: report
/// the contradiction (aligned, or flagged for review), or record why it is
/// not one; apply a supersession when a later state replaces an earlier one.
/// Returns whether a new contradiction was stored.
async fn record_conflict(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    unit: &Scanned,
    other: &Scanned,
    conflict: &score::Conflict,
    policy: &TemporalPolicy,
    outcomes: &mut Outcomes,
) -> anyhow::Result<bool> {
    let (a, b) = if unit.facts.id < other.facts.id {
        (unit, other)
    } else {
        (other, unit)
    };
    let existing: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT id, status::text, resolved_by FROM contradictions \
         WHERE unit_a_id = $1 AND unit_b_id = $2",
    )
    .bind(a.facts.id)
    .bind(b.facts.id)
    .fetch_optional(&mut **tx)
    .await?;
    // A person's verdict on a pair Gather explained away is final: a
    // confirmed conflict is never explained away again (only brought back
    // if the safety layer withdrew it), and "not a conflict" blocks.
    let verdict = explained::verdict(tx, a.facts.id, b.facts.id).await?;
    if verdict.as_deref() == Some(explained::CONFIRMED) {
        let reopened = sqlx::query_scalar::<_, Uuid>(
            "UPDATE contradictions SET status = 'open', resolved_at = NULL, resolved_by = NULL, \
               resolution_note = NULL \
             WHERE unit_a_id = $1 AND unit_b_id = $2 AND status = 'dismissed' \
               AND resolved_by = 'safety' RETURNING id",
        )
        .bind(a.facts.id)
        .bind(b.facts.id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(id) = reopened {
            sqlx::query(
                "INSERT INTO contradiction_audit \
                   (contradiction_id, action, actor, from_status, to_status, note) \
                 VALUES ($1, 'reopen', 'scanner', 'dismissed', 'open', $2)",
            )
            .bind(id)
            .bind("the claim it rested on was restored")
            .execute(&mut **tx)
            .await?;
        }
        return Ok(reopened.is_some());
    }
    let user_rejected = matches!(
        &existing,
        Some((_, status, by)) if (status == "dismissed" || status == "both_valid")
            && by.as_deref() != Some("safety")
            && by.as_deref() != Some(explained::AGREED_BY)
    );
    let targets: Vec<Uuid> = a
        .facts
        .assignments
        .iter()
        .chain(&b.facts.assignments)
        .map(|x| x.2)
        .collect();
    let ctx = ContradictionContext {
        part_of: if conflict.method == "rule:exclusive-assignment" {
            containment(tx, &targets).await?
        } else {
            BTreeMap::new()
        },
        user_rejected,
        user_agreed_compatible: verdict.as_deref() == Some(explained::NOT_CONFLICT),
        policy: *policy,
        sources_a: unit_artifacts(tx, a.facts.id).await?,
        sources_b: unit_artifacts(tx, b.facts.id).await?,
        model_version: match (&a.model, &b.model) {
            (Some(x), Some(y)) if x == y => Some(x.clone()),
            (Some(x), Some(y)) => Some(format!("{x} | {y}")),
            (x, y) => x.clone().or(y.clone()),
        },
    };
    let eval = evaluate(
        Claim {
            facts: &a.facts,
            time: &a.time,
        },
        Claim {
            facts: &b.facts,
            time: &b.time,
        },
        conflict,
        &ctx,
    );

    if let Some(sup) = &eval.supersession {
        let previous_valid_to: Option<Option<chrono::DateTime<chrono::Utc>>> = sqlx::query_scalar(
            "UPDATE atomic_units o SET status = 'superseded', superseded_by_unit_id = $2, \
               valid_to = coalesce(o.valid_to, GREATEST($3, o.valid_from)) \
             FROM (SELECT valid_to FROM atomic_units WHERE id = $1) prev \
             WHERE o.id = $1 AND o.status = 'active' RETURNING prev.valid_to",
        )
        .bind(sup.older)
        .bind(sup.newer)
        .bind(sup.boundary)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(previous) = previous_valid_to {
            sqlx::query(
                "UPDATE relationships SET status = 'superseded' \
                 WHERE atomic_unit_id = $1 AND status = 'active'",
            )
            .bind(sup.older)
            .execute(&mut **tx)
            .await?;
            let mut cert = sup.certificate.clone();
            cert.conclusion_id = Some(sup.older);
            cert.scope = json!({
                "older": sup.older,
                "newer": sup.newer,
                "previous_valid_to": previous,
            });
            store::record(tx, &cert).await?;
            outcomes.superseded += 1;
        }
    }

    let alignment = serde_json::to_value(&eval.alignment)?;
    match &eval.decision {
        InferenceDecision::Blocked(cert) => {
            store::record(tx, cert).await?;
            outcomes.blocked += 1;
            Ok(false)
        }
        InferenceDecision::AutoApply(cert) | InferenceDecision::NeedsReview(cert) => {
            let certainty = if eval.decision.is_auto() {
                "aligned"
            } else {
                outcomes.review += 1;
                "needs_review"
            };
            let cert_id = store::record(tx, cert).await?;
            let inserted: Option<(Uuid,)> = sqlx::query_as(
                r#"
                INSERT INTO contradictions
                    (unit_a_id, unit_b_id, score, detection_method, explanation,
                     certificate_id, alignment, certainty)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (unit_a_id, unit_b_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(a.facts.id)
            .bind(b.facts.id)
            .bind(conflict.score)
            .bind(conflict.method)
            .bind(&conflict.explanation)
            .bind(cert_id)
            .bind(&alignment)
            .bind(certainty)
            .fetch_optional(&mut **tx)
            .await?;
            match inserted {
                Some((contradiction_id,)) => {
                    store::set_conclusion(tx, cert_id, contradiction_id).await?;
                    sqlx::query(
                        r#"
                        INSERT INTO contradiction_audit
                            (contradiction_id, action, actor, to_status, note)
                        VALUES ($1, 'detected', 'scanner', 'open', $2)
                        "#,
                    )
                    .bind(contradiction_id)
                    .bind(&conflict.explanation)
                    .execute(&mut **tx)
                    .await?;
                    Ok(true)
                }
                None => {
                    let Some((id, status, by)) = existing else {
                        return Ok(false);
                    };
                    store::set_conclusion(tx, cert_id, id).await?;
                    // A contradiction the safety layer withdrew (its claim was
                    // rejected, then restored) comes back once it holds again;
                    // one a person resolved stays resolved.
                    let reopen = status == "dismissed" && by.as_deref() == Some("safety");
                    let updated = sqlx::query(
                        "UPDATE contradictions SET certificate_id = $2, alignment = $3, \
                           certainty = $4, status = 'open', resolved_at = NULL, \
                           resolved_by = NULL, resolution_note = NULL \
                         WHERE id = $1 AND (status = 'open' \
                           OR (status = 'dismissed' AND resolved_by = 'safety'))",
                    )
                    .bind(id)
                    .bind(cert_id)
                    .bind(&alignment)
                    .bind(certainty)
                    .execute(&mut **tx)
                    .await?
                    .rows_affected();
                    if reopen && updated > 0 {
                        sqlx::query(
                            "INSERT INTO contradiction_audit \
                               (contradiction_id, action, actor, from_status, to_status, note) \
                             VALUES ($1, 'reopen', 'scanner', 'dismissed', 'open', $2)",
                        )
                        .bind(id)
                        .bind("the claim it rested on was restored")
                        .execute(&mut **tx)
                        .await?;
                        return Ok(true);
                    }
                    Ok(false)
                }
            }
        }
    }
}
