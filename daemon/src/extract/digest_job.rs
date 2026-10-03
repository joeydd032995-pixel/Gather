//! Building and storing document digests: the worker stage that follows
//! reading. The digest itself is pure (`digest.rs`); this finds documents
//! that have none, builds one, stores it, and, when a local model is on,
//! asks it to reword the result and keeps only what the text supports.

use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::digest::{self, Digest, Section};
use super::ollama::OllamaClient;
use crate::config::Config;

/// Documents digested per pass: a digest is quick, but a model call is not.
const BATCH: i64 = 5;
/// Most sections read for one document.
const MAX_SECTIONS: i64 = 1500;
/// How much of a model's wording must come from the document to be kept.
const SUMMARY_GROUNDING: f32 = 0.4;
const TAKEAWAY_GROUNDING: f32 = 0.5;

/// Digest the next few documents that don't have one. Returns how many.
pub async fn run_pass(
    pool: &PgPool,
    config: &Config,
    ollama: Option<&OllamaClient>,
) -> anyhow::Result<usize> {
    let rows = sqlx::query(
        "SELECT d.id AS document_id, d.artifact_id
           FROM documents d
           JOIN artifacts a ON a.id = d.artifact_id
          WHERE d.extraction_status = 'completed'
            AND a.retracted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM document_digests g WHERE g.artifact_id = d.artifact_id)
            AND EXISTS (SELECT 1 FROM document_segments s WHERE s.document_id = d.id)
          ORDER BY d.extracted_at NULLS LAST, d.artifact_id
          LIMIT $1",
    )
    .bind(BATCH)
    .fetch_all(pool)
    .await?;

    let mut built = 0usize;
    for row in rows {
        let document_id: Uuid = row.get("document_id");
        let artifact_id: Uuid = row.get("artifact_id");
        match digest_document(pool, config, ollama, document_id, artifact_id).await {
            Ok(()) => built += 1,
            Err(e) => {
                // Leave it for the next pass rather than failing the others.
                tracing::warn!(%artifact_id, error = %e, "digest: could not summarize a document");
            }
        }
    }
    Ok(built)
}

async fn digest_document(
    pool: &PgPool,
    config: &Config,
    ollama: Option<&OllamaClient>,
    document_id: Uuid,
    artifact_id: Uuid,
) -> anyhow::Result<()> {
    let segments = sqlx::query(
        "SELECT seq, heading, content FROM document_segments
          WHERE document_id = $1 ORDER BY seq LIMIT $2",
    )
    .bind(document_id)
    .bind(MAX_SECTIONS)
    .fetch_all(pool)
    .await?;
    let owned: Vec<(i32, Option<String>, String)> = segments
        .iter()
        .map(|r| (r.get("seq"), r.get("heading"), r.get("content")))
        .collect();

    // Reading a long document takes a moment of CPU: off the async workers.
    let built: Digest = tokio::task::spawn_blocking(move || {
        let sections: Vec<Section> = owned
            .iter()
            .map(|(seq, heading, text)| Section {
                seq: *seq,
                heading: heading.as_deref(),
                text,
            })
            .collect();
        digest::build(&sections)
    })
    .await?;

    // A document with nothing to say still gets a row, so it isn't looked at
    // again on every pass.
    sqlx::query(
        "INSERT INTO document_digests
             (artifact_id, method, summary, key_points, topics, outline, stats)
         VALUES ($1, 'extractive', $2, $3, $4, $5, $6)
         ON CONFLICT (artifact_id) DO NOTHING",
    )
    .bind(artifact_id)
    .bind(&built.summary)
    .bind(json!(built.key_points))
    .bind(json!(built.topics))
    .bind(json!(built.outline))
    .bind(json!(built.stats))
    .execute(pool)
    .await?;
    metrics::counter!("gather_digests_total", "method" => "extractive").increment(1);

    if built.is_thin() {
        return Ok(());
    }
    let Some((client, model)) = ollama.and_then(|c| c.model.as_deref().map(|m| (c, m))) else {
        return Ok(());
    };
    let material = digest::material_for_model(&built);
    let asked = std::time::Instant::now();
    let answer = client.digest(&material).await;
    super::pace(asked.elapsed(), config.extraction_ai_duty_percent).await;
    match answer {
        Ok(model_digest) => {
            // The model rewords; the text decides what is kept.
            let summary =
                digest::keep_grounded(vec![model_digest.summary], &material, SUMMARY_GROUNDING)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
            let takeaways =
                digest::keep_grounded(model_digest.takeaways, &material, TAKEAWAY_GROUNDING);
            let open_questions =
                digest::keep_grounded(model_digest.open_questions, &material, TAKEAWAY_GROUNDING);
            if summary.is_empty() && takeaways.is_empty() && open_questions.is_empty() {
                return Ok(()); // nothing the text supports: the plain digest stands
            }
            sqlx::query(
                "UPDATE document_digests
                    SET method = $2,
                        summary = CASE WHEN $3 = '' THEN summary ELSE $3 END,
                        takeaways = $4, open_questions = $5, updated_at = now()
                  WHERE artifact_id = $1 AND method = 'extractive'",
            )
            .bind(artifact_id)
            .bind(format!("llm:{model}"))
            .bind(summary)
            .bind(json!(takeaways))
            .bind(json!(open_questions))
            .execute(pool)
            .await?;
            metrics::counter!("gather_digests_total", "method" => "llm").increment(1);
        }
        Err(e) => tracing::warn!(%artifact_id, error = %e, "digest: the model could not reword it"),
    }
    Ok(())
}
