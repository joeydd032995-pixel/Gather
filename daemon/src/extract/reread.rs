//! Re-reading earlier files with the AI model.
//!
//! A chat model that is switched on later only reads what is imported after
//! that. A re-read job goes back over everything the model has not read yet:
//! it walks those chunks a batch at a time, in the spare time of the worker
//! (only when nothing new is waiting, and at the same reading speed), and
//! adds the units only the model finds. Rules are not run again, and a unit
//! a chunk already backs is left as it is.

use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::ollama::OllamaClient;
use super::persist::{self, Chunk, ChunkAnchor, Claim};
use super::{load_chunks, ChunkStats, Queue};
use crate::config::Config;
use crate::decide::live::LiveThresholds;

/// A re-read job, as the app shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RereadJob {
    pub id: Uuid,
    /// The chat model reading, e.g. `smollm2:360m`.
    pub model: String,
    /// `running`, `done` or `cancelled`.
    pub status: String,
    /// Chunks to read when the job started.
    pub total: i64,
    /// Chunks the model has read.
    pub done: i64,
    /// Chunks the model could not read.
    pub failed: i64,
}

fn job_from(row: &sqlx::postgres::PgRow) -> RereadJob {
    RereadJob {
        id: row.get("id"),
        model: row.get("model"),
        status: row.get("status"),
        total: row.get("total"),
        done: row.get("done"),
        failed: row.get("failed"),
    }
}

/// The value stored on a chunk the model has read (as on its units).
fn stored_model(model: &str) -> String {
    format!("ollama:{model}")
}

/// How many chunks `model` has not read yet.
pub async fn unread_by(pool: &PgPool, model: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT (SELECT count(*) FROM messages
                 WHERE units_extracted_at IS NOT NULL
                   AND units_llm_model IS DISTINCT FROM $1
                   AND length(trim(coalesce(content, ''))) > 0)
             + (SELECT count(*) FROM document_segments
                 WHERE units_extracted_at IS NOT NULL
                   AND units_llm_model IS DISTINCT FROM $1
                   AND length(trim(coalesce(content, ''))) > 0)
             + (SELECT count(*) FROM images
                 WHERE units_extracted_at IS NOT NULL
                   AND units_llm_model IS DISTINCT FROM $1
                   AND ocr_status = 'completed'
                   AND length(trim(coalesce(ocr_text, ''))) > 0)
        "#,
    )
    .bind(stored_model(model))
    .fetch_one(pool)
    .await
}

/// Start a job for `model`, or return the one already running. None when
/// there is nothing for the model to read.
pub async fn start(pool: &PgPool, model: &str) -> Result<Option<RereadJob>, sqlx::Error> {
    if let Some(job) = running(pool).await? {
        return Ok(Some(job));
    }
    let total = unread_by(pool, model).await?;
    if total == 0 {
        return Ok(None);
    }
    // The unique index allows one running job: a second start racing this one
    // gets that one back.
    let row = sqlx::query(
        "INSERT INTO reread_jobs (model, total) VALUES ($1, $2)
         ON CONFLICT DO NOTHING
         RETURNING id, model, status, total, done, failed",
    )
    .bind(model)
    .bind(total)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(row) => {
            tracing::info!(model, chunks = total, "re-read: started");
            Ok(Some(job_from(&row)))
        }
        None => running(pool).await,
    }
}

/// Stop the running job, if any. What it has read stays read.
pub async fn cancel(pool: &PgPool) -> Result<Option<RereadJob>, sqlx::Error> {
    let row = sqlx::query(
        "UPDATE reread_jobs SET status = 'cancelled', finished_at = now()
         WHERE status = 'running'
         RETURNING id, model, status, total, done, failed",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| job_from(&r)))
}

pub async fn running(pool: &PgPool) -> Result<Option<RereadJob>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, model, status, total, done, failed FROM reread_jobs WHERE status = 'running'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| job_from(&r)))
}

