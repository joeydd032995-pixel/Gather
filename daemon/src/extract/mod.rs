//! Extraction worker subsystem (write-up §5.1).
//!
//! A single background task drains three queues every interval:
//!   1. pending PDFs      -> text extraction + segmentation (documents)
//!   2. pending images    -> dimensions + EXIF + OCR         (images)
//!   3. unextracted chunks (messages ∪ segments ∪ image OCR) -> atomic units
//!
//! Claims are crash-safe: modality rows move pending→processing→terminal
//! (stale 'processing' rows are reset at loop start), and unit chunks are
//! stamped atomically with their units in one transaction (persist.rs).

pub mod image;
pub mod ollama;
pub mod pdf;
pub mod persist;
pub mod rules;
pub mod segment;

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::config::Config;
use ollama::OllamaClient;
use persist::{Chunk, ChunkAnchor};

#[derive(Debug, Default)]
pub struct PassStats {
    pub pdfs_processed: usize,
    pub images_processed: usize,
    pub chunks_processed: usize,
    pub units_created: usize,
}

/// Long-running worker entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    let ollama = match OllamaClient::from_config(&config) {
        Ok(client) => {
            if client.is_some() {
                tracing::info!("extraction: Ollama enabled (llm + embeddings)");
            }
            client
        }
        Err(e) => {
            tracing::error!(error = %e, "extraction: Ollama misconfigured; continuing rule-based only");
            None
        }
    };

    // Recover rows a previous process left mid-flight.
    if let Err(e) = reset_stale_processing(&pool).await {
        tracing::warn!(error = %e, "extraction: failed to reset stale processing rows");
    }

    let mut interval = tokio::time::interval(Duration::from_secs(config.extraction_interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match run_one_pass(&pool, &config, ollama.as_ref()).await {
            Ok(stats)
                if stats.pdfs_processed + stats.images_processed + stats.chunks_processed > 0 =>
            {
                tracing::info!(
                    pdfs = stats.pdfs_processed,
                    images = stats.images_processed,
                    chunks = stats.chunks_processed,
                    units = stats.units_created,
                    "extraction pass complete"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "extraction pass failed"),
        }
    }
}

async fn reset_stale_processing(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE documents SET extraction_status = 'pending' WHERE extraction_status = 'processing'",
    )
    .execute(pool)
    .await?;
    sqlx::query("UPDATE images SET ocr_status = 'pending' WHERE ocr_status = 'processing'")
        .execute(pool)
        .await?;
    Ok(())
}

