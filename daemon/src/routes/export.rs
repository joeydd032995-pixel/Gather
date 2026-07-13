//! Portable export / import: the `gather-bundle-v1` NDJSON format.
//!
//! Each line is `{"type": "<table>", "row": {<row as JSON>}}`. Rows are
//! serialized by Postgres itself (`row_to_json`) and re-hydrated with
//! `jsonb_populate_record`, so the bundle round-trips every column —
//! including pgvector embeddings and bytea raw content — without bespoke
//! (de)serializers per table. Generated columns (tsvectors) are excluded on
//! import; Postgres recomputes them.
//!
//! This is the exact payload the optional VPS replication encrypts and ships:
//! `GET /api/v1/export` -> age/restic encryption -> rsync/SSH (see write-up §7).

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use sqlx::Row;

use crate::error::ApiError;
use crate::AppState;

/// Tables in FK-dependency order, with the explicit (non-generated) column
/// list used for import. Export order == import order.
const TABLES: &[(&str, &str)] = &[
    (
        "ingestion_jobs",
        "id, source, status, started_at, finished_at, stats, error",
    ),
    (
        "artifacts",
        "id, kind, source_platform, source_format_version, original_filename, \
         media_type, byte_size, content_hash, raw_content, storage_path, version, \
         supersedes_artifact_id, source_created_at, ingested_at, ingestion_job_id, metadata",
    ),
    (
        "conversations",
        "id, artifact_id, external_id, title, source_platform, model, started_at, \
         ended_at, metadata",
    ),
    (
        "messages",
        "id, conversation_id, external_id, parent_message_id, seq, role, author, \
         model, content, created_at, metadata, units_extracted_at",
    ),
    (
        "documents",
        "id, artifact_id, page_count, language, extracted_text, extraction_tool, \
         extraction_status, extracted_at, metadata",
    ),
    (
        "document_segments",
        "id, document_id, seq, page, heading, content, content_hash, embedding, metadata, \
         units_extracted_at",
    ),
    (
        "images",
        "id, artifact_id, width, height, exif, taken_at, ocr_text, ocr_confidence, \
         ocr_status, caption, caption_model, metadata, units_extracted_at",
    ),
    (
        "entities",
        "id, name, kind, description, merged_into_entity_id, embedding, metadata, \
         created_at, updated_at",
    ),
    ("entity_aliases", "id, entity_id, alias"),
    (
        "atomic_units",
        "id, kind, statement, statement_hash, subject_entity_id, confidence, \
         extraction_method, extraction_model, embedding, valid_from, valid_to, \
         status, superseded_by_unit_id, attrs, created_at, updated_at, \
         contradiction_scanned_at",
    ),
    (
        "atomic_unit_provenance",
        "id, atomic_unit_id, artifact_id, message_id, document_segment_id, image_id, \
         char_start, char_end, quote, created_at",
    ),
    (
        "relationships",
        "id, source_entity_id, target_entity_id, relation_type, atomic_unit_id, \
         confidence, valid_from, valid_to, status, metadata, created_at, updated_at",
    ),
    (
        "contradictions",
        "id, unit_a_id, unit_b_id, score, detection_method, explanation, status, \
         detected_at, resolved_at, resolved_by, resolution_note",
    ),
    (
        "contradiction_audit",
        "id, contradiction_id, action, actor, from_status, to_status, note, created_at",
    ),
];

// ---------------------------------------------------------------------------
// GET /api/v1/export
// ---------------------------------------------------------------------------

pub async fn export_bundle(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let mut out = String::new();
    out.push_str(
        &json!({
            "type": "manifest",
            "row": {
                "format": "gather-bundle-v1",
                "exported_at": chrono::Utc::now(),
                "tables": TABLES.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            }
        })
        .to_string(),
    );
    out.push('\n');

    for (table, columns) in TABLES {
        // `table` and `columns` come from the compile-time constant above,
        // never from request input.
        let sql =
            format!("SELECT row_to_json(t)::text AS j FROM (SELECT {columns} FROM {table}) t");
        // Safe: table/column names come from the TABLES constant, not input.
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&state.pool)
            .await?;
        for row in rows {
            let j: String = row.get("j");
            out.push_str(&format!("{{\"type\":\"{table}\",\"row\":{j}}}\n"));
        }
    }

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"gather-bundle.ndjson\"",
            ),
        ],
        out,
    ))
}

// ---------------------------------------------------------------------------
// POST /api/v1/import
// ---------------------------------------------------------------------------

pub async fn import_bundle(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    // Group lines by table so inserts run in FK-dependency order regardless
    // of line order in the bundle.
    let mut by_table: std::collections::HashMap<&str, Vec<Value>> =
        std::collections::HashMap::new();
    let mut manifest_seen = false;

    for (lineno, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| {
            ApiError::BadRequest(format!("invalid NDJSON at line {}: {e}", lineno + 1))
        })?;
        let typ = v
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::BadRequest(format!("line {} missing 'type'", lineno + 1)))?;
        if typ == "manifest" {
            let format = v.pointer("/row/format").and_then(Value::as_str);
            if format != Some("gather-bundle-v1") {
                return Err(ApiError::BadRequest(format!(
                    "unsupported bundle format {format:?}"
                )));
            }
            manifest_seen = true;
            continue;
        }
        let Some((table, _)) = TABLES.iter().find(|(t, _)| *t == typ) else {
            return Err(ApiError::BadRequest(format!(
                "line {}: unknown record type '{typ}'",
                lineno + 1
            )));
        };
        let row = v
            .get("row")
            .cloned()
            .ok_or_else(|| ApiError::BadRequest(format!("line {} missing 'row'", lineno + 1)))?;
        by_table.entry(table).or_default().push(row);
    }

    if !manifest_seen {
        return Err(ApiError::BadRequest(
            "bundle has no manifest line (expected format gather-bundle-v1)".to_string(),
        ));
    }

    let mut tx = state.pool.begin().await?;
    let mut counts = serde_json::Map::new();
    for (table, columns) in TABLES {
        let Some(rows) = by_table.get(table) else {
            continue;
        };
        // Idempotent import: existing rows (same PK or unique key) are kept.
        let sql = format!(
            "INSERT INTO {table} ({columns}) \
             SELECT {columns} FROM jsonb_populate_record(NULL::{table}, $1::jsonb) \
             ON CONFLICT DO NOTHING"
        );
        let mut inserted = 0u64;
        for row in rows {
            // Safe: table/column names come from the TABLES constant, not input.
            let result = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(row)
                .execute(&mut *tx)
                .await?;
            inserted += result.rows_affected();
        }
        counts.insert(
            (*table).to_string(),
            json!({ "in_bundle": rows.len(), "inserted": inserted }),
        );
    }
    tx.commit().await?;

    Ok(Json(
        json!({ "format": "gather-bundle-v1", "tables": counts }),
    ))
}
