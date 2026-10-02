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

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use sqlx::Row;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};

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
         supersedes_artifact_id, source_created_at, ingested_at, ingestion_job_id, metadata, \
         retracted_at, retraction_reason",
    ),
    (
        "artifact_derivations",
        "id, child_artifact_id, parent_artifact_id, kind, actor, created_at",
    ),
    (
        "conversations",
        "id, artifact_id, external_id, title, source_platform, model, started_at, \
         ended_at, metadata",
    ),
    (
        "messages",
        "id, conversation_id, external_id, parent_message_id, seq, role, author, \
         model, content, created_at, metadata, units_extracted_at, units_extract_error, units_llm_model",
    ),
    (
        "documents",
        "id, artifact_id, page_count, language, extracted_text, extraction_tool, \
         extraction_status, extracted_at, metadata",
    ),
    (
        "document_segments",
        "id, document_id, seq, page, heading, content, content_hash, embedding, embedding_model, metadata, \
         units_extracted_at, units_extract_error, units_llm_model",
    ),
    // Clusters before images and atomic_units, whose *_cluster_id columns
    // reference it. Clusters have no outbound FKs, so this position is safe.
    (
        "clusters",
        "id, kind, label, cohesion, size, created_at, updated_at, representative_id",
    ),
    (
        "images",
        "id, artifact_id, width, height, exif, taken_at, ocr_text, ocr_confidence, \
         ocr_status, caption, caption_model, metadata, units_extracted_at, phash, latitude, \
         longitude, embedding, embedding_model, photo_prepared_at, photo_grouped_at, captioned_at, \
         dup_cluster_id, album_cluster_id, topic_cluster_id, units_extract_error, units_llm_model",
    ),
    (
        "entities",
        "id, name, kind, description, merged_into_entity_id, embedding, embedding_model, metadata, \
         created_at, updated_at",
    ),
    ("entity_aliases", "id, entity_id, alias"),
    (
        "atomic_units",
        "id, kind, statement, statement_hash, subject_entity_id, confidence, \
         extraction_method, extraction_model, embedding, valid_from, valid_to, \
         status, superseded_by_unit_id, attrs, created_at, updated_at, \
         contradiction_scanned_at, topic_cluster_id, clustered_at, asserted_at, observed_at, content_revision, embedding_model, embedding_retry_at, embedding_attempts, has_source_history",
    ),
    (
        "atomic_unit_revisions",
        "id, atomic_unit_id, revision, before_state, relationships, created_at",
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
    // Before contradictions, which point at the certificate that reported them.
    (
        "inference_certificates",
        "id, conclusion_kind, conclusion_key, conclusion_id, subject_ids, rule_id, \
         rule_version, decision, outcome, evidence_class, inputs, input_ids, \
         source_artifact_ids, source_family_ids, model_version, config, scope, temporal, \
         predicates, reason_codes, explanation, evidence_digest, created_at, superseded_at, \
         retracted_at, status_reason, caused_by",
    ),
    (
        "contradictions",
        "id, unit_a_id, unit_b_id, score, detection_method, explanation, status, \
         detected_at, resolved_at, resolved_by, resolution_note, certificate_id, alignment, \
         certainty",
    ),
    (
        "contradiction_audit",
        "id, contradiction_id, action, actor, from_status, to_status, note, created_at",
    ),
    // After "entities", whose rows it references on both sides.
    (
        "entity_merge_audit",
        "id, winner_entity_id, loser_entity_id, action, actor, note, created_at, undo, score, \
         undone_at",
    ),
    // Feedback loop (autonomous pipeline). No FK dependencies among these
    // tables, so ordering among them is free; they round-trip the correction
    // history, the review tray, and the learned thresholds.
    (
        "unit_feedback",
        "id, target_kind, target_id, action, actor, corrected, note, created_at, score",
    ),
    (
        "review_queue",
        "id, target_kind, target_id, info_gain, reason, signals, state, created_at",
    ),
    ("decision_tuning", "key, value, updated_at"),
    (
        "decision_tuning_audit",
        "id, key, old_value, new_value, actor, reason, created_at",
    ),
    // After clusters (its cluster_id FK). member_id is a plain uuid, no FK.
    ("cluster_members", "cluster_id, member_kind, member_id, sim"),
    // User decisions no automatic rule may reverse ("not a duplicate").
    (
        "semantic_user_decisions",
        "id, kind, a_id, b_id, actor, note, created_at, revoked_at",
    ),
    // Projects: after artifacts (project_items.artifact_id). parent_id is
    // deferrable, so items restore in any order.
    ("projects", "id, name, source, created_at, updated_at"),
    (
        "project_items",
        "id, project_id, parent_id, item_kind, name, path, depth, artifact_id, status, detail, \
         byte_size, created_at",
    ),
];