/// The running job, or else the latest finished one.
pub async fn latest(pool: &PgPool) -> Result<Option<RereadJob>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, model, status, total, done, failed FROM reread_jobs
         ORDER BY (status = 'running') DESC, created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| job_from(&r)))
}

/// Leave the chunk for this job to skip: the model failed on it, and a
/// second try within the job would fail the same way.
async fn mark_visited(pool: &PgPool, chunk: &Chunk, job: Uuid) -> Result<(), sqlx::Error> {
    let (sql, id) = match chunk.anchor {
        ChunkAnchor::Message(id) => (
            "UPDATE messages SET units_reread_job = $2 WHERE id = $1",
            id,
        ),
        ChunkAnchor::Segment(id) => (
            "UPDATE document_segments SET units_reread_job = $2 WHERE id = $1",
            id,
        ),
        ChunkAnchor::Image(id) => ("UPDATE images SET units_reread_job = $2 WHERE id = $1", id),
    };
    sqlx::query(sql).bind(id).bind(job).execute(pool).await?;
    Ok(())
}

/// One batch of the running job (nothing when there is none). Public so
/// integration tests can drive it without the rest of a pass.
pub async fn run_pass(
    pool: &PgPool,
    config: &Config,
    client: &OllamaClient,
) -> anyhow::Result<ChunkStats> {
    let mut stats = ChunkStats::default();
    let Some(job) = running(pool).await? else {
        return Ok(stats);
    };
    let Some(model) = client.model.as_deref() else {
        return Ok(stats);
    };
    if job.model != model {
        // The model was changed since: this job belongs to the old one.
        tracing::info!(
            job_model = %job.model, model,
            "re-read: the reading model changed; stopped"
        );
        cancel(pool).await?;
        return Ok(stats);
    }

    let queue = Queue::Reread {
        job: job.id,
        model: &stored_model(model),
    };
    let chunks = load_chunks(pool, config.extraction_batch, &queue).await?;
    if chunks.is_empty() {
        sqlx::query(
            "UPDATE reread_jobs SET status = 'done', finished_at = now()
             WHERE id = $1 AND status = 'running'",
        )
        .bind(job.id)
        .execute(pool)
        .await?;
        tracing::info!(
            model,
            read = job.done,
            could_not_read = job.failed,
            "re-read: finished"
        );
        return Ok(stats);
    }

    let live = LiveThresholds::load(pool, config).await?;
    let claim = Claim::Reread {
        job: job.id,
        model: stored_model(model),
    };
    let stored = stored_model(model);
    for chunk in &chunks {
        let asked = std::time::Instant::now();
        let answer = client.extract(&chunk.text).await;
        super::pace(asked.elapsed(), config.extraction_ai_duty_percent).await;
        let units = match answer {
            Ok(found) => found
                .into_iter()
                .map(|u| (u, "llm_local", Some(stored.clone())))
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(error = %e, "re-read: the model could not read a section");
                mark_visited(pool, chunk, job.id).await?;
                stats.failed += 1;
                continue;
            }
        };
        // Rules ran when the chunk was first read: only the model's units.
        match persist::persist_chunk_units(
            pool,
            chunk,
            &claim,
            &units,
            live.admit_hold_below,
            live.admit_drop_below,
        )
        .await
        {
            Ok(Some(outcome)) => {
                stats.processed += 1;
                stats.created += outcome.units_created;
                if let Err(e) = persist::embed_new_units(pool, client, &outcome.new_units).await {
                    tracing::warn!(error = %e, "unit embedding failed; will remain NULL");
                }
            }
            Ok(None) => {} // raced: already read
            Err(e) => {
                tracing::warn!(chunk = ?chunk.anchor, error = %e, "re-read: could not save a section");
                mark_visited(pool, chunk, job.id).await?;
                stats.failed += 1;
            }
        }
    }

    sqlx::query("UPDATE reread_jobs SET done = done + $2, failed = failed + $3 WHERE id = $1")
        .bind(job.id)
        .bind(stats.processed as i64)
        .bind(stats.failed as i64)
        .execute(pool)
        .await?;
    Ok(stats)
}
