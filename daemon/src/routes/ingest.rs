//! Ingestion endpoints: chat exports, agent logs, and manual file uploads.
//!
//! All three paths converge on the same persistence rules:
//!   1. raw bytes are content-addressed (sha-256) and stored once (`artifacts`)
//!   2. modality-specific rows are created (`conversations`/`messages`,
//!      `documents`/`document_segments`, `images`)
//!   3. everything downstream (extraction, graph, contradiction scan) reads
//!      from those normalized tables and never re-parses platform formats.

use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::adapters::{self, NormalizedConversation};
use crate::error::ApiError;
use crate::AppState;

// ---------------------------------------------------------------------------
// Shared persistence helpers
// ---------------------------------------------------------------------------

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub struct StoredArtifact {
    pub id: Uuid,
    pub deduplicated: bool,
}

#[allow(clippy::too_many_arguments)]
async fn store_artifact(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    source_platform: &str,
    source_format_version: Option<&str>,
    original_filename: Option<&str>,
    media_type: Option<&str>,
    bytes: &[u8],
    source_created_at: Option<DateTime<Utc>>,
    job_id: Uuid,
    metadata: Value,
) -> Result<StoredArtifact, ApiError> {
    let hash = sha256_hex(bytes);

    let inserted: Option<(Uuid,)> = sqlx::query_as(
        r#"
        INSERT INTO artifacts
            (kind, source_platform, source_format_version, original_filename,
             media_type, byte_size, content_hash, raw_content,
             source_created_at, ingestion_job_id, metadata)
        VALUES ($1::artifact_kind, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (content_hash) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(kind)
    .bind(source_platform)
    .bind(source_format_version)
    .bind(original_filename)
    .bind(media_type)
    .bind(bytes.len() as i64)
    .bind(&hash)
    .bind(bytes)
    .bind(source_created_at)
    .bind(job_id)
    .bind(metadata)
    .fetch_optional(&mut **tx)
    .await?;

    match inserted {
        Some((id,)) => Ok(StoredArtifact {
            id,
            deduplicated: false,
        }),
        None => {
            let (id,): (Uuid,) = sqlx::query_as("SELECT id FROM artifacts WHERE content_hash = $1")
                .bind(&hash)
                .fetch_one(&mut **tx)
                .await?;
            Ok(StoredArtifact {
                id,
                deduplicated: true,
            })
        }
    }
}

async fn create_job(pool: &PgPool, source: &str) -> Result<Uuid, ApiError> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO ingestion_jobs (source, status) VALUES ($1, 'processing') RETURNING id",
    )
    .bind(source)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn finish_job(pool: &PgPool, job_id: Uuid, ok: bool, stats: Value) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        UPDATE ingestion_jobs
        SET status = $2::ingestion_status, finished_at = now(), stats = $3
        WHERE id = $1
        "#,
    )
    .bind(job_id)
    .bind(if ok { "completed" } else { "partial" })
    .bind(stats)
    .execute(pool)
    .await?;
    Ok(())
}

