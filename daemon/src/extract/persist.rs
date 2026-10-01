//! Persistence for extracted units: dedup within a live or anchored episode,
//! provenance anchoring, entity resolution, relationship edges, temporal
//! validity, and optional embedding backfill. One transaction per chunk —
//! a crash mid-pass never leaves a chunk half-persisted or double-stamped.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::ollama::OllamaClient;
use super::rules::ExtractedUnit;
use crate::decide::{admit_unit, Band};
use crate::error::ApiError;

#[derive(Debug, Clone, Copy)]
pub enum ChunkAnchor {
    Message(Uuid),
    Segment(Uuid),
    Image(Uuid),
}

/// A unit-extraction work item: one message, document segment, or image OCR
/// text, with everything persistence needs to score and date its units.
pub struct Chunk {
    pub anchor: ChunkAnchor,
    pub artifact_id: Uuid,
    pub text: String,
    /// Best source timestamp (message time / EXIF taken_at / artifact time).
    pub source_time: Option<DateTime<Utc>>,
    /// True for chat messages authored by the user (confidence bonus).
    pub user_authored: bool,
    /// OCR mean confidence when the chunk came from an image.
    pub ocr_confidence: Option<f32>,
}

/// How a chunk is taken for persisting.
#[derive(Debug, Clone)]
pub enum Claim {
    /// First read: the chunk must not have been read yet. `llm_model` is the
    /// model whose answer is among the units, if it answered.
    Fresh { llm_model: Option<String> },
    /// A re-read by `model` within re-read job `job`: the chunk must already
    /// have been read, but not by this model and not yet by this job. Only the
    /// model's units are given; a unit the chunk already backs is left alone.
    Reread { job: Uuid, model: String },
}

pub struct PersistOutcome {
    pub units_created: usize,
    pub units_reasserted: usize,
    /// Newly created unit ids + statements, for embedding backfill.
    pub new_units: Vec<(Uuid, String)>,
}

/// Normalize a statement for dedup hashing: case-, whitespace- and trailing-
/// punctuation-insensitive.
pub fn normalize_statement(statement: &str) -> String {
    statement
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_string()
}