/// One full pass over all three queues. Public so integration tests can
/// drive the worker deterministically.
pub async fn run_one_pass(
    pool: &PgPool,
    config: &Config,
    ollama: Option<&OllamaClient>,
) -> anyhow::Result<PassStats> {
    let pdfs_processed = process_pending_pdfs(pool, config).await?;
    let images_processed = process_pending_images(pool, config).await?;
    let (chunks_processed, units_created) = process_unit_chunks(pool, config, ollama).await?;
    let stats = PassStats {
        pdfs_processed,
        images_processed,
        chunks_processed,
        units_created,
    };

    if let Some(client) = ollama {
        match persist::embed_pending_segments(pool, client, config.extraction_batch).await {
            Ok(n) if n > 0 => tracing::debug!(segments = n, "embedded document segments"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "segment embedding failed; will retry"),
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Phase 1: PDFs
// ---------------------------------------------------------------------------

async fn process_pending_pdfs(pool: &PgPool, config: &Config) -> anyhow::Result<usize> {
    let claimed = sqlx::query(
        r#"
        UPDATE documents SET extraction_status = 'processing'
        WHERE id IN (
            SELECT d.id FROM documents d
            WHERE d.extraction_status = 'pending'
            ORDER BY d.id LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, artifact_id
        "#,
    )
    .bind(config.extraction_batch)
    .fetch_all(pool)
    .await?;

    let mut processed = 0usize;
    for row in claimed {
        let document_id: Uuid = row.get("id");
        let artifact_id: Uuid = row.get("artifact_id");
        let bytes: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT raw_content FROM artifacts WHERE id = $1")
                .bind(artifact_id)
                .fetch_optional(pool)
                .await?
                .filter(|(b,): &(Vec<u8>,)| !b.is_empty());

        let outcome = match bytes {
            Some((b,)) => pdf::extract(b).await,
            None => pdf::PdfOutcome::Failed("artifact has no inline raw content".to_string()),
        };

        match outcome {
            pdf::PdfOutcome::Ok(extraction) => {
                let mut tx = pool.begin().await?;
                let mut segments = 0usize;
                for (seq, seg) in segment::segment_text(&extraction.text)
                    .into_iter()
                    .enumerate()
                {
                    sqlx::query(
                        r#"
                        INSERT INTO document_segments
                            (document_id, seq, heading, content, content_hash)
                        VALUES ($1, $2, $3, $4, $5)
                        ON CONFLICT (document_id, seq) DO NOTHING
                        "#,
                    )
                    .bind(document_id)
                    .bind(seq as i32)
                    .bind(&seg.heading)
                    .bind(&seg.content)
                    .bind(crate::routes::ingest::sha256_hex(seg.content.as_bytes()))
                    .execute(&mut *tx)
                    .await?;
                    segments += 1;
                }
                sqlx::query(
                    r#"
                    UPDATE documents
                    SET extracted_text = $2, page_count = $3, extraction_tool = 'pdf-extract',
                        extraction_status = 'completed', extracted_at = now()
                    WHERE id = $1
                    "#,
                )
                .bind(document_id)
                .bind(&extraction.text)
                .bind(extraction.page_count)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                metrics::counter!("gather_extraction_segments_total", "tool" => "pdf-extract")
                    .increment(segments as u64);
            }
            pdf::PdfOutcome::NeedsOcr { page_count } => {
                sqlx::query(
                    r#"
                    UPDATE documents
                    SET extraction_status = 'skipped', page_count = $2,
                        extraction_tool = 'pdf-extract', extracted_at = now(),
                        metadata = metadata || $3::jsonb
                    WHERE id = $1
                    "#,
                )
                .bind(document_id)
                .bind(page_count)
                .bind(json!({ "reason": "scanned-pdf-needs-ocr" }))
                .execute(pool)
                .await?;
                tracing::warn!(%document_id, "pdf has no extractable text (scanned?); marked skipped");
            }
            pdf::PdfOutcome::Failed(reason) => {
                sqlx::query(
                    r#"
                    UPDATE documents
                    SET extraction_status = 'failed', extracted_at = now(),
                        metadata = metadata || $2::jsonb
                    WHERE id = $1
                    "#,
                )
                .bind(document_id)
                .bind(json!({ "error": reason }))
                .execute(pool)
                .await?;
                tracing::warn!(%document_id, "pdf extraction failed");
            }
        }
        processed += 1;
    }
    Ok(processed)
}

// ---------------------------------------------------------------------------
// Phase 2: images
// ---------------------------------------------------------------------------

async fn process_pending_images(pool: &PgPool, config: &Config) -> anyhow::Result<usize> {
    let claimed = sqlx::query(
        r#"
        UPDATE images SET ocr_status = 'processing'
        WHERE id IN (
            SELECT i.id FROM images i
            WHERE i.ocr_status = 'pending'
            ORDER BY i.id LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, artifact_id
        "#,
    )
    .bind(config.extraction_batch)
    .fetch_all(pool)
    .await?;

    let mut processed = 0usize;
    for row in claimed {
        let image_id: Uuid = row.get("id");
        let artifact_id: Uuid = row.get("artifact_id");
        let artifact = sqlx::query("SELECT raw_content, media_type FROM artifacts WHERE id = $1")
            .bind(artifact_id)
            .fetch_optional(pool)
            .await?;
        let Some(artifact) = artifact else {
            continue;
        };
        let bytes: Vec<u8> = artifact
            .get::<Option<Vec<u8>>, _>("raw_content")
            .unwrap_or_default();
        let media_type: Option<String> = artifact.get("media_type");

        // Metadata is cheap and never blocks OCR.
        let analysis = image::analyze(&bytes);
        sqlx::query(
            "UPDATE images SET width = $2, height = $3, exif = $4, taken_at = $5 WHERE id = $1",
        )
        .bind(image_id)
        .bind(analysis.width)
        .bind(analysis.height)
        .bind(&analysis.exif)
        .bind(analysis.taken_at)
        .execute(pool)
        .await?;

        let extension = image::extension_for(media_type.as_deref());
        let (status, text, confidence): (&str, Option<String>, Option<f32>) =
            match image::ocr(&config.tesseract_path, &bytes, extension).await {
                image::OcrOutcome::Ok(result) => {
                    ("completed", Some(result.text), Some(result.confidence))
                }
                image::OcrOutcome::Empty => ("completed", None, None),
                image::OcrOutcome::Unavailable => {
                    tracing::warn!(
                        tesseract = %config.tesseract_path,
                        "tesseract binary not found; image OCR skipped"
                    );
                    ("skipped", None, None)
                }
                image::OcrOutcome::Failed(reason) => {
                    tracing::warn!(%image_id, error = %reason, "ocr failed");
                    ("failed", None, None)
                }
            };
        sqlx::query(
            r#"
            UPDATE images
            SET ocr_status = $2::extraction_status, ocr_text = $3, ocr_confidence = $4
            WHERE id = $1
            "#,
        )
        .bind(image_id)
        .bind(status)
        .bind(&text)
        .bind(confidence)
        .execute(pool)
        .await?;
        metrics::counter!("gather_extraction_ocr_total", "status" => status.to_string())
            .increment(1);
        processed += 1;
    }
    Ok(processed)
}

// ---------------------------------------------------------------------------
// Phase 3: unified atomic-unit extraction
// ---------------------------------------------------------------------------

async fn process_unit_chunks(
    pool: &PgPool,
    config: &Config,
    ollama: Option<&OllamaClient>,
) -> anyhow::Result<(usize, usize)> {
    let mut chunks: Vec<Chunk> = Vec::new();

    for row in sqlx::query(
        r#"
        SELECT m.id, m.content, m.role,
               COALESCE(m.created_at, a.source_created_at, a.ingested_at) AS source_time,
               c.artifact_id
        FROM messages m
        JOIN conversations c ON c.id = m.conversation_id
        JOIN artifacts a ON a.id = c.artifact_id
        WHERE m.units_extracted_at IS NULL
        ORDER BY m.id LIMIT $1
        "#,
    )
    .bind(config.extraction_batch)
    .fetch_all(pool)
    .await?
    {
        chunks.push(Chunk {
            anchor: ChunkAnchor::Message(row.get("id")),
            artifact_id: row.get("artifact_id"),
            text: row.get("content"),
            source_time: row.get("source_time"),
            user_authored: row.get::<String, _>("role") == "user",
            ocr_confidence: None,
        });
    }

    for row in sqlx::query(
        r#"
        SELECT s.id, s.content,
               COALESCE(a.source_created_at, a.ingested_at) AS source_time,
               d.artifact_id
        FROM document_segments s
        JOIN documents d ON d.id = s.document_id
        JOIN artifacts a ON a.id = d.artifact_id
        WHERE s.units_extracted_at IS NULL
        ORDER BY s.id LIMIT $1
        "#,
    )
    .bind(config.extraction_batch)
    .fetch_all(pool)
    .await?
    {
        chunks.push(Chunk {
            anchor: ChunkAnchor::Segment(row.get("id")),
            artifact_id: row.get("artifact_id"),
            text: row.get("content"),
            source_time: row.get("source_time"),
            user_authored: false,
            ocr_confidence: None,
        });
    }

    for row in sqlx::query(
        r#"
        SELECT i.id, i.ocr_text, i.ocr_confidence,
               COALESCE(i.taken_at, a.source_created_at, a.ingested_at) AS source_time,
               i.artifact_id
        FROM images i
        JOIN artifacts a ON a.id = i.artifact_id
        WHERE i.units_extracted_at IS NULL
          AND i.ocr_status = 'completed'
          AND i.ocr_text IS NOT NULL AND length(trim(i.ocr_text)) > 0
        ORDER BY i.id LIMIT $1
        "#,
    )
    .bind(config.extraction_batch)
    .fetch_all(pool)
    .await?
    {
        chunks.push(Chunk {
            anchor: ChunkAnchor::Image(row.get("id")),
            artifact_id: row.get("artifact_id"),
            text: row.get("ocr_text"),
            source_time: row.get("source_time"),
            user_authored: false,
            ocr_confidence: row.get("ocr_confidence"),
        });
    }

    let mut processed = 0usize;
    let mut created = 0usize;
    for chunk in &chunks {
        let mut units: Vec<(rules::ExtractedUnit, &'static str, Option<String>)> =
            rules::extract_units(&chunk.text)
                .into_iter()
                .map(|u| (u, "rule_based", None))
                .collect();

        if let Some(client) = ollama {
            match client.extract(&chunk.text).await {
                Ok(llm_units) => {
                    let model = Some(format!("ollama:{}", client.model));
                    units.extend(
                        llm_units
                            .into_iter()
                            .map(|u| (u, "llm_local", model.clone())),
                    );
                }
                Err(e) => tracing::warn!(error = %e, "llm extraction failed; rule-based only"),
            }
        }

        match persist::persist_chunk_units(pool, chunk, &units).await? {
            Some(outcome) => {
                processed += 1;
                created += outcome.units_created;

                if let Some(client) = ollama {
                    if let Err(e) = persist::embed_new_units(pool, client, &outcome.new_units).await
                    {
                        tracing::warn!(error = %e, "unit embedding failed; will remain NULL");
                    }
                }
            }
            None => {
                // Raced with another pass; nothing to do.
            }
        }
    }
    Ok((processed, created))
}