async fn persist_conversations(
    tx: &mut Transaction<'_, Postgres>,
    artifact_id: Uuid,
    source_platform: &str,
    conversations: &[NormalizedConversation],
) -> Result<(usize, usize), ApiError> {
    let mut message_count = 0usize;
    for conv in conversations {
        let (conversation_id,): (Uuid,) = sqlx::query_as(
            r#"
            INSERT INTO conversations
                (artifact_id, external_id, title, source_platform, model, started_at, ended_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING id
            "#,
        )
        .bind(artifact_id)
        .bind(&conv.external_id)
        .bind(&conv.title)
        .bind(source_platform)
        .bind(&conv.model)
        .bind(conv.started_at)
        .bind(conv.ended_at)
        .fetch_one(&mut **tx)
        .await?;

        // Messages are linearized parent-before-child, so a single pass can
        // resolve parent_message_id from previously inserted external ids.
        let mut id_by_external: std::collections::HashMap<String, Uuid> =
            std::collections::HashMap::new();
        for (seq, msg) in conv.messages.iter().enumerate() {
            let parent_id = msg
                .parent_external_id
                .as_ref()
                .and_then(|ext| id_by_external.get(ext))
                .copied();
            let (message_id,): (Uuid,) = sqlx::query_as(
                r#"
                INSERT INTO messages
                    (conversation_id, external_id, parent_message_id, seq,
                     role, author, model, content, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                RETURNING id
                "#,
            )
            .bind(conversation_id)
            .bind(&msg.external_id)
            .bind(parent_id)
            .bind(seq as i32)
            .bind(&msg.role)
            .bind(&msg.author)
            .bind(&msg.model)
            .bind(&msg.content)
            .bind(msg.created_at)
            .fetch_one(&mut **tx)
            .await?;
            if let Some(ext) = &msg.external_id {
                id_by_external.insert(ext.clone(), message_id);
            }
            message_count += 1;
        }
    }
    Ok((conversations.len(), message_count))
}

// ---------------------------------------------------------------------------
// POST /api/v1/ingest/chat-export
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ChatExportRequest {
    /// One of: chatgpt | claude | generic (dedicated adapters), or any other
    /// platform name pre-converted to the gather-generic-v1 schema.
    pub platform: String,
    /// The raw export payload (e.g. the parsed contents of conversations.json).
    pub data: Value,
    pub filename: Option<String>,
}

#[derive(Serialize)]
pub struct ChatExportResponse {
    pub job_id: Uuid,
    pub artifact_id: Uuid,
    pub deduplicated: bool,
    pub conversations: usize,
    pub messages: usize,
}

pub async fn ingest_chat_export(
    State(state): State<AppState>,
    Json(req): Json<ChatExportRequest>,
) -> Result<(StatusCode, Json<ChatExportResponse>), ApiError> {
    let normalized = adapters::normalize(&req.platform, &req.data)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let raw = serde_json::to_vec(&req.data).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let job_id = create_job(&state.pool, "rest").await?;

    let mut tx = state.pool.begin().await?;
    let stored = store_artifact(
        &mut tx,
        "chat_export",
        &req.platform,
        Some(normalized.source_format_version),
        req.filename.as_deref(),
        Some("application/json"),
        &raw,
        None,
        job_id,
        json!({}),
    )
    .await?;

    let (conv_count, msg_count) = if stored.deduplicated {
        (0, 0) // identical export already ingested; nothing new to normalize
    } else {
        persist_conversations(&mut tx, stored.id, &req.platform, &normalized.conversations).await?
    };
    tx.commit().await?;

    finish_job(
        &state.pool,
        job_id,
        true,
        json!({"conversations": conv_count, "messages": msg_count, "deduplicated": stored.deduplicated}),
    )
    .await?;

    metrics::counter!("gather_ingest_artifacts_total", "kind" => "chat_export").increment(1);
    metrics::counter!("gather_ingest_messages_total", "platform" => req.platform.clone())
        .increment(msg_count as u64);

    Ok((
        StatusCode::ACCEPTED,
        Json(ChatExportResponse {
            job_id,
            artifact_id: stored.id,
            deduplicated: stored.deduplicated,
            conversations: conv_count,
            messages: msg_count,
        }),
    ))
}

// ---------------------------------------------------------------------------
// POST /api/v1/ingest/agent-log
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AgentLogRequest {
    /// e.g. claude_code | goose | aider | generic — recorded as source_platform.
    pub platform: String,
    /// Raw JSONL session log: one JSON object per line with role/content
    /// fields (Claude Code, Goose and Aider session formats all satisfy this).
    pub jsonl: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
}

pub async fn ingest_agent_log(
    State(state): State<AppState>,
    Json(req): Json<AgentLogRequest>,
) -> Result<(StatusCode, Json<ChatExportResponse>), ApiError> {
    // Convert JSONL lines to the gather-generic-v1 shape, then reuse the
    // generic adapter so agent logs and chat exports share one code path.
    let mut messages = Vec::new();
    for (lineno, line) in req.jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| {
            ApiError::BadRequest(format!("invalid JSONL at line {}: {e}", lineno + 1))
        })?;
        let role = v
            .get("role")
            .or_else(|| v.get("type"))
            .or_else(|| v.get("sender"))
            .and_then(Value::as_str)
            .unwrap_or("other");
        let content = extract_log_content(&v);
        let Some(content) = content else { continue };
        messages.push(json!({
            "role": role,
            "content": content,
            "created_at": v.get("timestamp").or_else(|| v.get("created_at")).cloned(),
        }));
    }

    let generic = json!({
        "schema": "gather-generic-v1",
        "conversations": [{
            "id": req.session_id,
            "title": req.title,
            "messages": messages,
        }]
    });

    let normalized = adapters::normalize("generic", &generic)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let job_id = create_job(&state.pool, "rest").await?;
    let mut tx = state.pool.begin().await?;
    let stored = store_artifact(
        &mut tx,
        "agent_log",
        &req.platform,
        Some("agent-jsonl-v1"),
        None,
        Some("application/x-ndjson"),
        req.jsonl.as_bytes(),
        None,
        job_id,
        json!({"session_id": req.session_id}),
    )
    .await?;

    let (conv_count, msg_count) = if stored.deduplicated {
        (0, 0)
    } else {
        persist_conversations(&mut tx, stored.id, &req.platform, &normalized.conversations).await?
    };
    tx.commit().await?;

    finish_job(
        &state.pool,
        job_id,
        true,
        json!({"conversations": conv_count, "messages": msg_count, "deduplicated": stored.deduplicated}),
    )
    .await?;

    metrics::counter!("gather_ingest_artifacts_total", "kind" => "agent_log").increment(1);
    metrics::counter!("gather_ingest_messages_total", "platform" => req.platform.clone())
        .increment(msg_count as u64);

    Ok((
        StatusCode::ACCEPTED,
        Json(ChatExportResponse {
            job_id,
            artifact_id: stored.id,
            deduplicated: stored.deduplicated,
            conversations: conv_count,
            messages: msg_count,
        }),
    ))
}