/// Persist all units for one chunk and stamp its `units_extracted_at`
/// marker atomically. Returns None if another pass already claimed the chunk.
pub async fn persist_chunk_units(
    pool: &PgPool,
    chunk: &Chunk,
    claim: &Claim,
    units: &[(ExtractedUnit, &'static str, Option<String>)], // (unit, method, model)
    hold_below: f32,
    drop_below: f32,
) -> Result<Option<PersistOutcome>, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gather.scan.write'))")
        .execute(&mut *tx)
        .await?;

    // Serialize with source withdrawal before touching chunk or derived rows.
    // The model call happens before this transaction, so check liveness again.
    let live: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM artifacts WHERE id = $1 AND retracted_at IS NULL FOR UPDATE",
    )
    .bind(chunk.artifact_id)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }

    // Claim: stamp the marker iff still unstamped (or, for a re-read, still
    // not read by this model or job); concurrent workers skip.
    let claim_sql = match (claim, chunk.anchor) {
        (Claim::Fresh { .. }, ChunkAnchor::Message(_)) => {
            "UPDATE messages SET units_extracted_at = now(), units_llm_model = $2
             WHERE id = $1 AND units_extracted_at IS NULL RETURNING id"
        }
        (Claim::Fresh { .. }, ChunkAnchor::Segment(_)) => {
            "UPDATE document_segments SET units_extracted_at = now(), units_llm_model = $2
             WHERE id = $1 AND units_extracted_at IS NULL RETURNING id"
        }
        (Claim::Fresh { .. }, ChunkAnchor::Image(_)) => {
            "UPDATE images SET units_extracted_at = now(), units_llm_model = $2
             WHERE id = $1 AND units_extracted_at IS NULL RETURNING id"
        }
        (Claim::Reread { .. }, ChunkAnchor::Message(_)) => {
            "UPDATE messages SET units_llm_model = $2, units_reread_job = $3,
                    units_extract_error = NULL
             WHERE id = $1 AND units_extracted_at IS NOT NULL
               AND units_llm_model IS DISTINCT FROM $2
               AND units_reread_job IS DISTINCT FROM $3 RETURNING id"
        }
        (Claim::Reread { .. }, ChunkAnchor::Segment(_)) => {
            "UPDATE document_segments SET units_llm_model = $2, units_reread_job = $3,
                    units_extract_error = NULL
             WHERE id = $1 AND units_extracted_at IS NOT NULL
               AND units_llm_model IS DISTINCT FROM $2
               AND units_reread_job IS DISTINCT FROM $3 RETURNING id"
        }
        (Claim::Reread { .. }, ChunkAnchor::Image(_)) => {
            "UPDATE images SET units_llm_model = $2, units_reread_job = $3,
                    units_extract_error = NULL
             WHERE id = $1 AND units_extracted_at IS NOT NULL
               AND units_llm_model IS DISTINCT FROM $2
               AND units_reread_job IS DISTINCT FROM $3 RETURNING id"
        }
    };
    let anchor_id = match chunk.anchor {
        ChunkAnchor::Message(id) | ChunkAnchor::Segment(id) | ChunkAnchor::Image(id) => id,
    };
    let query = sqlx::query_as(claim_sql).bind(anchor_id);
    let query = match claim {
        Claim::Fresh { llm_model } => query.bind(llm_model.as_deref()),
        Claim::Reread { job, model } => query.bind(model.as_str()).bind(job),
    };
    let claimed: Option<(Uuid,)> = query.fetch_optional(&mut *tx).await?;
    if claimed.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }

    let mut outcome = PersistOutcome {
        units_created: 0,
        units_reasserted: 0,
        new_units: Vec::new(),
    };

    for (unit, method, model) in units {
        let subject_entity_id = match &unit.subject {
            Some(name) => Some(resolve_or_create_entity(&mut tx, name).await?),
            None => None,
        };

        // Confidence adjustments (write-up §5.2).
        let mut confidence = unit.confidence;
        if subject_entity_id.is_some() && unit.subject.as_deref() != Some("Me") {
            confidence += 0.1;
        }
        if chunk.user_authored {
            confidence += 0.1;
        }
        if chunk.ocr_confidence.map(|c| c < 0.7).unwrap_or(false) {
            confidence -= 0.1;
        }
        let confidence = confidence.clamp(0.0, 1.0);

        let valid_from = unit.event_time.or(chunk.source_time);
        let statement_hash = hex::encode(Sha256::digest(normalize_statement(&unit.statement)));

        // Modality and polarity travel with the unit. Only an actual,
        // non-negated claim may assert graph edges: a plan, a possibility, a
        // condition, a rejection or a denial never becomes a present fact.
        let reading = crate::safety::modality::classify(&unit.statement);
        let mut attrs = unit.attrs.clone();
        if let Some(map) = attrs.as_object_mut() {
            map.entry("modality")
                .or_insert_with(|| serde_json::json!(reading.modality.as_str()));
            map.entry("negated")
                .or_insert_with(|| serde_json::json!(reading.negated));
            if reading.ambiguous {
                map.insert("modality_ambiguous".into(), serde_json::json!(true));
            }
        }

        // Per-proposition serialization replaces global uniqueness. Reuse a
        // episode with the same assertion time or this anchor's original
        // episode, preserving explicit rejection until the user restores it.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(&statement_hash)
            .execute(&mut *tx)
            .await?;
        let (message_id, segment_id, image_id) = anchor_columns(chunk.anchor);
        let existing: Option<(Uuid,)> = sqlx::query_as(
            "SELECT u.id FROM atomic_units u WHERE u.statement_hash = $1
             AND ((u.status IN ('active', 'disputed') AND u.valid_from IS NOT DISTINCT FROM $5)
                  OR EXISTS (SELECT 1 FROM atomic_unit_provenance p
                             WHERE p.atomic_unit_id = u.id
                               AND p.message_id IS NOT DISTINCT FROM $2
                               AND p.document_segment_id IS NOT DISTINCT FROM $3
                               AND p.image_id IS NOT DISTINCT FROM $4)
                  OR (SELECT f.action FROM unit_feedback f
                      WHERE f.target_kind = 'unit' AND f.target_id = u.id
                        AND f.action IN ('reject', 'confirm')
                      ORDER BY f.created_at DESC, f.id DESC LIMIT 1) = 'reject')
             ORDER BY EXISTS (SELECT 1 FROM atomic_unit_provenance p
                              WHERE p.atomic_unit_id = u.id
                                AND p.message_id IS NOT DISTINCT FROM $2
                                AND p.document_segment_id IS NOT DISTINCT FROM $3
                                AND p.image_id IS NOT DISTINCT FROM $4) DESC,
                      u.created_at DESC, u.id DESC LIMIT 1",
        )
        .bind(&statement_hash)
        .bind(message_id)
        .bind(segment_id)
        .bind(image_id)
        .bind(valid_from)
        .fetch_optional(&mut *tx)
        .await?;
        let inserted: Option<(Uuid,)> = if existing.is_some() {
            None
        } else {
            sqlx::query_as(
                r#"
                INSERT INTO atomic_units
                    (kind, statement, statement_hash, subject_entity_id, confidence,
                     extraction_method, extraction_model, valid_from, attrs,
                     asserted_at, observed_at)
                VALUES ($1::unit_kind, $2, $3, $4, $5, $6::extraction_method, $7, $8, $9, $10, $11)
                RETURNING id
                "#,
            )
            .bind(unit.kind)
            .bind(&unit.statement)
            .bind(&statement_hash)
            .bind(subject_entity_id)
            .bind(confidence)
            .bind(method)
            .bind(model)
            .bind(valid_from)
            .bind(&attrs)
            .bind(chunk.source_time)
            .bind(unit.event_time)
            .fetch_optional(&mut *tx)
            .await?
        };
        let (unit_id, is_new) = match inserted {
            Some((id,)) => {
                outcome.units_created += 1;
                outcome.new_units.push((id, unit.statement.clone()));
                metrics::counter!(
                    "gather_extraction_units_total",
                    "method" => *method, "status" => "ok"
                )
                .increment(1);
                (id, true)
            }
            None => {
                // Re-assertion of a known statement: reuse the unit, add provenance.
                let (id,) = existing.expect("existing episode checked above");
                if matches!(claim, Claim::Reread { .. }) {
                    // Already backed by this very chunk: a second provenance
                    // row would count one source twice.
                    let (message_id, segment_id, image_id) = anchor_columns(chunk.anchor);
                    let backed: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM atomic_unit_provenance
                            WHERE atomic_unit_id = $1
                              AND message_id IS NOT DISTINCT FROM $2
                              AND document_segment_id IS NOT DISTINCT FROM $3
                              AND image_id IS NOT DISTINCT FROM $4)
                        "#,
                    )
                    .bind(id)
                    .bind(message_id)
                    .bind(segment_id)
                    .bind(image_id)
                    .fetch_one(&mut *tx)
                    .await?;
                    if backed {
                        continue;
                    }
                }
                outcome.units_reasserted += 1;
                metrics::counter!(
                    "gather_extraction_units_total",
                    "method" => *method, "status" => "deduplicated"
                )
                .increment(1);
                (id, false)
            }
        };

        // Auto-act admission (autonomous pipeline, Phase A). `confidence`
        // already carries the source-context adjustments above. Auto -> keep
        // active (the INSERT default); Hold -> still active but parked in
        // review_queue for optional attention; Drop -> retract (off by default,
        // drop_below = 0). Only new units are classified; a re-assertion keeps
        // whatever state it already had.
        let admit_band = if is_new {
            match admit_unit(confidence, hold_below, drop_below) {
                // Rules can't tell whether this is stated, denied or only
                // possible: keep it, and ask.
                Band::Auto if reading.ambiguous => Band::Hold,
                band => band,
            }
        } else {
            Band::Auto
        };
        if is_new {
            match admit_band {
                Band::Auto => {}
                Band::Hold => {
                    let reason = if reading.ambiguous {
                        "modality-uncertain"
                    } else {
                        "low-confidence"
                    };
                    sqlx::query(
                        r#"
                        INSERT INTO review_queue (target_kind, target_id, reason, signals)
                        VALUES ('unit', $1, $3,
                                jsonb_build_object('confidence', $2::float4,
                                                   'modality', $4::text,
                                                   'negated', $5::bool))
                        ON CONFLICT DO NOTHING
                        "#,
                    )
                    .bind(unit_id)
                    .bind(confidence)
                    .bind(reason)
                    .bind(reading.modality.as_str())
                    .bind(reading.negated)
                    .execute(&mut *tx)
                    .await?;
                }
                Band::Drop => {
                    sqlx::query("UPDATE atomic_units SET status = 'retracted' WHERE id = $1")
                        .bind(unit_id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }

        let (message_id, segment_id, image_id) = anchor_columns(chunk.anchor);
        // A re-assertion folds a new source into an existing proposition.
        // Record that, and whether the new source is independent of the ones
        // already behind it (copies and derivations are not corroboration).
        if !is_new {
            let existing = crate::safety::service::unit_sources(&mut tx, unit_id).await?;
            let new_source = crate::safety::provenance::SourceRef {
                artifact: chunk.artifact_id,
                fingerprint: Some(hex::encode(Sha256::digest(chunk.text.as_bytes()))),
            };
            let mut artifacts: Vec<Uuid> = existing.iter().map(|s| s.artifact).collect();
            artifacts.push(chunk.artifact_id);
            let derivations = crate::safety::service::derivations_for(&mut tx, &artifacts).await?;
            let (canonical, corroboration) = crate::safety::provenance::reassertion_certificates(
                unit_id,
                &unit.statement,
                &existing,
                &new_source,
                &derivations,
                model.clone().or_else(|| Some((*method).to_string())),
            );
            crate::safety::store::record(&mut tx, &canonical).await?;
            crate::safety::store::record(&mut tx, &corroboration).await?;
        }

        let quote_end = unit.char_end.min(chunk.text.len());
        let quote = chunk
            .text
            .get(unit.char_start..quote_end)
            .unwrap_or(&unit.statement)
            .trim();
        sqlx::query(
            r#"
            INSERT INTO atomic_unit_provenance
                (atomic_unit_id, artifact_id, message_id, document_segment_id,
                 image_id, char_start, char_end, quote)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(unit_id)
        .bind(chunk.artifact_id)
        .bind(message_id)
        .bind(segment_id)
        .bind(image_id)
        .bind(unit.char_start as i32)
        .bind(quote_end as i32)
        .bind(quote)
        .execute(&mut *tx)
        .await?;

        // Relationship edges asserted by this unit (only on first creation;
        // re-assertions already carry them). A dropped unit is retracted, so it
        // must not seed active edges.
        if is_new && admit_band != Band::Drop && reading.asserts_positive_fact() {
            if let Some(source_entity) = subject_entity_id {
                for (object_name, relation) in &unit.objects {
                    let target_entity = resolve_or_create_entity(&mut tx, object_name).await?;
                    if target_entity == source_entity {
                        continue; // schema forbids self-loops
                    }
                    sqlx::query(
                        r#"
                        INSERT INTO relationships
                            (source_entity_id, target_entity_id, relation_type,
                             atomic_unit_id, confidence, valid_from)
                        VALUES ($1, $2, $3, $4, $5, $6)
                        ON CONFLICT DO NOTHING
                        "#,
                    )
                    .bind(source_entity)
                    .bind(target_entity)
                    .bind(relation)
                    .bind(unit_id)
                    .bind(confidence)
                    .bind(valid_from)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
    }

    tx.commit().await?;
    Ok(Some(outcome))
}

/// The (message, segment, image) provenance columns an anchor fills.
fn anchor_columns(anchor: ChunkAnchor) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
    match anchor {
        ChunkAnchor::Message(id) => (Some(id), None, None),
        ChunkAnchor::Segment(id) => (None, Some(id), None),
        ChunkAnchor::Image(id) => (None, None, Some(id)),
    }
}

/// Resolve an entity name against entities + aliases (case-insensitive),
/// creating a kind='other' entity on first sight.
///
/// Public because it is the read counterpart to the merge write path in
/// `crate::entities` — after a merge, the loser's name must resolve here to
/// the winner, and the integration suite asserts exactly that.
/// Find a live entity by name, or by an alias (followed to its survivor).
async fn lookup_entity(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
) -> Result<Option<Uuid>, ApiError> {
    let existing: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT e.id FROM entities e
        WHERE lower(e.name) = lower($1) AND e.merged_into_entity_id IS NULL
        UNION
        -- Resolve through the alias owner's merge pointer rather than trusting
        -- it: an alias left on a merged-away entity would otherwise hand back
        -- the retired node and attach new units to it.
        SELECT coalesce(owner.merged_into_entity_id, owner.id)
        FROM entity_aliases a
        JOIN entities owner ON owner.id = a.entity_id
        WHERE lower(a.alias) = lower($1)
        LIMIT 1
        "#,
    )
    .bind(name)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(existing.map(|(id,)| id))
}

/// Upper bound on lock-then-recheck rounds; a name only moves when a merge
/// or unmerge commits, so a second round is already rare.
const RESOLVE_ATTEMPTS: usize = 3;

pub async fn resolve_or_create_entity(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
) -> Result<Uuid, ApiError> {
    let name = name.trim();
    let mut candidate = lookup_entity(tx, name).await?;
    for _ in 0..RESOLVE_ATTEMPTS {
        let Some(id) = candidate else {
            break;
        };
        // Re-read the resolved row under a shared lock before handing it back.
        // A merge (or unmerge) holds FOR UPDATE on both operands while it
        // repoints units and relationships, and soft-deletes rather than
        // removes the loser — so without this, an ingest that read the row
        // just before the merge committed would still insert onto the retired
        // node, after the repointing had already run, stranding the new data.
        //
        // FOR SHARE makes both interleavings safe: if ingestion gets here
        // first, the merge waits and repoints this unit along with the rest;
        // if the merge got there first, this blocks until it commits.
        let merged_into: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into_entity_id FROM entities WHERE id = $1 FOR SHARE",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
        // What we waited on may have moved the NAME, not just the row: an
        // unmerge hands the loser's name and aliases back to the loser. So
        // look the name up again now that the change is committed; if it now
        // resolves to a different entity, lock and settle on that one. (A
        // lookup that finds nothing means the row was retired without an
        // alias; its merge pointer, below, is still the right answer.)
        if let Some(other) = lookup_entity(tx, name).await?.filter(|&o| o != id) {
            candidate = Some(other);
            continue;
        }
        return match merged_into {
            // Merges flatten descendants, so this is one hop; resolve_head_tx
            // still walks defensively in case of rows from an older build.
            Some(Some(head)) => crate::entities::merge::resolve_head_tx(tx, head).await,
            _ => Ok(id),
        };
    }
    if let Some(id) = candidate {
        // Still moving after several rounds: keep the latest answer, following
        // any merge pointer, rather than creating a duplicate entity.
        return crate::entities::merge::resolve_head_tx(tx, id).await;
    }
    let created: Option<(Uuid,)> = sqlx::query_as(
        r#"
        INSERT INTO entities (name, kind)
        VALUES ($1, CASE WHEN $1 = 'Me' THEN 'person'::entity_kind ELSE 'other'::entity_kind END)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(name)
    .fetch_optional(&mut **tx)
    .await?;
    match created {
        Some((id,)) => Ok(id),
        None => {
            // Raced with another insert in this transaction scope; re-select.
            let (id,): (Uuid,) = sqlx::query_as(
                "SELECT id FROM entities WHERE lower(name) = lower($1) \
                 AND merged_into_entity_id IS NULL",
            )
            .bind(name)
            .fetch_one(&mut **tx)
            .await?;
            Ok(id)
        }
    }
}

/// Backfill embeddings for newly created units and any segments still
/// missing one. Failures degrade gracefully (embeddings are an enhancement,
/// not a dependency).
pub async fn embed_new_units(
    pool: &PgPool,
    ollama: &OllamaClient,
    new_units: &[(Uuid, String)],
) -> Result<usize, String> {
    if new_units.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = new_units.iter().map(|(_, s)| s.clone()).collect();
    let ids: Vec<Uuid> = new_units.iter().map(|(id, _)| *id).collect();
    let revisions: std::collections::HashMap<Uuid, i64> =
        sqlx::query_as("SELECT id, content_revision FROM atomic_units WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect();
    let embeddings = ollama.embed(&texts).await?;
    let Some(mut tx) = embedding_write_transaction(pool, &ollama.embed_model).await? else {
        return Ok(0);
    };
    let mut updated = 0usize;
    for ((id, statement), embedding) in new_units.iter().zip(embeddings) {
        let result = sqlx::query(
            "UPDATE atomic_units SET embedding = $2, embedding_model = $4,
                    embedding_attempts = 0, embedding_retry_at = NULL
             WHERE id = $1 AND statement = $3 AND content_revision = $5
               AND embedding IS NULL AND status = 'active'
               AND (SELECT model FROM embedding_state WHERE singleton) = $4",
        )
        .bind(id)
        .bind(Vector::from(embedding))
        .bind(statement)
        .bind(&ollama.embed_model)
        .bind(revisions.get(id).copied().unwrap_or(-1))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        updated += result.rows_affected() as usize;
    }
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(updated)
}

/// Retry old ingestion, temporary model failures, and corrections in bounded batches.
pub async fn embed_pending_units(
    pool: &PgPool,
    ollama: &OllamaClient,
    batch: i64,
) -> Result<usize, String> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, statement FROM atomic_units
         WHERE embedding IS NULL AND status = 'active'
           AND (embedding_retry_at IS NULL OR embedding_retry_at <= now())
         ORDER BY embedding_retry_at NULLS FIRST, created_at, id LIMIT $1",
    )
    .bind(batch)
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;
    match embed_new_units(pool, ollama, &rows).await {
        Ok(count) => Ok(count),
        Err(error) => {
            for (id, statement) in &rows {
                sqlx::query(
                    "UPDATE atomic_units
                     SET embedding_attempts = least(embedding_attempts + 1, 10),
                         embedding_retry_at = now() + make_interval(
                             secs => least(3600, 5 * power(2, embedding_attempts)::integer))
                     WHERE id = $1 AND statement = $2 AND embedding IS NULL",
                )
                .bind(id)
                .bind(statement)
                .execute(pool)
                .await
                .map_err(|error| error.to_string())?;
            }
            Err(error)
        }
    }
}

/// Changing the configured local model invalidates all incompatible vectors in
/// one transaction, before serving searches or starting background workers.
pub async fn ensure_embedding_model(pool: &PgPool, model: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gather.scan.write'))")
        .execute(&mut *tx)
        .await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT model FROM embedding_state WHERE singleton FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    if current.as_deref() != Some(model) {
        sqlx::query(
            "UPDATE atomic_units SET embedding = NULL, embedding_model = NULL,
             clustered_at = NULL, topic_cluster_id = NULL, contradiction_scanned_at = NULL,
             embedding_retry_at = NULL, embedding_attempts = 0",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE document_segments SET embedding = NULL, embedding_model = NULL")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE entities SET embedding = NULL, embedding_model = NULL")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE images SET embedding = NULL, embedding_model = NULL,
             captioned_at = NULL, topic_cluster_id = NULL",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM cluster_members m USING clusters c WHERE m.cluster_id = c.id
             AND (m.member_kind = 'unit' OR (m.member_kind = 'image' AND c.kind = 'photo_topic'))",
        )
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE embedding_state SET model = $1, generation = generation + 1 WHERE singleton",
        )
        .bind(model)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Writers lock model identity before derived rows, so a model change and an
/// in-flight request cannot repopulate each other's vector space.
pub async fn embedding_write_transaction<'a>(
    pool: &'a PgPool,
    model: &str,
) -> Result<Option<Transaction<'a, Postgres>>, String> {
    let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
    // Also supports library users/tests that construct a client without main.
    // Once set, only the explicit startup model-change path may replace it.
    sqlx::query(
        "UPDATE embedding_state SET model = $1, generation = generation + 1
         WHERE singleton AND model IS NULL",
    )
    .bind(model)
    .execute(&mut *tx)
    .await
    .map_err(|error| error.to_string())?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT model FROM embedding_state WHERE singleton FOR SHARE")
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
    if current.as_deref() != Some(model) {
        return Ok(None);
    }
    Ok(Some(tx))
}

pub async fn embed_pending_segments(
    pool: &PgPool,
    ollama: &OllamaClient,
    batch: i64,
) -> Result<usize, String> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT s.id, s.content FROM document_segments s
         JOIN documents d ON d.id = s.document_id
         JOIN artifacts a ON a.id = d.artifact_id
         WHERE s.embedding IS NULL AND a.retracted_at IS NULL ORDER BY s.id LIMIT $1",
    )
    .bind(batch)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = rows.iter().map(|(_, c)| c.clone()).collect();
    let embeddings = ollama.embed(&texts).await?;
    let Some(mut tx) = embedding_write_transaction(pool, &ollama.embed_model).await? else {
        return Ok(0);
    };
    let mut updated = 0usize;
    for ((id, statement), embedding) in rows.iter().zip(embeddings) {
        let result = sqlx::query(
            "UPDATE document_segments SET embedding = $2, embedding_model = $4
             WHERE id = $1 AND content = $3 AND embedding IS NULL
               AND (SELECT model FROM embedding_state WHERE singleton) = $4",
        )
        .bind(id)
        .bind(Vector::from(embedding))
        .bind(statement)
        .bind(&ollama.embed_model)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        updated += result.rows_affected() as usize;
    }
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::normalize_statement;

    #[test]
    fn normalization_is_case_space_punct_insensitive() {
        assert_eq!(
            normalize_statement("My  VPS Budget is $75 per month."),
            normalize_statement("my vps budget is $75 per month")
        );
        assert_ne!(
            normalize_statement("budget is $75"),
            normalize_statement("budget is $50")
        );
    }
}