/// Bundle limits are independent of the per-file ingestion limit.
pub(crate) const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 1024 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct BundleLimit {
    bytes: u64,
    limit: u64,
}

impl Default for BundleLimit {
    fn default() -> Self {
        Self {
            bytes: 0,
            limit: MAX_BUNDLE_BYTES,
        }
    }
}

pub(crate) async fn write_bundle_chunk(
    file: &mut tokio::fs::File,
    limit: &mut BundleLimit,
    chunk: &[u8],
) -> Result<(), ApiError> {
    let total = limit
        .bytes
        .checked_add(chunk.len() as u64)
        .filter(|total| *total <= limit.limit)
        .ok_or_else(|| ApiError::PayloadTooLarge("bundle exceeds its size limit".into()))?;
    file.write_all(chunk)
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    limit.bytes = total;
    Ok(())
}

pub async fn export_bundle(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let stream = bundle_stream(&state.pool).await?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"gather-bundle.ndjson\"",
            ),
        ],
        Body::from_stream(stream),
    ))
}

/// A bounded producer holds one database snapshot and at most one serialized
/// row. Dropping the receiver cancels production and rolls back the snapshot.
pub(crate) async fn bundle_stream(
    pool: &sqlx::PgPool,
) -> Result<ReceiverStream<Result<Vec<u8>, ApiError>>, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    tokio::spawn(async move {
        let result: Result<(), ApiError> = async {
            let manifest = format!(
                "{}\n",
                json!({
                    "type": "manifest",
                    "row": {
                        "format": "gather-bundle-v1",
                        "exported_at": chrono::Utc::now(),
                        "tables": TABLES.iter().map(|(table, _)| *table).collect::<Vec<_>>(),
                    }
                })
            );
            let mut bytes_sent = manifest.len() as u64;
            if sender.send(Ok(manifest.into_bytes())).await.is_err() {
                return Ok(());
            }
            for (table, columns) in TABLES {
                let sql = format!(
                    "SELECT row_to_json(t)::text AS j FROM (SELECT {columns} FROM {table}) t"
                );
                let mut rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch(&mut *tx);
                while let Some(row) = rows.next().await {
                    let row = row?;
                    let json: &str = row.try_get("j")?;
                    if json.len() as u64 > MAX_RECORD_BYTES - 128 {
                        return Err(ApiError::PayloadTooLarge(
                            "bundle or record exceeds its size limit".into(),
                        ));
                    }
                    let prefix = format!("{{\"type\":\"{table}\",\"row\":");
                    bytes_sent = bytes_sent.saturating_add((prefix.len() + json.len() + 2) as u64);
                    if bytes_sent > MAX_BUNDLE_BYTES {
                        return Err(ApiError::PayloadTooLarge("bundle exceeds 64 GiB".into()));
                    }
                    if sender.send(Ok(prefix.into_bytes())).await.is_err() {
                        return Ok(());
                    }
                    for chunk in json.as_bytes().chunks(CHUNK_BYTES) {
                        if sender.send(Ok(chunk.to_vec())).await.is_err() {
                            return Ok(());
                        }
                    }
                    if sender.send(Ok(b"}\n".to_vec())).await.is_err() {
                        return Ok(());
                    }
                }
            }
            tx.commit().await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = sender.send(Err(error)).await;
        }
    });
    Ok(ReceiverStream::new(receiver))
}