/// Agent-log lines store content as a string, a {text} object, or an array of
/// content blocks; accept all three.
fn extract_log_content(v: &Value) -> Option<String> {
    let content = v.get("content").or_else(|| v.get("text"))?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                b.as_str()
                    .map(String::from)
                    .or_else(|| b.get("text").and_then(Value::as_str).map(String::from))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => content.get("text").and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ingest/files  (multipart: drag-and-drop / file picker uploads)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct FileResult {
    pub filename: String,
    pub kind: Option<String>,
    pub artifact_id: Option<Uuid>,
    pub deduplicated: bool,
    pub status: String, // accepted | deduplicated | rejected
    pub detail: Option<String>,
    pub segments: usize,
}

#[derive(Serialize)]
pub struct FilesResponse {
    pub job_id: Uuid,
    pub files: Vec<FileResult>,
}

/// Multipart contract (matches the Tauri UI):
///   - each file is a part named `file` (kind auto-detected from
///     content-type + extension), or named with an explicit artifact kind
///     (`document_pdf`, `document_markdown`, `document_text`, `image_photo`,
///     `image_screenshot`) to override detection.
pub async fn ingest_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FilesResponse>), ApiError> {
    let job_id = create_job(&state.pool, "rest").await?;
    let mut results: Vec<FileResult> = Vec::new();
    let mut all_ok = true;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed multipart body: {e}")))?
    {
        let part_name = field.name().unwrap_or("file").to_string();
        let filename = field
            .file_name()
            .map(String::from)
            .unwrap_or_else(|| "unnamed".to_string());
        let declared_type = field.content_type().map(String::from);
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ApiError::BadRequest(format!("failed reading part '{part_name}': {e}")))?;

        match ingest_one_file(&state, job_id, &part_name, &filename, declared_type, &bytes).await {
            Ok(result) => {
                metrics::counter!(
                    "gather_ingest_files_total",
                    "kind" => result.kind.clone().unwrap_or_else(|| "unknown".into()),
                    "status" => result.status.clone()
                )
                .increment(1);
                results.push(result);
            }
            Err(e) => {
                all_ok = false;
                metrics::counter!(
                    "gather_ingest_files_total",
                    "kind" => "unknown",
                    "status" => "rejected"
                )
                .increment(1);
                results.push(FileResult {
                    filename,
                    kind: None,
                    artifact_id: None,
                    deduplicated: false,
                    status: "rejected".to_string(),
                    detail: Some(e.to_string()),
                    segments: 0,
                });
            }
        }
    }

    if results.is_empty() {
        finish_job(&state.pool, job_id, false, json!({"files": 0})).await?;
        return Err(ApiError::BadRequest(
            "multipart body contained no file parts".to_string(),
        ));
    }

    finish_job(
        &state.pool,
        job_id,
        all_ok,
        json!({
            "files": results.len(),
            "accepted": results.iter().filter(|r| r.status != "rejected").count(),
            "rejected": results.iter().filter(|r| r.status == "rejected").count(),
        }),
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(FilesResponse {
            job_id,
            files: results,
        }),
    ))
}

