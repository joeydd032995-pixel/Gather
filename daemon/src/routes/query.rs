//! Read-side endpoints: artifacts, atomic units, graph traversal, and
//! semantic / full-text search.

use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::Json;
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

fn clamp_limit(limit: Option<i64>, default: i64, max: i64) -> i64 {
    limit.unwrap_or(default).clamp(1, max)
}

// ---------------------------------------------------------------------------
// GET /api/v1/artifacts and GET /api/v1/artifacts/{id}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ArtifactListParams {
    pub kind: Option<String>,
    pub source_platform: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_artifacts(
    State(state): State<AppState>,
    Query(params): Query<ArtifactListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(params.limit, 50, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = sqlx::query(
        r#"
        SELECT id, kind::text AS kind, source_platform, source_format_version,
               original_filename, media_type, byte_size, content_hash, version,
               supersedes_artifact_id, source_created_at, ingested_at, metadata
        FROM artifacts
        WHERE ($1::artifact_kind IS NULL OR kind = $1::artifact_kind)
          AND ($2::text IS NULL OR source_platform = $2)
        ORDER BY ingested_at DESC
        LIMIT $3 OFFSET $4
        "#,
    )
    .bind(&params.kind)
    .bind(&params.source_platform)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows.iter().map(artifact_row_to_json).collect();
    Ok(Json(
        json!({ "items": items, "limit": limit, "offset": offset }),
    ))
}

pub async fn get_artifact(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT id, kind::text AS kind, source_platform, source_format_version,
               original_filename, media_type, byte_size, content_hash, version,
               supersedes_artifact_id, source_created_at, ingested_at, metadata
        FROM artifacts WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("artifact {id}")))?;

    let mut body = artifact_row_to_json(&row);

    // Attach modality detail so one call answers "what is this artifact".
    let conversations: Vec<Value> = sqlx::query(
        r#"SELECT id, external_id, title, model, started_at, ended_at
           FROM conversations WHERE artifact_id = $1 ORDER BY started_at"#,
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "external_id": r.get::<Option<String>, _>("external_id"),
            "title": r.get::<Option<String>, _>("title"),
            "model": r.get::<Option<String>, _>("model"),
            "started_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at"),
            "ended_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("ended_at"),
        })
    })
    .collect();

    let document = sqlx::query(
        r#"SELECT id, page_count, language, extraction_tool, extraction_status::text AS status,
                  extracted_at,
                  (SELECT count(*) FROM document_segments s WHERE s.document_id = d.id) AS segment_count
           FROM documents d WHERE artifact_id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "page_count": r.get::<Option<i32>, _>("page_count"),
            "language": r.get::<Option<String>, _>("language"),
            "extraction_tool": r.get::<Option<String>, _>("extraction_tool"),
            "extraction_status": r.get::<String, _>("status"),
            "extracted_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("extracted_at"),
            "segment_count": r.get::<i64, _>("segment_count"),
        })
    });

    let image = sqlx::query(
        r#"SELECT id, width, height, exif, taken_at, ocr_status::text AS ocr_status,
                  ocr_confidence, caption
           FROM images WHERE artifact_id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "width": r.get::<Option<i32>, _>("width"),
            "height": r.get::<Option<i32>, _>("height"),
            "exif": r.get::<Value, _>("exif"),
            "taken_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("taken_at"),
            "ocr_status": r.get::<String, _>("ocr_status"),
            "ocr_confidence": r.get::<Option<f32>, _>("ocr_confidence"),
            "caption": r.get::<Option<String>, _>("caption"),
        })
    });

    body["conversations"] = json!(conversations);
    body["document"] = document.unwrap_or(Value::Null);
    body["image"] = image.unwrap_or(Value::Null);
    Ok(Json(body))
}

