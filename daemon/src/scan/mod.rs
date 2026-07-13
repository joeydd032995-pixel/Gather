//! Contradiction scanner (write-up §6.1).
//!
//! Incremental: each active atomic unit is scanned once (cursor column
//! `contradiction_scanned_at`, migration 0003). Candidates are blocked by
//! shared subject entity and — when embeddings exist — pgvector similarity,
//! then scored by the pure rules in score.rs; pairs at or above the
//! threshold land in `contradictions` with a `detected` audit row. Detection
//! is uniform across source modalities because units are modality-blind.

pub mod score;

use std::time::{Duration, Instant};

use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::config::Config;
use crate::extract::ollama::OllamaClient;
use score::{score_pair, UnitFacts};

#[derive(Debug, Default)]
pub struct ScanStats {
    pub units_scanned: usize,
    pub pairs_scored: usize,
    pub contradictions_found: usize,
}

/// Long-running scanner entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    let ollama = OllamaClient::from_config(&config).unwrap_or_else(|e| {
        tracing::error!(error = %e, "scanner: Ollama misconfigured; judging disabled");
        None
    });

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

    for unit_id in pending {
        let Some(unit) = load_unit_facts(pool, unit_id).await? else {
            continue; // deleted or superseded since the id was listed
        };
        let candidates = load_candidates(pool, &unit, config).await?;

        let mut conflicts: Vec<(Uuid, score::Conflict)> = Vec::new();
        for (candidate, cosine_sim) in &candidates {
            stats.pairs_scored += 1;
            let Some(mut conflict) = score_pair(&unit, candidate, *cosine_sim) else {
                continue;
            };
            // Optional local judge, only on structurally flagged pairs.
            if let Some(client) = ollama {
                match client.judge(&unit.statement, &candidate.statement).await {
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
                conflicts.push((candidate.id, conflict));
            }
        }

        // Persist findings and stamp the cursor atomically.
        let mut tx = pool.begin().await?;
        for (other_id, conflict) in &conflicts {
            let (a, b) = if unit.id < *other_id {
                (unit.id, *other_id)
            } else {
                (*other_id, unit.id)
            };
            let inserted: Option<(Uuid,)> = sqlx::query_as(
                r#"
                INSERT INTO contradictions
                    (unit_a_id, unit_b_id, score, detection_method, explanation)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (unit_a_id, unit_b_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(a)
            .bind(b)
            .bind(conflict.score)
            .bind(conflict.method)
            .bind(&conflict.explanation)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((contradiction_id,)) = inserted {
                sqlx::query(
                    r#"
                    INSERT INTO contradiction_audit
                        (contradiction_id, action, actor, to_status, note)
                    VALUES ($1, 'detected', 'scanner', 'open', $2)
                    "#,
                )
                .bind(contradiction_id)
                .bind(&conflict.explanation)
                .execute(&mut *tx)
                .await?;
                stats.contradictions_found += 1;
                metrics::counter!(
                    "gather_contradictions_detected_total",
                    "method" => conflict.method
                )
                .increment(1);
            }
        }
        sqlx::query("UPDATE atomic_units SET contradiction_scanned_at = now() WHERE id = $1")
            .bind(unit.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        stats.units_scanned += 1;
    }

    metrics::histogram!("gather_scan_duration_seconds").record(started.elapsed().as_secs_f64());
    Ok(stats)
}

async fn load_unit_facts(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<UnitFacts>> {
    let Some(row) = sqlx::query(
        r#"
        SELECT id, statement, attrs, subject_entity_id, valid_from, valid_to
        FROM atomic_units WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let mut facts = UnitFacts {
        id: row.get("id"),
        statement: row.get("statement"),
        attrs: row.get("attrs"),
        subject_entity_id: row.get("subject_entity_id"),
        valid_from: row.get("valid_from"),
        valid_to: row.get("valid_to"),
        assignments: vec![],
    };
    facts.assignments = load_assignments(pool, id).await?;
    Ok(Some(facts))
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
/// Returns each candidate's facts plus the cosine similarity when known.
async fn load_candidates(
    pool: &PgPool,
    unit: &UnitFacts,
    config: &Config,
) -> anyhow::Result<Vec<(UnitFacts, Option<f32>)>> {
    let mut rows = Vec::new();

    if let Some(subject) = unit.subject_entity_id {
        rows.extend(
            sqlx::query(
                r#"
                SELECT c.id, c.statement, c.attrs, c.subject_entity_id,
                       c.valid_from, c.valid_to,
                       CASE WHEN c.embedding IS NOT NULL AND u.embedding IS NOT NULL
                            THEN (1 - (u.embedding <=> c.embedding))::float4 END AS cosine_sim
                FROM atomic_units c
                JOIN atomic_units u ON u.id = $1
                WHERE c.subject_entity_id = $2 AND c.id <> $1 AND c.status = 'active'
                ORDER BY c.created_at DESC
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
                       c.valid_from, c.valid_to,
                       (1 - (c.embedding <=> $2))::float4 AS cosine_sim
                FROM atomic_units c
                WHERE c.embedding IS NOT NULL AND c.id <> $1 AND c.status = 'active'
                  AND (c.embedding <=> $2) < 0.35
                ORDER BY c.embedding <=> $2
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
        let mut facts = UnitFacts {
            id,
            statement: row.get("statement"),
            attrs: row.get("attrs"),
            subject_entity_id: row.get("subject_entity_id"),
            valid_from: row.get("valid_from"),
            valid_to: row.get("valid_to"),
            assignments: vec![],
        };
        facts.assignments = load_assignments(pool, id).await?;
        out.push((facts, row.get::<Option<f32>, _>("cosine_sim")));
    }
    Ok(out)
}