const EXPLICIT_KINDS: &[&str] = &[
    "document_pdf",
    "document_markdown",
    "document_text",
    "image_photo",
    "image_screenshot",
];

fn classify(
    part_name: &str,
    filename: &str,
    declared_type: Option<&str>,
) -> Result<String, ApiError> {
    if EXPLICIT_KINDS.contains(&part_name) {
        return Ok(part_name.to_string());
    }
    let guessed = declared_type
        .map(String::from)
        .filter(|t| t != "application/octet-stream")
        .unwrap_or_else(|| {
            mime_guess::from_path(filename)
                .first_or_octet_stream()
                .essence_str()
                .to_string()
        });
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    let kind = match guessed.as_str() {
        "application/pdf" => "document_pdf",
        "text/markdown" => "document_markdown",
        "text/plain" if ext == "md" || ext == "markdown" => "document_markdown",
        "text/plain" => "document_text",
        // Heuristic: PNGs are overwhelmingly screenshots at personal scale,
        // camera formats are photos. Callers can override via the part name.
        "image/png" => "image_screenshot",
        "image/jpeg" | "image/heic" | "image/heif" | "image/webp" | "image/tiff" => "image_photo",
        other => {
            return Err(ApiError::UnsupportedMedia(format!(
                "{other} (supported: pdf, markdown, plain text, png, jpeg, heic, webp, tiff)"
            )))
        }
    };
    Ok(kind.to_string())
}

async fn ingest_one_file(
    state: &AppState,
    job_id: Uuid,
    part_name: &str,
    filename: &str,
    declared_type: Option<String>,
    bytes: &[u8],
) -> Result<FileResult, ApiError> {
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("empty file".to_string()));
    }
    let kind = classify(part_name, filename, declared_type.as_deref())?;
    let media_type = declared_type.unwrap_or_else(|| {
        mime_guess::from_path(filename)
            .first_or_octet_stream()
            .essence_str()
            .to_string()
    });

    let mut tx = state.pool.begin().await?;
    let stored = store_artifact(
        &mut tx,
        &kind,
        "manual",
        None,
        Some(filename),
        Some(&media_type),
        bytes,
        None,
        job_id,
        json!({}),
    )
    .await?;

    let mut segments = 0usize;
    if !stored.deduplicated {
        match kind.as_str() {
            // Markdown / plain text: extraction is pure UTF-8 decoding, so it
            // completes synchronously at ingest time, including segmentation.
            "document_markdown" | "document_text" => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                let (document_id,): (Uuid,) = sqlx::query_as(
                    r#"
                    INSERT INTO documents
                        (artifact_id, extracted_text, extraction_tool,
                         extraction_status, extracted_at)
                    VALUES ($1, $2, 'utf8-passthrough', 'completed', now())
                    RETURNING id
                    "#,
                )
                .bind(stored.id)
                .bind(&text)
                .fetch_one(&mut *tx)
                .await?;

                for (seq, seg) in segment_text(&text).into_iter().enumerate() {
                    sqlx::query(
                        r#"
                        INSERT INTO document_segments
                            (document_id, seq, heading, content, content_hash)
                        VALUES ($1, $2, $3, $4, $5)
                        "#,
                    )
                    .bind(document_id)
                    .bind(seq as i32)
                    .bind(&seg.heading)
                    .bind(&seg.content)
                    .bind(sha256_hex(seg.content.as_bytes()))
                    .execute(&mut *tx)
                    .await?;
                    segments += 1;
                }
                metrics::counter!("gather_extraction_segments_total", "tool" => "utf8-passthrough")
                    .increment(segments as u64);
            }
            // PDFs and images: the stored artifact is complete; text/OCR
            // extraction runs in the pipeline workers (pdfium / tesseract),
            // which poll for rows in 'pending' state.
            "document_pdf" => {
                sqlx::query(
                    "INSERT INTO documents (artifact_id, extraction_status) VALUES ($1, 'pending')",
                )
                .bind(stored.id)
                .execute(&mut *tx)
                .await?;
            }
            "image_photo" | "image_screenshot" => {
                sqlx::query("INSERT INTO images (artifact_id, ocr_status) VALUES ($1, 'pending')")
                    .bind(stored.id)
                    .execute(&mut *tx)
                    .await?;
            }
            other => {
                return Err(ApiError::UnsupportedMedia(other.to_string()));
            }
        }
        metrics::counter!("gather_ingest_artifacts_total", "kind" => kind.clone()).increment(1);
    }
    tx.commit().await?;

    Ok(FileResult {
        filename: filename.to_string(),
        kind: Some(kind),
        artifact_id: Some(stored.id),
        deduplicated: stored.deduplicated,
        status: if stored.deduplicated {
            "deduplicated".to_string()
        } else {
            "accepted".to_string()
        },
        detail: None,
        segments,
    })
}

