//! Contradiction review workflow: list, inspect, resolve, annotate.
//!
//! Resolution is transactional and always leaves an audit trail: the
//! contradiction row changes status, a contradiction_audit row records who /
//! when / why, and the losing atomic unit (if any) is marked superseded with
//! its temporal validity closed — which is how resolutions propagate back
//! into the knowledge graph (graph queries and search only consider
//! status = 'active' units and relationships).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

#[derive(Deserialize)]
pub struct ContradictionListParams {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_contradictions(
    State(state): State<AppState>,
    Query(params): Query<ContradictionListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = sqlx::query(
        r#"
        SELECT c.id, c.score, c.detection_method, c.explanation,
               c.status::text AS status, c.detected_at, c.resolved_at,
               c.resolved_by, c.resolution_note,
               a.id AS unit_a_id, a.statement AS unit_a_statement,
               b.id AS unit_b_id, b.statement AS unit_b_statement
        FROM contradictions c
        JOIN atomic_units a ON a.id = c.unit_a_id
        JOIN atomic_units b ON b.id = c.unit_b_id
        WHERE ($1::contradiction_status IS NULL OR c.status = $1::contradiction_status)
        ORDER BY c.score DESC, c.detected_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(&params.status)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "score": r.get::<f32, _>("score"),
                "detection_method": r.get::<String, _>("detection_method"),
                "explanation": r.get::<Option<String>, _>("explanation"),
                "status": r.get::<String, _>("status"),
                "detected_at": r.get::<chrono::DateTime<chrono::Utc>, _>("detected_at"),
                "resolved_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("resolved_at"),
                "resolved_by": r.get::<Option<String>, _>("resolved_by"),
                "resolution_note": r.get::<Option<String>, _>("resolution_note"),
                "unit_a": {
                    "id": r.get::<Uuid, _>("unit_a_id"),
                    "statement": r.get::<String, _>("unit_a_statement"),
                },
                "unit_b": {
                    "id": r.get::<Uuid, _>("unit_b_id"),
                    "statement": r.get::<String, _>("unit_b_statement"),
                },
            })
        })
        .collect();

    Ok(Json(
        json!({ "items": items, "limit": limit, "offset": offset }),
    ))
}