/// NamedTempFile creates a private file and removes it on error or cancellation.
pub(crate) fn private_bundle_file() -> Result<tempfile::NamedTempFile, ApiError> {
    tempfile::NamedTempFile::new().map_err(|error| ApiError::Internal(error.into()))
}

pub async fn import_bundle(
    State(state): State<AppState>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let temporary = private_bundle_file()?;
    let mut file = tokio::fs::File::from_std(
        temporary
            .reopen()
            .map_err(|error| ApiError::Internal(error.into()))?,
    );
    let mut stream = body.into_data_stream();
    let mut limit = BundleLimit::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ApiError::BadRequest(error.to_string()))?;
        write_bundle_chunk(&mut file, &mut limit, &chunk).await?;
    }
    file.flush()
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    let counts = import_bundle_file(&state.pool, &temporary).await?;
    Ok(Json(
        json!({ "format": "gather-bundle-v1", "tables": counts }),
    ))
}

async fn read_record(reader: &mut BufReader<tokio::fs::File>) -> Result<Option<String>, ApiError> {
    let mut line = String::new();
    let mut bounded = reader.take(MAX_RECORD_BYTES + 1);
    let size = bounded
        .read_line(&mut line)
        .await
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    if size as u64 > MAX_RECORD_BYTES {
        return Err(ApiError::PayloadTooLarge(
            "bundle or record exceeds its size limit".into(),
        ));
    }
    Ok((size > 0).then_some(line))
}

