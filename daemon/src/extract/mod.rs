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

pub mod digest;
pub mod digest_job;
pub mod formats;
pub mod image;
pub mod ollama;
pub mod pdf;
pub mod persist;
pub mod reread;
pub mod rules;
pub mod segment;
pub mod worth;

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::config::Config;
use crate::decide::live::LiveThresholds;
use ollama::OllamaClient;
use persist::{Chunk, ChunkAnchor};

#[derive(Debug, Default)]
pub struct PassStats {
    pub pdfs_processed: usize,
    pub images_processed: usize,
    pub chunks_processed: usize,
    pub units_created: usize,
    /// Chunks that failed and were set aside with their error.
    pub chunks_failed: usize,
    /// Documents summarized.
    pub digests_built: usize,
}

impl PassStats {
    fn did_work(&self) -> bool {
        self.pdfs_processed
            + self.images_processed
            + self.chunks_processed
            + self.chunks_failed
            + self.digests_built
            > 0
    }
}

/// Pause between passes while there is still work queued: enough to let
/// requests from the app in, short enough that a big import is read in
/// minutes or hours rather than days.
const BUSY_PAUSE: Duration = Duration::from_millis(200);
/// Longest rest after one pass, however long the pass took.
const MAX_REST: Duration = Duration::from_secs(120);

/// How long to rest after a busy pass that took `worked`, so the worker is
/// busy `duty_percent` of the time: at 30 %, 3 s of work is followed by 7 s
/// of rest. Never shorter than [`BUSY_PAUSE`].
fn rest_after(worked: Duration, duty_percent: u8) -> Duration {
    let duty = f64::from(duty_percent.clamp(10, 100));
    let rest = worked.mul_f64((100.0 - duty) / duty);
    rest.clamp(BUSY_PAUSE, MAX_REST)
}
/// Rest after one model request that took `worked`, so that the model is
/// working `duty_percent` of the time however long a pass runs. Done per
/// request, not per pass: a slow model can keep a pass going for minutes.
pub(crate) async fn pace(worked: Duration, duty_percent: u8) {
    if duty_percent < 100 {
        tokio::time::sleep(rest_after(worked, duty_percent)).await;
    }
}

/// How often, while busy, the log says how much is left.
const PROGRESS_EVERY: Duration = Duration::from_secs(60);

/// Long-running worker entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    let ollama = match OllamaClient::from_config(&config) {
        Ok(client) => {
            match client.as_ref().map(|c| c.model.as_deref()) {
                Some(Some(model)) => tracing::info!(
                    url = config.ollama_url.as_deref().unwrap_or(""),
                    model,
                    embed_model = %config.ollama_embed_model,
                    "extraction: Ollama enabled (AI reading + embeddings)"
                ),
                Some(None) => tracing::info!(
                    url = config.ollama_url.as_deref().unwrap_or(""),
                    embed_model = %config.ollama_embed_model,
                    "extraction: Ollama enabled (embeddings only)"
                ),
                None => tracing::info!("extraction: no AI model set up; reading with rules only"),
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
    match backlog(&pool).await {
        Ok(b) if b.chunks > 0 => tracing::info!(
            sections = b.chunks,
            files = b.files,
            "extraction: resuming; still to read"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "extraction: could not count the backlog"),
    }

    let idle = Duration::from_secs(config.extraction_interval_secs);
    let mut busy_since: Option<std::time::Instant> = None;
    let mut last_progress = std::time::Instant::now();
    loop {
        let stats = match run_one_pass(&pool, &config, ollama.as_ref()).await {
            Ok(stats) => stats,
            Err(e) => {
                tracing::error!(error = %e, "extraction pass failed");
                PassStats::default()
            }
        };
        if stats.did_work() {
            tracing::debug!(
                pdfs = stats.pdfs_processed,
                images = stats.images_processed,
                chunks = stats.chunks_processed,
                failed = stats.chunks_failed,
                units = stats.units_created,
                "extraction pass complete"
            );
            let started = *busy_since.get_or_insert_with(std::time::Instant::now);
            if last_progress.elapsed() >= PROGRESS_EVERY {
                last_progress = std::time::Instant::now();
                if let Ok(b) = backlog(&pool).await {
                    tracing::info!(
                        sections = b.chunks,
                        files = b.files,
                        set_aside = b.failed,
                        minutes = started.elapsed().as_secs() / 60,
                        "extraction: still reading"
                    );
                }
            }
            // More may be queued: go again. (The model's share of the time is
            // kept request by request, see `pace`.)
            tokio::time::sleep(BUSY_PAUSE).await;
        } else {
            if let Some(started) = busy_since.take() {
                let failed = backlog(&pool).await.map(|b| b.failed).unwrap_or(0);
                tracing::info!(
                    minutes = started.elapsed().as_secs() / 60,
                    set_aside = failed,
                    "extraction: everything queued has been read"
                );
            }
            tokio::time::sleep(idle).await;
        }
    }
}

/// What is still waiting to be read into units.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Backlog {
    /// Chunks (sections, messages, image text) not yet read.
    pub chunks: i64,
    /// Files with at least one chunk not yet read, or not yet opened.
    pub files: i64,
    /// Chunks set aside after an error.
    pub failed: i64,
}