pub async fn get_contradiction(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT c.id, c.unit_a_id, c.unit_b_id, c.score, c.detection_method,
               c.explanation, c.status::text AS status, c.detected_at,
               c.resolved_at, c.resolved_by, c.resolution_note
        FROM contradictions c WHERE c.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("contradiction {id}")))?;

    // Full provenance for both units, across every source modality, so the
    // review UI can show "chat said X on Jan 3, the uploaded PDF says Y".
    let provenance = |unit_id: Uuid| {
        sqlx::query(
            r#"
            SELECT p.id, p.artifact_id, p.message_id, p.document_segment_id,
                   p.image_id, p.quote, p.char_start, p.char_end,
                   a.kind::text AS artifact_kind, a.source_platform,
                   a.original_filename, a.ingested_at
            FROM atomic_unit_provenance p
            JOIN artifacts a ON a.id = p.artifact_id
            WHERE p.atomic_unit_id = $1
            ORDER BY p.created_at
            "#,
        )
        .bind(unit_id)
        .fetch_all(&state.pool)
    };

    let unit = |unit_id: Uuid| {
        sqlx::query(
            r#"
            SELECT id, kind::text AS kind, statement, confidence, status::text AS status,
                   valid_from, valid_to, extraction_method::text AS extraction_method
            FROM atomic_units WHERE id = $1
            "#,
        )
        .bind(unit_id)
        .fetch_one(&state.pool)
    };

    let unit_a_id: Uuid = row.get("unit_a_id");
    let unit_b_id: Uuid = row.get("unit_b_id");
    let (unit_a, unit_b) = (unit(unit_a_id).await?, unit(unit_b_id).await?);
    let (prov_a, prov_b) = (provenance(unit_a_id).await?, provenance(unit_b_id).await?);

    let audit = sqlx::query(
        r#"SELECT action, actor, from_status::text AS from_status,
                  to_status::text AS to_status, note, created_at
           FROM contradiction_audit WHERE contradiction_id = $1 ORDER BY created_at"#,
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    let unit_json = |r: &sqlx::postgres::PgRow, prov: &[sqlx::postgres::PgRow]| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "kind": r.get::<String, _>("kind"),
            "statement": r.get::<String, _>("statement"),
            "confidence": r.get::<f32, _>("confidence"),
            "status": r.get::<String, _>("status"),
            "valid_from": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("valid_from"),
            "valid_to": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("valid_to"),
            "extraction_method": r.get::<String, _>("extraction_method"),
            "provenance": prov.iter().map(|p| json!({
                "artifact_id": p.get::<Uuid, _>("artifact_id"),
                "artifact_kind": p.get::<String, _>("artifact_kind"),
                "source_platform": p.get::<String, _>("source_platform"),
                "original_filename": p.get::<Option<String>, _>("original_filename"),
                "ingested_at": p.get::<chrono::DateTime<chrono::Utc>, _>("ingested_at"),
                "message_id": p.get::<Option<Uuid>, _>("message_id"),
                "document_segment_id": p.get::<Option<Uuid>, _>("document_segment_id"),
                "image_id": p.get::<Option<Uuid>, _>("image_id"),
                "quote": p.get::<Option<String>, _>("quote"),
            })).collect::<Vec<_>>(),
        })
    };

    Ok(Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "score": row.get::<f32, _>("score"),
        "detection_method": row.get::<String, _>("detection_method"),
        "explanation": row.get::<Option<String>, _>("explanation"),
        "status": row.get::<String, _>("status"),
        "detected_at": row.get::<chrono::DateTime<chrono::Utc>, _>("detected_at"),
        "resolved_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("resolved_at"),
        "resolved_by": row.get::<Option<String>, _>("resolved_by"),
        "resolution_note": row.get::<Option<String>, _>("resolution_note"),
        "unit_a": unit_json(&unit_a, &prov_a),
        "unit_b": unit_json(&unit_b, &prov_b),
        "audit": audit.iter().map(|a| json!({
            "action": a.get::<String, _>("action"),
            "actor": a.get::<String, _>("actor"),
            "from_status": a.get::<Option<String>, _>("from_status"),
            "to_status": a.get::<Option<String>, _>("to_status"),
            "note": a.get::<Option<String>, _>("note"),
            "created_at": a.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct ResolveRequest {
    /// resolved_a (keep A, supersede B) | resolved_b (keep B, supersede A)
    /// | both_valid | dismissed
    pub resolution: String,
    pub note: Option<String>,
    /// Reviewer identity; defaults to 'local-user' (single-user desktop).
    pub actor: Option<String>,
}

pub async fn resolve_contradiction(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ResolveRequest>,
) -> Result<Json<Value>, ApiError> {
    let valid = ["resolved_a", "resolved_b", "both_valid", "dismissed"];
    if !valid.contains(&req.resolution.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "resolution must be one of {valid:?}"
        )));
    }
    let actor = req
        .actor
        .clone()
        .unwrap_or_else(|| "local-user".to_string());

    let mut tx = state.pool.begin().await?;

    let row = sqlx::query(
        "SELECT unit_a_id, unit_b_id, status::text AS status FROM contradictions WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("contradiction {id}")))?;

    let previous_status: String = row.get("status");
    if previous_status != "open" {
        return Err(ApiError::BadRequest(format!(
            "contradiction is already '{previous_status}'"
        )));
    }

    sqlx::query(
        r#"
        UPDATE contradictions
        SET status = $2::contradiction_status, resolved_at = now(),
            resolved_by = $3, resolution_note = $4
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(&req.resolution)
    .bind(&actor)
    .bind(&req.note)
    .execute(&mut *tx)
    .await?;

    // Propagate into the graph: the losing unit is superseded by the winner
    // and its validity window is closed as of now.
    let (winner, loser): (Option<Uuid>, Option<Uuid>) = match req.resolution.as_str() {
        "resolved_a" => (Some(row.get("unit_a_id")), Some(row.get("unit_b_id"))),
        "resolved_b" => (Some(row.get("unit_b_id")), Some(row.get("unit_a_id"))),
        _ => (None, None),
    };
    if let (Some(winner), Some(loser)) = (winner, loser) {
        sqlx::query(
            r#"
            UPDATE atomic_units
            SET status = 'superseded', superseded_by_unit_id = $2,
                valid_to = coalesce(valid_to, now())
            WHERE id = $1
            "#,
        )
        .bind(loser)
        .bind(winner)
        .execute(&mut *tx)
        .await?;
        // Relationships asserted only by the superseded unit go inactive too.
        sqlx::query("UPDATE relationships SET status = 'superseded' WHERE atomic_unit_id = $1")
            .bind(loser)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query(
        r#"
        INSERT INTO contradiction_audit
            (contradiction_id, action, actor, from_status, to_status, note)
        VALUES ($1, 'resolve', $2, $3::contradiction_status, $4::contradiction_status, $5)
        "#,
    )
    .bind(id)
    .bind(&actor)
    .bind(&previous_status)
    .bind(&req.resolution)
    .bind(&req.note)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    metrics::counter!("gather_contradictions_resolved_total", "resolution" => req.resolution.clone())
        .increment(1);

    Ok(Json(json!({
        "id": id,
        "status": req.resolution,
        "resolved_by": actor,
    })))
}

#[derive(Deserialize)]
pub struct AnnotateRequest {
    pub note: String,
    pub actor: Option<String>,
}

pub async fn annotate_contradiction(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<AnnotateRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if req.note.trim().is_empty() {
        return Err(ApiError::BadRequest("note must not be empty".to_string()));
    }
    let exists: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM contradictions WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::NotFound(format!("contradiction {id}")));
    }

    let actor = req.actor.unwrap_or_else(|| "local-user".to_string());
    let (audit_id,): (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO contradiction_audit (contradiction_id, action, actor, note)
        VALUES ($1, 'annotate', $2, $3)
        RETURNING id
        "#,
    )
    .bind(id)
    .bind(&actor)
    .bind(req.note.trim())
    .fetch_one(&state.pool)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": audit_id, "contradiction_id": id, "actor": actor })),
    ))
}
