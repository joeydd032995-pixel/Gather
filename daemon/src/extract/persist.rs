//! Persistence for extracted units: dedup on normalized statement hash,
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
    units: &[(ExtractedUnit, &'static str, Option<String>)], // (unit, method, model)
) -> Result<Option<PersistOutcome>, ApiError> {
    let mut tx = pool.begin().await?;

    // Claim: stamp the marker iff still unstamped; concurrent workers skip.
    let claim_sql = match chunk.anchor {
        ChunkAnchor::Message(_) => {
            "UPDATE messages SET units_extracted_at = now()
             WHERE id = $1 AND units_extracted_at IS NULL RETURNING id"
        }
        ChunkAnchor::Segment(_) => {
            "UPDATE document_segments SET units_extracted_at = now()
             WHERE id = $1 AND units_extracted_at IS NULL RETURNING id"
        }
        ChunkAnchor::Image(_) => {
            "UPDATE images SET units_extracted_at = now()
             WHERE id = $1 AND units_extracted_at IS NULL RETURNING id"
        }
    };
    let anchor_id = match chunk.anchor {
        ChunkAnchor::Message(id) | ChunkAnchor::Segment(id) | ChunkAnchor::Image(id) => id,
    };
    let claimed: Option<(Uuid,)> = sqlx::query_as(claim_sql)
        .bind(anchor_id)
        .fetch_optional(&mut *tx)
        .await?;
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

        let inserted: Option<(Uuid,)> = sqlx::query_as(
            r#"
            INSERT INTO atomic_units
                (kind, statement, statement_hash, subject_entity_id, confidence,
                 extraction_method, extraction_model, valid_from, attrs)
            VALUES ($1::unit_kind, $2, $3, $4, $5, $6::extraction_method, $7, $8, $9)
            ON CONFLICT (statement_hash) DO NOTHING
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
        .bind(&unit.attrs)
        .fetch_optional(&mut *tx)
        .await?;

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
                let (id,): (Uuid,) =
                    sqlx::query_as("SELECT id FROM atomic_units WHERE statement_hash = $1")
                        .bind(&statement_hash)
                        .fetch_one(&mut *tx)
                        .await?;
                outcome.units_reasserted += 1;
                metrics::counter!(
                    "gather_extraction_units_total",
                    "method" => *method, "status" => "deduplicated"
                )
                .increment(1);
                (id, false)
            }
        };

        let (message_id, segment_id, image_id) = match chunk.anchor {
            ChunkAnchor::Message(id) => (Some(id), None, None),
            ChunkAnchor::Segment(id) => (None, Some(id), None),
            ChunkAnchor::Image(id) => (None, None, Some(id)),
        };
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
        // re-assertions already carry them).
        if is_new {
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

/// Resolve an entity name against entities + aliases (case-insensitive),
/// creating a kind='other' entity on first sight.
async fn resolve_or_create_entity(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
) -> Result<Uuid, ApiError> {
    let name = name.trim();
    let existing: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT e.id FROM entities e
        WHERE lower(e.name) = lower($1) AND e.merged_into_entity_id IS NULL
        UNION
        SELECT a.entity_id FROM entity_aliases a WHERE lower(a.alias) = lower($1)
        LIMIT 1
        "#,
    )
    .bind(name)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((id,)) = existing {
        return Ok(id);
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
    let embeddings = ollama.embed(&texts).await?;
    let mut updated = 0usize;
    for ((id, _), embedding) in new_units.iter().zip(embeddings) {
        sqlx::query("UPDATE atomic_units SET embedding = $2 WHERE id = $1")
            .bind(id)
            .bind(Vector::from(embedding))
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        updated += 1;
    }
    Ok(updated)
}

pub async fn embed_pending_segments(
    pool: &PgPool,
    ollama: &OllamaClient,
    batch: i64,
) -> Result<usize, String> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, content FROM document_segments WHERE embedding IS NULL ORDER BY id LIMIT $1",
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
    let mut updated = 0usize;
    for ((id, _), embedding) in rows.iter().zip(embeddings) {
        sqlx::query("UPDATE document_segments SET embedding = $2 WHERE id = $1")
            .bind(id)
            .bind(Vector::from(embedding))
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        updated += 1;
    }
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
