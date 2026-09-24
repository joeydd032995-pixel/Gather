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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::cluster::survivor_key;
use crate::entities::{dismiss_suggestion_in, merge_entities_in};
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
        // score snapshots the confidence the user judged, so the tuner learns
        // from what was actually labelled even if the unit is re-scored later.
        "INSERT INTO unit_feedback (target_kind, target_id, action, corrected, note, score) \
         VALUES ('unit', $1, $2, $3, $4, \
                 (SELECT confidence FROM atomic_units WHERE id = $1))",
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

/// Retract a unit (and the relationships it asserted) and record a negative
/// label. Shared by the REST handler and the review tray.
pub async fn reject_unit_core(pool: &PgPool, id: Uuid, note: Option<&str>) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    unit_status(&mut tx, id).await?;
    sqlx::query("UPDATE atomic_units SET status = 'retracted' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Relationships this unit asserted must go inactive too, or the graph
    // (which filters on relationship status) keeps showing the rejected claim.
    // Same propagation the contradiction resolver does for a superseded unit.
    deactivate_relationships(&mut tx, id).await?;
    record_feedback(&mut tx, id, "reject", None, note).await?;
    tx.commit().await?;
    Ok(())
}

/// POST /units/{id}/reject — retract the unit and record a negative label.
pub async fn reject_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    reject_unit_core(&state.pool, id, note.as_deref()).await?;
    Ok(Json(json!({ "id": id, "status": "retracted" })))
}

/// Retract the relationships a unit asserts (graph edges hang off `atomic_unit_id`).
async fn deactivate_relationships(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    unit_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE relationships SET status = 'retracted' \
         WHERE atomic_unit_id = $1 AND status = 'active'",
    )
    .bind(unit_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Reactivate the relationships a unit asserts, on restore.
async fn reactivate_relationships(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    unit_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE relationships SET status = 'active' \
         WHERE atomic_unit_id = $1 AND status = 'retracted'",
    )
    .bind(unit_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Undo a reject: reactivate a retracted unit and its relationships, recorded
/// as a keep. Shared by REST and gRPC.
pub async fn restore_unit_core(
    pool: &PgPool,
    id: Uuid,
    note: Option<&str>,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    let status = unit_status(&mut tx, id).await?;
    // Restore is the inverse of reject, nothing else. A unit superseded by a
    // contradiction resolution carries superseded_by_unit_id / valid_to that
    // this endpoint must not silently strip, so only a retracted unit qualifies.
    if status != "retracted" {
        return Err(ApiError::BadRequest(format!(
            "only a retracted unit can be restored; unit {id} is '{status}'"
        )));
    }
    sqlx::query("UPDATE atomic_units SET status = 'active' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    reactivate_relationships(&mut tx, id).await?;
    record_feedback(&mut tx, id, "confirm", None, note).await?;
    tx.commit().await?;
    Ok(())
}

/// POST /units/{id}/restore — undo a reject.
pub async fn restore_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    restore_unit_core(&state.pool, id, note.as_deref()).await?;
    Ok(Json(json!({ "id": id, "status": "active" })))
}

/// Record a positive label without changing the unit; returns its status.
/// Shared by the REST handler and the review tray.
pub async fn confirm_unit_core(
    pool: &PgPool,
    id: Uuid,
    note: Option<&str>,
) -> Result<String, ApiError> {
    let mut tx = pool.begin().await?;
    let status = unit_status(&mut tx, id).await?;
    record_feedback(&mut tx, id, "confirm", None, note).await?;
    tx.commit().await?;
    Ok(status)
}

/// POST /units/{id}/confirm — positive label, no state change.
pub async fn confirm_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    let status = confirm_unit_core(&state.pool, id, note.as_deref()).await?;
    Ok(Json(json!({ "id": id, "status": status })))
}

/// Correct a unit's statement; returns the stored (trimmed) statement. Records
/// the edit and re-hashes for dedup; a collision with an existing statement is
/// a BadRequest. Shared by REST and gRPC.
pub async fn edit_unit_core(
    pool: &PgPool,
    id: Uuid,
    statement: &str,
    note: Option<&str>,
) -> Result<String, ApiError> {
    let statement = statement.trim().to_string();
    if statement.is_empty() {
        return Err(ApiError::BadRequest(
            "statement must not be empty".to_string(),
        ));
    }
    let hash = hex::encode(Sha256::digest(normalize_statement(&statement)));

    let mut tx = pool.begin().await?;
    // Lock the row and capture the pre-edit statement so the correction is
    // reversible: the feedback row keeps both before and after.
    let before: Option<(String,)> =
        sqlx::query_as("SELECT statement FROM atomic_units WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((before,)) = before else {
        return Err(ApiError::NotFound(format!("unit {id}")));
    };

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

    // Changing the text invalidates derived state: the old embedding no longer
    // matches (search would rank the new text by the old vector) and the
    // contradiction scanner must re-examine the unit (it only picks up rows
    // with a null cursor).
    sqlx::query(
        "UPDATE atomic_units \
         SET statement = $2, statement_hash = $3, extraction_method = 'manual', \
             embedding = NULL, contradiction_scanned_at = NULL, \
             clustered_at = NULL, topic_cluster_id = NULL \
         WHERE id = $1",
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
        Some(json!({ "before": before, "after": statement })),
        note,
    )
    .await?;
    tx.commit().await?;
    Ok(statement)
}

/// PATCH /units/{id} — correct a unit's statement.
pub async fn edit_unit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<EditRequest>,
) -> Result<Json<Value>, ApiError> {
    let statement = edit_unit_core(&state.pool, id, &req.statement, req.note.as_deref()).await?;
    Ok(Json(json!({ "id": id, "statement": statement })))
}