fn artifact_row_to_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "kind": row.get::<String, _>("kind"),
        "source_platform": row.get::<String, _>("source_platform"),
        "source_format_version": row.get::<Option<String>, _>("source_format_version"),
        "original_filename": row.get::<Option<String>, _>("original_filename"),
        "media_type": row.get::<Option<String>, _>("media_type"),
        "byte_size": row.get::<i64, _>("byte_size"),
        "content_hash": row.get::<String, _>("content_hash"),
        "version": row.get::<i32, _>("version"),
        "supersedes_artifact_id": row.get::<Option<Uuid>, _>("supersedes_artifact_id"),
        "source_created_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("source_created_at"),
        "ingested_at": row.get::<chrono::DateTime<chrono::Utc>, _>("ingested_at"),
        "metadata": row.get::<Value, _>("metadata"),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/atomic-units
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UnitListParams {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub subject_entity_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_atomic_units(
    State(state): State<AppState>,
    Query(params): Query<UnitListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = clamp_limit(params.limit, 50, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = sqlx::query(
        r#"
        SELECT u.id, u.kind::text AS kind, u.statement, u.confidence,
               u.extraction_method::text AS extraction_method, u.extraction_model,
               u.valid_from, u.valid_to, u.status::text AS status,
               u.subject_entity_id, u.superseded_by_unit_id, u.created_at,
               (SELECT count(*) FROM atomic_unit_provenance p
                WHERE p.atomic_unit_id = u.id) AS provenance_count
        FROM atomic_units u
        WHERE ($1::unit_kind IS NULL OR u.kind = $1::unit_kind)
          AND ($2::unit_status IS NULL OR u.status = $2::unit_status)
          AND ($3::uuid IS NULL OR u.subject_entity_id = $3)
        ORDER BY u.created_at DESC
        LIMIT $4 OFFSET $5
        "#,
    )
    .bind(&params.kind)
    .bind(&params.status)
    .bind(params.subject_entity_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows.iter().map(unit_row_to_json).collect();
    Ok(Json(
        json!({ "items": items, "limit": limit, "offset": offset }),
    ))
}

pub fn unit_row_to_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "kind": row.get::<String, _>("kind"),
        "statement": row.get::<String, _>("statement"),
        "confidence": row.get::<f32, _>("confidence"),
        "extraction_method": row.get::<String, _>("extraction_method"),
        "extraction_model": row.get::<Option<String>, _>("extraction_model"),
        "valid_from": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("valid_from"),
        "valid_to": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("valid_to"),
        "status": row.get::<String, _>("status"),
        "subject_entity_id": row.get::<Option<Uuid>, _>("subject_entity_id"),
        "superseded_by_unit_id": row.get::<Option<Uuid>, _>("superseded_by_unit_id"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "provenance_count": row.get::<i64, _>("provenance_count"),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/entities/{id}/graph?depth=N
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GraphParams {
    pub depth: Option<i32>,
}

pub async fn entity_graph(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<GraphParams>,
) -> Result<Json<Value>, ApiError> {
    let depth = params.depth.unwrap_or(2).clamp(1, 5);
    let started = Instant::now();

    let root =
        sqlx::query("SELECT id, name, kind::text AS kind, description FROM entities WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("entity {id}")))?;

    let edges = sqlx::query(
        r#"SELECT depth, relationship_id, source_entity_id, target_entity_id,
                  relation_type, confidence
           FROM entity_neighborhood($1, $2)"#,
    )
    .bind(id)
    .bind(depth)
    .fetch_all(&state.pool)
    .await?;

    // Hydrate the node set referenced by the edges.
    let mut node_ids: Vec<Uuid> = edges
        .iter()
        .flat_map(|e| {
            [
                e.get::<Uuid, _>("source_entity_id"),
                e.get::<Uuid, _>("target_entity_id"),
            ]
        })
        .collect();
    node_ids.push(id);
    node_ids.sort();
    node_ids.dedup();

    let nodes = sqlx::query(
        "SELECT id, name, kind::text AS kind, description FROM entities WHERE id = ANY($1)",
    )
    .bind(&node_ids)
    .fetch_all(&state.pool)
    .await?;

    let elapsed = started.elapsed();
    metrics::histogram!("gather_graph_query_duration_seconds").record(elapsed.as_secs_f64());

    Ok(Json(json!({
        "root": {
            "id": root.get::<Uuid, _>("id"),
            "name": root.get::<String, _>("name"),
            "kind": root.get::<String, _>("kind"),
        },
        "depth": depth,
        "nodes": nodes.iter().map(|n| json!({
            "id": n.get::<Uuid, _>("id"),
            "name": n.get::<String, _>("name"),
            "kind": n.get::<String, _>("kind"),
            "description": n.get::<Option<String>, _>("description"),
        })).collect::<Vec<_>>(),
        "edges": edges.iter().map(|e| json!({
            "id": e.get::<Uuid, _>("relationship_id"),
            "depth": e.get::<i32, _>("depth"),
            "source": e.get::<Uuid, _>("source_entity_id"),
            "target": e.get::<Uuid, _>("target_entity_id"),
            "relation_type": e.get::<String, _>("relation_type"),
            "confidence": e.get::<f32, _>("confidence"),
        })).collect::<Vec<_>>(),
        "query_ms": elapsed.as_millis() as u64,
    })))
}

// ---------------------------------------------------------------------------
// POST /api/v1/search/semantic
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SemanticSearchRequest {
    /// Full-text query (Postgres websearch syntax). Used when no embedding is
    /// supplied, or in hybrid mode alongside one.
    pub text: Option<String>,
    /// Pre-computed 768-dim embedding (e.g. from local Ollama
    /// nomic-embed-text). When present, results are ranked by cosine distance.
    pub embedding: Option<Vec<f32>>,
    /// One of: atomic_units (default) | messages | document_segments
    pub scope: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct SearchHit {
    pub id: Uuid,
    pub scope: String,
    pub content: String,
    pub score: f64,
    pub artifact_id: Option<Uuid>,
}

pub async fn semantic_search(
    State(state): State<AppState>,
    Json(req): Json<SemanticSearchRequest>,
) -> Result<Json<Value>, ApiError> {
    let (scope, hits) = search_core(&state, req).await?;
    Ok(Json(json!({ "scope": scope, "hits": hits })))
}

/// Shared semantic/full-text search core (REST + gRPC). When the caller
/// supplies no embedding but Ollama is configured, the query text is embedded
/// server-side so semantic ranking works out of the box; embedding failure
/// degrades to full-text with a warning.
pub(crate) async fn search_core(
    state: &AppState,
    mut req: SemanticSearchRequest,
) -> Result<(String, Vec<SearchHit>), ApiError> {
    let scope = req
        .scope
        .clone()
        .unwrap_or_else(|| "atomic_units".to_string());
    let scope = scope.as_str();
    let limit = clamp_limit(req.limit, 20, 100);

    if req.embedding.is_none() && scope != "messages" {
        if let (Some(text), Some(client)) = (
            req.text.as_deref().map(str::trim).filter(|t| !t.is_empty()),
            state.ollama.as_ref(),
        ) {
            match client.embed(&[text.to_string()]).await {
                Ok(mut vectors) if vectors.first().map(Vec::len) == Some(768) => {
                    req.embedding = vectors.pop();
                }
                Ok(_) => {
                    tracing::warn!("embed model returned non-768-dim vector; full-text fallback")
                }
                Err(e) => tracing::warn!(error = %e, "query embedding failed; full-text fallback"),
            }
        }
    }

    if req.embedding.is_none() && req.text.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err(ApiError::BadRequest(
            "provide 'text', 'embedding', or both".to_string(),
        ));
    }
    if let Some(e) = &req.embedding {
        if e.len() != 768 {
            return Err(ApiError::BadRequest(format!(
                "embedding must have 768 dimensions, got {}",
                e.len()
            )));
        }
    }

    let hits: Vec<SearchHit> = match (scope, &req.embedding) {
        ("atomic_units", Some(embedding)) => {
            let vec = Vector::from(embedding.clone());
            sqlx::query(
                r#"
                SELECT u.id, u.statement AS content,
                       1 - (u.embedding <=> $1) AS score,
                       (SELECT p.artifact_id FROM atomic_unit_provenance p
                        WHERE p.atomic_unit_id = u.id LIMIT 1) AS artifact_id
                FROM atomic_units u
                WHERE u.embedding IS NOT NULL AND u.status = 'active'
                ORDER BY u.embedding <=> $1
                LIMIT $2
                "#,
            )
            .bind(vec)
            .bind(limit)
            .fetch_all(&state.pool)
            .await?
            .iter()
            .map(|r| hit(r, "atomic_units"))
            .collect()
        }
        ("document_segments", Some(embedding)) => {
            let vec = Vector::from(embedding.clone());
            sqlx::query(
                r#"
                SELECT s.id, s.content, 1 - (s.embedding <=> $1) AS score,
                       d.artifact_id
                FROM document_segments s
                JOIN documents d ON d.id = s.document_id
                WHERE s.embedding IS NOT NULL
                ORDER BY s.embedding <=> $1
                LIMIT $2
                "#,
            )
            .bind(vec)
            .bind(limit)
            .fetch_all(&state.pool)
            .await?
            .iter()
            .map(|r| hit(r, "document_segments"))
            .collect()
        }
        ("atomic_units", None) => {
            sqlx::query(
                r#"
                SELECT u.id, u.statement AS content,
                       ts_rank(u.statement_tsv, websearch_to_tsquery('english', $1))::float8 AS score,
                       (SELECT p.artifact_id FROM atomic_unit_provenance p
                        WHERE p.atomic_unit_id = u.id LIMIT 1) AS artifact_id
                FROM atomic_units u
                WHERE u.statement_tsv @@ websearch_to_tsquery('english', $1)
                  AND u.status = 'active'
                ORDER BY score DESC
                LIMIT $2
                "#,
            )
            .bind(req.text.as_deref().unwrap_or(""))
            .bind(limit)
            .fetch_all(&state.pool)
            .await?
            .iter()
            .map(|r| hit(r, "atomic_units"))
            .collect()
        }
        ("messages", None) => {
            sqlx::query(
                r#"
                SELECT m.id, m.content,
                       ts_rank(m.content_tsv, websearch_to_tsquery('english', $1))::float8 AS score,
                       c.artifact_id
                FROM messages m
                JOIN conversations c ON c.id = m.conversation_id
                WHERE m.content_tsv @@ websearch_to_tsquery('english', $1)
                ORDER BY score DESC
                LIMIT $2
                "#,
            )
            .bind(req.text.as_deref().unwrap_or(""))
            .bind(limit)
            .fetch_all(&state.pool)
            .await?
            .iter()
            .map(|r| hit(r, "messages"))
            .collect()
        }
        ("document_segments", None) => {
            sqlx::query(
                r#"
                SELECT s.id, s.content,
                       ts_rank(s.content_tsv, websearch_to_tsquery('english', $1))::float8 AS score,
                       d.artifact_id
                FROM document_segments s
                JOIN documents d ON d.id = s.document_id
                WHERE s.content_tsv @@ websearch_to_tsquery('english', $1)
                ORDER BY score DESC
                LIMIT $2
                "#,
            )
            .bind(req.text.as_deref().unwrap_or(""))
            .bind(limit)
            .fetch_all(&state.pool)
            .await?
            .iter()
            .map(|r| hit(r, "document_segments"))
            .collect()
        }
        ("messages", Some(_)) => {
            return Err(ApiError::BadRequest(
                "messages are searched by text only; embeddings live on atomic_units \
                 and document_segments"
                    .to_string(),
            ))
        }
        (other, _) => {
            return Err(ApiError::BadRequest(format!(
                "unknown scope '{other}' (atomic_units | messages | document_segments)"
            )))
        }
    };

    Ok((scope.to_string(), hits))
}

fn hit(row: &sqlx::postgres::PgRow, scope: &str) -> SearchHit {
    SearchHit {
        id: row.get("id"),
        scope: scope.to_string(),
        content: row.get("content"),
        score: row.try_get::<f64, _>("score").unwrap_or(0.0),
        artifact_id: row.get("artifact_id"),
    }
}