// ---------------------------------------------------------------------------
// Markdown / plain-text segmentation
// ---------------------------------------------------------------------------

pub struct Segment {
    pub heading: Option<String>,
    pub content: String,
}

const MAX_SEGMENT_CHARS: usize = 2000;

/// Deterministic segmentation: split on markdown headings, then split any
/// oversized section on paragraph boundaries so each segment stays under
/// MAX_SEGMENT_CHARS (embedding-friendly and provenance-precise).
pub fn segment_text(text: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current: Vec<&str> = Vec::new();

    let flush = |heading: &Option<String>, lines: &mut Vec<&str>, segments: &mut Vec<Segment>| {
        let body = lines.join("\n").trim().to_string();
        lines.clear();
        if body.is_empty() {
            return;
        }
        for chunk in split_paragraph_chunks(&body) {
            segments.push(Segment {
                heading: heading.clone(),
                content: chunk,
            });
        }
    };

    for line in text.lines() {
        if let Some(heading) = parse_heading(line) {
            flush(&current_heading, &mut current, &mut segments);
            current_heading = Some(heading);
        } else {
            current.push(line);
        }
    }
    flush(&current_heading, &mut current, &mut segments);
    segments
}

fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
        Some(trimmed[hashes + 1..].trim().to_string())
    } else {
        None
    }
}

fn split_paragraph_chunks(body: &str) -> Vec<String> {
    if body.len() <= MAX_SEGMENT_CHARS {
        return vec![body.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for para in body.split("\n\n") {
        if !current.is_empty() && current.len() + para.len() + 2 > MAX_SEGMENT_CHARS {
            chunks.push(current.trim().to_string());
            current = String::new();
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
        // A single paragraph larger than the cap is split on char boundaries.
        while current.len() > MAX_SEGMENT_CHARS {
            let mut cut = MAX_SEGMENT_CHARS;
            while !current.is_char_boundary(cut) {
                cut -= 1;
            }
            let rest = current.split_off(cut);
            chunks.push(current.trim().to_string());
            current = rest;
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_markdown_by_heading() {
        let md = "# Intro\nHello world.\n\n## Details\nMore text here.\n";
        let segs = segment_text(md);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].heading.as_deref(), Some("Intro"));
        assert_eq!(segs[0].content, "Hello world.");
        assert_eq!(segs[1].heading.as_deref(), Some("Details"));
    }

    #[test]
    fn splits_oversized_sections() {
        let long_para = "word ".repeat(1000); // ~5000 chars, single paragraph
        let segs = segment_text(&long_para);
        assert!(segs.len() >= 3);
        assert!(segs.iter().all(|s| s.content.len() <= MAX_SEGMENT_CHARS));
    }

    #[test]
    fn classify_detects_kinds() {
        assert_eq!(
            classify("file", "notes.md", None).unwrap(),
            "document_markdown"
        );
        assert_eq!(
            classify("file", "paper.pdf", Some("application/pdf")).unwrap(),
            "document_pdf"
        );
        assert_eq!(
            classify("file", "shot.png", Some("image/png")).unwrap(),
            "image_screenshot"
        );
        assert_eq!(
            classify("file", "pic.jpg", Some("image/jpeg")).unwrap(),
            "image_photo"
        );
        // explicit override wins over detection
        assert_eq!(
            classify("image_photo", "shot.png", Some("image/png")).unwrap(),
            "image_photo"
        );
        assert!(classify("file", "archive.zip", Some("application/zip")).is_err());
    }
}