#[derive(Deserialize)]
pub struct ReviewListParams {
    pub limit: Option<i64>,
}

/// One open item in the review tray.
#[derive(Debug, Serialize)]
pub struct ReviewEntry {
    pub id: Uuid,
    pub target_kind: String,
    pub target_id: Uuid,
    pub reason: String,
    pub info_gain: f32,
    pub signals: Value,
    /// The unit's statement, for unit items.
    pub statement: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The optional hold tray, highest info_gain first. Unit targets carry their
/// statement so the tray is readable without a second call.
pub async fn list_review_core(pool: &PgPool, limit: i64) -> Result<Vec<ReviewEntry>, ApiError> {
    let rows = sqlx::query(
        "SELECT r.id, r.target_kind, r.target_id, r.reason, r.info_gain, r.signals, \
                r.created_at, u.statement AS unit_statement \
         FROM review_queue r \
         LEFT JOIN atomic_units u ON r.target_kind = 'unit' AND u.id = r.target_id \
         WHERE r.state = 'open' \
         ORDER BY r.info_gain DESC, r.created_at \
         LIMIT $1",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ReviewEntry {
            id: r.get("id"),
            target_kind: r.get("target_kind"),
            target_id: r.get("target_id"),
            reason: r.get("reason"),
            info_gain: r.get("info_gain"),
            signals: r.get("signals"),
            statement: r.get("unit_statement"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// GET /review — the optional hold tray.
pub async fn list_review(
    State(state): State<AppState>,
    Query(params): Query<ReviewListParams>,
) -> Result<Json<Value>, ApiError> {
    let items = list_review_core(&state.pool, params.limit.unwrap_or(100)).await?;
    Ok(Json(json!({ "items": items })))
}

/// Close a tray entry without acting on the item (no label).
pub async fn resolve_review_core(pool: &PgPool, id: Uuid) -> Result<(), ApiError> {
    let updated =
        sqlx::query("UPDATE review_queue SET state = 'resolved' WHERE id = $1 AND state = 'open'")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!("open review item {id}")));
    }
    Ok(())
}

/// POST /review/{id}/resolve — dismiss a tray entry without acting on the item.
pub async fn resolve_review(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    resolve_review_core(&state.pool, id).await?;
    Ok(Json(json!({ "id": id, "state": "resolved" })))
}

/// An open tray entry, locked in by id.
struct ReviewItem {
    target_kind: String,
    target_id: Uuid,
    reason: String,
    signals: Value,
}

async fn open_review_item(pool: &PgPool, id: Uuid) -> Result<ReviewItem, ApiError> {
    let row = sqlx::query(
        "SELECT target_kind, target_id, reason, signals FROM review_queue \
         WHERE id = $1 AND state = 'open'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("open review item {id}")))?;
    Ok(ReviewItem {
        target_kind: row.get("target_kind"),
        target_id: row.get("target_id"),
        reason: row.get("reason"),
        signals: row.get("signals"),
    })
}

/// The two entities and score behind a held merge pair.
fn merge_pair(signals: &Value) -> Result<(Uuid, Uuid, Option<f32>), ApiError> {
    let id = |field: &str| {
        signals
            .get(field)
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| ApiError::BadRequest(format!("merge review item lacks '{field}'")))
    };
    let score = signals
        .get("score")
        .and_then(Value::as_f64)
        .map(|v| v as f32);
    Ok((id("a")?, id("b")?, score))
}

/// Label a merge pair (the tuner's training signal) and close its tray entry,
/// inside the caller's transaction so the label commits with the action.
async fn record_merge_verdict(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    review_id: Uuid,
    item: &ReviewItem,
    action: &str,
    score: Option<f32>,
    note: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO unit_feedback (target_kind, target_id, action, corrected, note, score) \
         VALUES ('merge', $1, $2, $3, $4, $5)",
    )
    .bind(item.target_id)
    .bind(action)
    .bind(&item.signals)
    .bind(note)
    .bind(score)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE review_queue SET state = 'resolved' WHERE id = $1")
        .bind(review_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Pick the merge survivor with the same rule the auto-merge uses.
async fn survivor_and_loser(pool: &PgPool, a: Uuid, b: Uuid) -> Result<(Uuid, Uuid), ApiError> {
    let rows: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, name, kind::text FROM entities WHERE id IN ($1, $2)")
            .bind(a)
            .bind(b)
            .fetch_all(pool)
            .await?;
    let find = |id: Uuid| {
        rows.iter()
            .find(|(rid, _, _)| *rid == id)
            .ok_or_else(|| ApiError::NotFound(format!("entity {id}")))
    };
    let (_, name_a, kind_a) = find(a)?;
    let (_, name_b, kind_b) = find(b)?;
    if survivor_key(kind_a, name_a) >= survivor_key(kind_b, name_b) {
        Ok((a, b))
    } else {
        Ok((b, a))
    }
}

/// What a tray action did.
#[derive(Debug, Default, Serialize)]
pub struct ReviewOutcome {
    pub id: Uuid,
    /// confirmed | retracted | merged | dismissed
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loser: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<[Uuid; 2]>,
}

/// Agree with a held item: keep a unit, or perform a held merge. Either way
/// the verdict becomes a positive tuning label. Shared by REST and gRPC.
pub async fn accept_review_core(
    pool: &PgPool,
    id: Uuid,
    note: Option<String>,
) -> Result<ReviewOutcome, ApiError> {
    let item = open_review_item(pool, id).await?;
    match (item.target_kind.as_str(), item.reason.as_str()) {
        ("unit", _) => {
            confirm_unit_core(pool, item.target_id, note.as_deref()).await?;
            Ok(ReviewOutcome {
                id,
                action: "confirmed",
                unit_id: Some(item.target_id),
                ..ReviewOutcome::default()
            })
        }
        ("entity", "merge-band") => {
            let (a, b, score) = merge_pair(&item.signals)?;
            let (winner, loser) = survivor_and_loser(pool, a, b).await?;
            // Merge and label in one transaction: a merge without its label
            // would leave the tray entry open and un-retryable.
            let mut tx = pool.begin().await?;
            merge_entities_in(
                &mut tx,
                winner,
                loser,
                Some(
                    note.clone()
                        .unwrap_or_else(|| "accepted from review tray".to_string()),
                ),
                Some("local-user".to_string()),
            )
            .await?;
            record_merge_verdict(&mut tx, id, &item, "confirm", score, note.as_deref()).await?;
            tx.commit().await?;
            metrics::counter!("gather_entity_merges_total").increment(1);
            Ok(ReviewOutcome {
                id,
                action: "merged",
                winner: Some(winner),
                loser: Some(loser),
                ..ReviewOutcome::default()
            })
        }
        _ => Err(ApiError::BadRequest(
            "this review item cannot be accepted as a whole; act on its members individually"
                .to_string(),
        )),
    }
}

/// POST /review/{id}/accept
pub async fn accept_review(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<ReviewOutcome>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    Ok(Json(accept_review_core(&state.pool, id, note).await?))
}

/// Disagree with a held item: retract a unit, or dismiss a held merge pair so
/// it is never suggested again. Either way the verdict becomes a negative
/// tuning label. Shared by REST and gRPC.
pub async fn reject_review_core(
    pool: &PgPool,
    id: Uuid,
    note: Option<String>,
) -> Result<ReviewOutcome, ApiError> {
    let item = open_review_item(pool, id).await?;
    match (item.target_kind.as_str(), item.reason.as_str()) {
        ("unit", _) => {
            reject_unit_core(pool, item.target_id, note.as_deref()).await?;
            Ok(ReviewOutcome {
                id,
                action: "retracted",
                unit_id: Some(item.target_id),
                ..ReviewOutcome::default()
            })
        }
        ("entity", "merge-band") => {
            let (a, b, score) = merge_pair(&item.signals)?;
            let mut tx = pool.begin().await?;
            dismiss_suggestion_in(&mut tx, a, b, note.clone(), Some("local-user".to_string()))
                .await?;
            record_merge_verdict(&mut tx, id, &item, "reject", score, note.as_deref()).await?;
            tx.commit().await?;
            Ok(ReviewOutcome {
                id,
                action: "dismissed",
                pair: Some([a, b]),
                ..ReviewOutcome::default()
            })
        }
        _ => {
            sqlx::query("UPDATE review_queue SET state = 'dismissed' WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            Ok(ReviewOutcome {
                id,
                action: "dismissed",
                ..ReviewOutcome::default()
            })
        }
    }
}

/// POST /review/{id}/reject
pub async fn reject_review(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<NoteRequest>>,
) -> Result<Json<ReviewOutcome>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    Ok(Json(reject_review_core(&state.pool, id, note).await?))
}