/// Count the queue. Cheap: every count runs on a partial index.
pub async fn backlog(pool: &PgPool) -> Result<Backlog, sqlx::Error> {
    let row = sqlx::query(
        r#"
        WITH pending AS (
            SELECT d.artifact_id FROM document_segments s
              JOIN documents d ON d.id = s.document_id
             WHERE s.units_extracted_at IS NULL
            UNION ALL
            SELECT c.artifact_id FROM messages m
              JOIN conversations c ON c.id = m.conversation_id
             WHERE m.units_extracted_at IS NULL
            UNION ALL
            SELECT i.artifact_id FROM images i
             WHERE i.units_extracted_at IS NULL AND i.ocr_status = 'completed'
               AND length(trim(coalesce(i.ocr_text, ''))) > 0
        ),
        unopened AS (
            SELECT artifact_id FROM documents WHERE extraction_status IN ('pending', 'processing')
            UNION
            SELECT artifact_id FROM images WHERE ocr_status IN ('pending', 'processing')
        )
        SELECT (SELECT count(*) FROM pending p JOIN artifacts a ON a.id = p.artifact_id
                WHERE a.retracted_at IS NULL) AS chunks,
               (SELECT count(*) FROM (SELECT artifact_id FROM pending
                                      UNION SELECT artifact_id FROM unopened) f
                                      JOIN artifacts a ON a.id = f.artifact_id
                                      WHERE a.retracted_at IS NULL) AS files,
               (SELECT count(*) FROM document_segments WHERE units_extract_error IS NOT NULL)
             + (SELECT count(*) FROM messages WHERE units_extract_error IS NOT NULL)
             + (SELECT count(*) FROM images WHERE units_extract_error IS NOT NULL) AS failed
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(Backlog {
        chunks: row.get("chunks"),
        files: row.get("files"),
        failed: row.get("failed"),
    })
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
    // The tray keeps a bounded number of optional items: trim any beyond that,
    // including a longer queue left by an earlier version.
    match persist::trim_optional_review(pool).await {
        Ok(n) if n > 0 => tracing::info!(items = n, "review: trimmed optional items past the cap"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "review: could not trim optional items"),
    }
    // Each queue on its own: one failing never keeps the others from moving.
    let pdfs_processed = process_pending_pdfs(pool, config)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "extraction: opening PDFs failed; will retry");
            0
        });
    let images_processed = process_pending_images(pool, config)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "extraction: reading images failed; will retry");
            0
        });
    let chunks = process_unit_chunks(pool, config, ollama)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "extraction: reading sections failed; will retry");
            ChunkStats::default()
        });
    // Nothing new to read: spend the time going back over earlier files with
    // the AI model, if a re-read was asked for.
    let reread = match ollama {
        Some(client) if chunks.processed + chunks.failed == 0 => {
            reread::run_pass(pool, config, client)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, "extraction: re-reading failed; will retry");
                    ChunkStats::default()
                })
        }
        _ => ChunkStats::default(),
    };
    // Documents whose text is read get a digest: what they are about and the
    // sentences that say the most.
    let digests_built = digest_job::run_pass(pool, config, ollama)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "extraction: summarizing documents failed; will retry");
            0
        });
    let stats = PassStats {
        digests_built,
        pdfs_processed,
        images_processed,
        chunks_processed: chunks.processed + reread.processed,
        units_created: chunks.created + reread.created,
        chunks_failed: chunks.failed + reread.failed,
    };

    if let Some(client) = ollama {
        match persist::embed_pending_units(pool, client, config.extraction_batch).await {
            Ok(n) if n > 0 => tracing::debug!(units = n, "embedded pending claims"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "claim embedding failed; will retry"),
        }
        match persist::embed_pending_segments(pool, client, config.extraction_batch).await {
            Ok(n) if n > 0 => tracing::debug!(segments = n, "embedded document segments"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "segment embedding failed; will retry"),
        }
        // Entity embeddings feed the cosine pass of merge suggestions (§6.4).
        // Without this the column stays NULL, entities_embedding_hnsw indexes
        // nothing, and only the offline text pass ever fires.
        match crate::entities::embed_pending_entities(pool, client, config.extraction_batch).await {
            Ok(n) if n > 0 => tracing::debug!(entities = n, "embedded entities"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "entity embedding failed; will retry"),
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
              AND EXISTS (SELECT 1 FROM artifacts a WHERE a.id = d.artifact_id AND a.retracted_at IS NULL)
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
              AND EXISTS (SELECT 1 FROM artifacts a WHERE a.id = i.artifact_id AND a.retracted_at IS NULL)
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

#[derive(Debug, Default)]
pub struct ChunkStats {
    pub processed: usize,
    pub created: usize,
    pub failed: usize,
}

/// Errors are kept short: enough to tell what went wrong, not a whole
/// statement echoed back.
const MAX_ERROR_CHARS: usize = 500;

/// Set a chunk aside after `error`: stamped as read, so it leaves the queue
/// and its file stops showing as still being read, with the error kept for
/// the log and for a later retry.
async fn set_aside(pool: &PgPool, chunk: &Chunk, error: &str) -> Result<(), sqlx::Error> {
    let (sql, id) = match chunk.anchor {
        ChunkAnchor::Message(id) => (
            "UPDATE messages SET units_extracted_at = now(), units_extract_error = $2 \
             WHERE id = $1 AND units_extracted_at IS NULL",
            id,
        ),
        ChunkAnchor::Segment(id) => (
            "UPDATE document_segments SET units_extracted_at = now(), units_extract_error = $2 \
             WHERE id = $1 AND units_extracted_at IS NULL",
            id,
        ),
        ChunkAnchor::Image(id) => (
            "UPDATE images SET units_extracted_at = now(), units_extract_error = $2 \
             WHERE id = $1 AND units_extracted_at IS NULL",
            id,
        ),
    };
    let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
    sqlx::query(sql).bind(id).bind(error).execute(pool).await?;
    Ok(())
}

/// Which chunks to take: those not yet read, or those a re-read job should
/// still visit.
pub(crate) enum Queue<'a> {
    Fresh,
    Reread { job: Uuid, model: &'a str },
}

impl Queue<'_> {
    /// The condition on chunk table alias `t`, and the text column's name for
    /// the "has something to read" check on a re-read.
    fn filter(&self, t: &str, text: &str) -> String {
        match self {
            Queue::Fresh => format!("{t}.units_extracted_at IS NULL"),
            Queue::Reread { .. } => format!(
                "{t}.units_extracted_at IS NOT NULL \
                 AND {t}.units_llm_model IS DISTINCT FROM $2 \
                 AND {t}.units_reread_job IS DISTINCT FROM $3 \
                 AND length(trim(coalesce({t}.{text}, ''))) > 0"
            ),
        }
    }
}

/// The next `limit` chunks of each kind in `queue`.
pub(crate) async fn load_chunks(
    pool: &PgPool,
    limit: i64,
    queue: &Queue<'_>,
) -> anyhow::Result<Vec<Chunk>> {
    let mut chunks: Vec<Chunk> = Vec::new();
    // Bind $2/$3 only where the statement mentions them.
    macro_rules! run {
        ($sql:expr) => {{
            let q = sqlx::query(sqlx::AssertSqlSafe($sql)).bind(limit);
            match queue {
                Queue::Fresh => q.fetch_all(pool).await?,
                Queue::Reread { job, model } => q.bind(*model).bind(*job).fetch_all(pool).await?,
            }
        }};
    }

    for row in run!(format!(
        r#"
        SELECT m.id, m.content, m.role,
               COALESCE(m.created_at, a.source_created_at) AS source_time,
               c.artifact_id
        FROM messages m
        JOIN conversations c ON c.id = m.conversation_id
        JOIN artifacts a ON a.id = c.artifact_id
        WHERE a.retracted_at IS NULL AND {}
        ORDER BY a.ingested_at, m.conversation_id, m.id LIMIT $1
        "#,
        queue.filter("m", "content")
    )) {
        chunks.push(Chunk {
            anchor: ChunkAnchor::Message(row.get("id")),
            artifact_id: row.get("artifact_id"),
            text: row.get("content"),
            source_time: row.get("source_time"),
            user_authored: row.get::<String, _>("role") == "user",
            ocr_confidence: None,
        });
    }

    for row in run!(format!(
        r#"
        SELECT s.id, s.content,
               a.source_created_at AS source_time,
               d.artifact_id
        FROM document_segments s
        JOIN documents d ON d.id = s.document_id
        JOIN artifacts a ON a.id = d.artifact_id
        WHERE a.retracted_at IS NULL AND {}
        -- A file at a time, oldest first, so each finishes before the next
        -- starts instead of every file waiting for the end of the queue.
        ORDER BY a.ingested_at, s.document_id, s.seq LIMIT $1
        "#,
        queue.filter("s", "content")
    )) {
        chunks.push(Chunk {
            anchor: ChunkAnchor::Segment(row.get("id")),
            artifact_id: row.get("artifact_id"),
            text: row.get("content"),
            source_time: row.get("source_time"),
            user_authored: false,
            ocr_confidence: None,
        });
    }

    for row in run!(format!(
        r#"
        SELECT i.id, i.ocr_text, i.ocr_confidence,
               COALESCE(i.taken_at, a.source_created_at) AS source_time,
               i.artifact_id
        FROM images i
        JOIN artifacts a ON a.id = i.artifact_id
        WHERE a.retracted_at IS NULL AND {}
          AND i.ocr_status = 'completed'
          AND i.ocr_text IS NOT NULL AND length(trim(i.ocr_text)) > 0
        ORDER BY i.id LIMIT $1
        "#,
        queue.filter("i", "ocr_text")
    )) {
        chunks.push(Chunk {
            anchor: ChunkAnchor::Image(row.get("id")),
            artifact_id: row.get("artifact_id"),
            text: row.get("ocr_text"),
            source_time: row.get("source_time"),
            user_authored: false,
            ocr_confidence: row.get("ocr_confidence"),
        });
    }
    Ok(chunks)
}

/// The artifacts among `chunks` that are source-code files, whose comments
/// aren't read for statements (see `Config::extract_source_code`). Their
/// chunks are still stamped as read, so they leave the queue.
pub(crate) async fn source_code_artifacts(
    pool: &PgPool,
    config: &Config,
    chunks: &[Chunk],
) -> anyhow::Result<std::collections::HashSet<Uuid>> {
    if config.extract_source_code || chunks.is_empty() {
        return Ok(Default::default());
    }
    let ids: Vec<Uuid> = chunks.iter().map(|c| c.artifact_id).collect();
    let names: Vec<(Uuid, Option<String>)> =
        sqlx::query_as("SELECT id, original_filename FROM artifacts WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await?;
    Ok(names
        .into_iter()
        .filter(|(_, name)| name.as_deref().is_some_and(worth::is_source_code_file))
        .map(|(id, _)| id)
        .collect())
}

async fn process_unit_chunks(
    pool: &PgPool,
    config: &Config,
    ollama: Option<&OllamaClient>,
) -> anyhow::Result<ChunkStats> {
    let chunks = load_chunks(pool, config.extraction_batch, &Queue::Fresh).await?;

    // Admission thresholds for this pass: tuned values from feedback when
    // present, env config otherwise.
    let live = LiveThresholds::load(pool, config).await?;
    let mut stats = ChunkStats::default();
    let source_files = source_code_artifacts(pool, config, &chunks).await?;
    for chunk in &chunks {
        // Code, data tables and rows of figures aren't read for statements:
        // they match sentence shapes without saying anything. They are blanked
        // out (same length, so offsets still point into the chunk) and the
        // prose around them is read as usual. A chunk with no prose is still
        // stamped as read, so it isn't looked at again.
        let readable = if source_files.contains(&chunk.artifact_id) {
            None
        } else {
            worth::readable(&chunk.text)
        };
        let mut units: Vec<(rules::ExtractedUnit, &'static str, Option<String>)> = match &readable {
            Some(text) => rules::extract_units(text)
                .into_iter()
                .map(|u| (u, "rule_based", None))
                .collect(),
            None => Vec::new(),
        };
        let mut llm_model = None;

        if let (Some(text), Some((client, chat_model))) = (
            readable.as_deref(),
            ollama.and_then(|c| c.model.as_deref().map(|m| (c, m))),
        ) {
            let asked = std::time::Instant::now();
            let answer = client.extract(text).await;
            pace(asked.elapsed(), config.extraction_ai_duty_percent).await;
            match answer {
                Ok(llm_units) => {
                    let model = Some(format!("ollama:{chat_model}"));
                    llm_model = model.clone();
                    units.extend(
                        llm_units
                            .into_iter()
                            .map(|u| (u, "llm_local", model.clone())),
                    );
                }
                Err(e) => tracing::warn!(error = %e, "llm extraction failed; rule-based only"),
            }
        }

        match persist::persist_chunk_units(
            pool,
            chunk,
            &persist::Claim::Fresh { llm_model },
            &units,
            live.admit_hold_below,
            live.admit_drop_below,
        )
        .await
        {
            Ok(Some(outcome)) => {
                stats.processed += 1;
                stats.created += outcome.units_created;

                if let Some(client) = ollama {
                    if let Err(e) = persist::embed_new_units(pool, client, &outcome.new_units).await
                    {
                        tracing::warn!(error = %e, "unit embedding failed; will remain NULL");
                    }
                }
            }
            Ok(None) => {
                // Raced with another pass; nothing to do.
            }
            Err(e) => {
                // This chunk alone: set it aside and carry on with the rest,
                // rather than failing the pass and meeting it again first
                // next time.
                // The underlying cause: ApiError's own text is kept short
                // for API clients ("database error").
                let error = match &e {
                    crate::error::ApiError::Db(inner) => inner.to_string(),
                    crate::error::ApiError::Internal(inner) => format!("{inner:#}"),
                    other => other.to_string(),
                };
                tracing::warn!(
                    artifact_id = %chunk.artifact_id,
                    chunk = ?chunk.anchor,
                    error = %error,
                    "extraction: could not read a section; set aside"
                );
                set_aside(pool, chunk, &error).await?;
                stats.failed += 1;
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_keeps_the_worker_to_its_share_of_the_time() {
        let s = Duration::from_secs;
        assert_eq!(rest_after(s(3), 30), Duration::from_secs_f64(7.0));
        assert_eq!(rest_after(s(10), 50), s(10));
        assert_eq!(
            rest_after(s(10), 100),
            BUSY_PAUSE,
            "full speed: a breath only"
        );
        assert_eq!(rest_after(Duration::ZERO, 30), BUSY_PAUSE, "never shorter");
        assert_eq!(rest_after(s(60), 10), MAX_REST, "capped");
        // Out-of-range shares are pulled into 10-100.
        assert_eq!(rest_after(s(1), 0), rest_after(s(1), 10));
    }
}