/// Stage records on disk by table, then apply in dependency order in one
/// transaction. The library size affects disk space, rather than heap use.
pub(crate) async fn import_bundle_file(
    pool: &sqlx::PgPool,
    bundle: &tempfile::NamedTempFile,
) -> Result<serde_json::Map<String, Value>, ApiError> {
    let file = tokio::fs::File::from_std(
        bundle
            .reopen()
            .map_err(|error| ApiError::Internal(error.into()))?,
    );
    let mut reader = BufReader::new(file);
    let mut tables = std::collections::HashMap::new();
    let mut manifest_seen = false;
    let mut line_number = 0;
    while let Some(line) = read_record(&mut reader).await? {
        line_number += 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|error| {
            ApiError::BadRequest(format!("invalid NDJSON at line {line_number}: {error}"))
        })?;
        let typ = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::BadRequest(format!("line {line_number} missing 'type'")))?;
        if typ == "manifest" {
            if manifest_seen
                || value.pointer("/row/format").and_then(Value::as_str) != Some("gather-bundle-v1")
            {
                return Err(ApiError::BadRequest(
                    "invalid or duplicate bundle manifest".into(),
                ));
            }
            manifest_seen = true;
            continue;
        }
        let Some((table, _)) = TABLES.iter().find(|(table, _)| *table == typ) else {
            return Err(ApiError::BadRequest(format!("unknown record type '{typ}'")));
        };
        if !value.get("row").is_some_and(Value::is_object) {
            return Err(ApiError::BadRequest(format!(
                "line {line_number} missing row object"
            )));
        }
        if !tables.contains_key(table) {
            let temporary = private_bundle_file()?;
            let writer = tokio::fs::File::from_std(
                temporary
                    .reopen()
                    .map_err(|error| ApiError::Internal(error.into()))?,
            );
            tables.insert(*table, (temporary, writer, 0u64));
        }
        let (_, writer, count) = tables.get_mut(table).expect("inserted above");
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|error| ApiError::Internal(error.into()))?;
        if !line.ends_with('\n') {
            writer
                .write_all(b"\n")
                .await
                .map_err(|error| ApiError::Internal(error.into()))?;
        }
        *count += 1;
    }
    if !manifest_seen {
        return Err(ApiError::BadRequest("bundle has no manifest line".into()));
    }

    let mut tx = pool.begin().await?;
    // Lock model identity before inserting vector-bearing records.
    let model: Option<String> =
        sqlx::query_scalar("SELECT model FROM embedding_state WHERE singleton FOR SHARE")
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("SET CONSTRAINTS ALL DEFERRED")
        .execute(&mut *tx)
        .await?;
    let mut counts = serde_json::Map::new();
    for (table, columns) in TABLES {
        let Some((temporary, writer, count)) = tables.get_mut(table) else {
            continue;
        };
        writer
            .flush()
            .await
            .map_err(|error| ApiError::Internal(error.into()))?;
        let file = tokio::fs::File::from_std(
            temporary
                .reopen()
                .map_err(|error| ApiError::Internal(error.into()))?,
        );
        let mut reader = BufReader::new(file);
        let membership = if *table == "cluster_members" {
            // Vector invalidation clears topic pointers before members restore.
            // Keep only memberships still represented by the restored rows.
            " WHERE member_kind = 'entity'
              OR (member_kind = 'unit' AND EXISTS (
                SELECT 1 FROM atomic_units u WHERE u.id = r.member_id
                  AND u.topic_cluster_id = r.cluster_id))
              OR (member_kind = 'image' AND EXISTS (
                SELECT 1 FROM images i WHERE i.id = r.member_id AND r.cluster_id IN
                  (i.topic_cluster_id, i.dup_cluster_id, i.album_cluster_id)))"
        } else {
            ""
        };
        let sql = format!(
            "INSERT INTO {table} ({columns}) SELECT {columns}
             FROM jsonb_populate_record(NULL::{table}, $1::jsonb) r{membership}
             ON CONFLICT DO NOTHING"
        );
        let mut inserted = 0u64;
        while let Some(line) = read_record(&mut reader).await? {
            let mut value: Value = serde_json::from_str(&line)
                .map_err(|error| ApiError::BadRequest(error.to_string()))?;
            drop(line);
            let mut row = value["row"].take();
            if let Some(object) = row.as_object_mut() {
                if *table == "atomic_units" {
                    object.entry("content_revision").or_insert(json!(0));
                    object.entry("has_source_history").or_insert(json!(false));
                    object.entry("embedding_attempts").or_insert(json!(0));
                }
                if ["atomic_units", "document_segments", "entities", "images"].contains(table)
                    && object
                        .get("embedding")
                        .is_some_and(|embedding| !embedding.is_null())
                    && (model.is_none()
                        || object.get("embedding_model").and_then(Value::as_str)
                            != model.as_deref())
                {
                    object.insert("embedding".into(), Value::Null);
                    object.insert("embedding_model".into(), Value::Null);
                    if *table == "atomic_units" {
                        object.insert("contradiction_scanned_at".into(), Value::Null);
                        object.insert("clustered_at".into(), Value::Null);
                        object.insert("topic_cluster_id".into(), Value::Null);
                        object.insert("embedding_retry_at".into(), Value::Null);
                        object.insert("embedding_attempts".into(), json!(0));
                    }
                    if *table == "images" {
                        object.insert("captioned_at".into(), Value::Null);
                        object.insert("topic_cluster_id".into(), Value::Null);
                    }
                }
            }
            inserted += sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(row)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        counts.insert(
            (*table).to_string(),
            json!({ "in_bundle": count, "inserted": inserted }),
        );
    }
    tx.commit().await?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn aggregate_import_limit_rejects_before_writing_the_excess_chunk() {
        let temporary = private_bundle_file().unwrap();
        let mut file = tokio::fs::File::from_std(temporary.reopen().unwrap());
        let mut limit = BundleLimit { bytes: 0, limit: 4 };
        write_bundle_chunk(&mut file, &mut limit, b"abc")
            .await
            .unwrap();
        assert!(matches!(
            write_bundle_chunk(&mut file, &mut limit, b"de").await,
            Err(ApiError::PayloadTooLarge(_))
        ));
        file.flush().await.unwrap();
        assert_eq!(std::fs::read(temporary.path()).unwrap(), b"abc");
    }
}
