//! Feedback loop (autonomous pipeline, Phase A).
//!
//! The user's rare, one-off correction is the training signal that keeps the
//! system honest — not a batch-labelling chore. Every action here is reversible
//! and leaves a durable `unit_feedback` row: a reject retracts a unit (and a
//! restore un-retracts it), an edit records the correction, a confirm is a
//! positive label. Acting on a unit also clears any open `review_queue` entry
//! for it, so the optional tray drains as the user works. All local, offline.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::persist::normalize_statement;
use crate::AppState;

#[derive(Deserialize)]
pub struct NoteRequest {
    pub note: Option<String>,
}

#[derive(Deserialize)]
pub struct EditRequest {
    pub statement: String,
    pub note: Option<String>,
}

/// Record feedback and, when the target is a unit, resolve its open tray entry.
async fn record_feedback(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target_id: Uuid,
    action: &str,
    corrected: Option<Value>,
    note: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO unit_feedback (target_kind, target_id, action, corrected, note) \
         VALUES ('unit', $1, $2, $3, $4)",
    )
    .bind(target_id)
    .bind(action)
    .bind(corrected)
    .bind(note)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE review_queue SET state = 'resolved' \
         WHERE target_kind = 'unit' AND target_id = $1 AND state = 'open'",
    )
    .bind(target_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn unit_status(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> Result<String, ApiError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status::text FROM atomic_units WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    match row {
        Some((s,)) => Ok(s),
        None => Err(ApiError::NotFound(format!("unit {id}"))),
    }
}

/// POST /units/{id}/reject — retract the unit and record a negative label.
pub async fn reject_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    let mut tx = state.pool.begin().await?;
    unit_status(&mut tx, id).await?;
    sqlx::query("UPDATE atomic_units SET status = 'retracted' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    record_feedback(&mut tx, id, "reject", None, note.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id, "status": "retracted" })))
}

/// POST /units/{id}/restore — undo a reject.
pub async fn restore_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    let mut tx = state.pool.begin().await?;
    unit_status(&mut tx, id).await?;
    sqlx::query("UPDATE atomic_units SET status = 'active' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    record_feedback(&mut tx, id, "confirm", None, note.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id, "status": "active" })))
}

/// POST /units/{id}/confirm — positive label, no state change.
pub async fn confirm_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    let mut tx = state.pool.begin().await?;
    let status = unit_status(&mut tx, id).await?;
    record_feedback(&mut tx, id, "confirm", None, note.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id, "status": status })))
}

/// PATCH /units/{id} — correct a unit's statement. Records the edit and
/// re-hashes for dedup; a collision with an existing statement is a 400.
pub async fn edit_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<EditRequest>,
) -> Result<Json<Value>, ApiError> {
    let statement = req.statement.trim().to_string();
    if statement.is_empty() {
        return Err(ApiError::BadRequest(
            "statement must not be empty".to_string(),
        ));
    }
    let hash = hex::encode(Sha256::digest(normalize_statement(&statement)));

    let mut tx = state.pool.begin().await?;
    unit_status(&mut tx, id).await?;

    let clash: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM atomic_units WHERE statement_hash = $1 AND id <> $2")
            .bind(&hash)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if clash.is_some() {
        return Err(ApiError::BadRequest(
            "edited statement collides with an existing unit".to_string(),
        ));
    }

    sqlx::query(
        "UPDATE atomic_units SET statement = $2, statement_hash = $3, \
         extraction_method = 'manual' WHERE id = $1",
    )
    .bind(id)
    .bind(&statement)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    record_feedback(
        &mut tx,
        id,
        "edit",
        Some(json!({ "statement": statement })),
        req.note.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id, "statement": statement })))
}

#[derive(Deserialize)]
pub struct ReviewListParams {
    pub limit: Option<i64>,
}

/// GET /review — the optional hold tray, highest info_gain first. Unit targets
/// carry their statement so the tray is readable without a second call.
pub async fn list_review(
    State(state): State<AppState>,
    Query(params): Query<ReviewListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.unwrap_or(100).clamp(1, 500);
    let rows = sqlx::query(
        "SELECT r.id, r.target_kind, r.target_id, r.reason, r.info_gain, r.signals, \
                r.created_at, u.statement AS unit_statement \
         FROM review_queue r \
         LEFT JOIN atomic_units u ON r.target_kind = 'unit' AND u.id = r.target_id \
         WHERE r.state = 'open' \
         ORDER BY r.info_gain DESC, r.created_at \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "target_kind": r.get::<String, _>("target_kind"),
                "target_id": r.get::<Uuid, _>("target_id"),
                "reason": r.get::<String, _>("reason"),
                "info_gain": r.get::<f32, _>("info_gain"),
                "signals": r.get::<Value, _>("signals"),
                "statement": r.get::<Option<String>, _>("unit_statement"),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// POST /review/{id}/resolve — dismiss a tray entry without acting on the item.
pub async fn resolve_review(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let updated =
        sqlx::query("UPDATE review_queue SET state = 'resolved' WHERE id = $1 AND state = 'open'")
            .bind(id)
            .execute(&state.pool)
            .await?
            .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!("open review item {id}")));
    }
    Ok(Json(json!({ "id": id, "state": "resolved" })))
}
