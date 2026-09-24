//! Read surface for clusters (autonomous pipeline, Phase B): the "arrangement"
//! of the brain into entity and topic groups.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

#[derive(Deserialize)]
pub struct ClusterListParams {
    /// Optional filter: 'entity', 'topic', 'photo_dup', 'album' or 'photo_topic'.
    pub kind: Option<String>,
    pub limit: Option<i64>,
}

/// GET /clusters — clusters newest-updated first, optionally filtered by kind.
pub async fn list_clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.unwrap_or(100).clamp(1, 500);
    let rows = sqlx::query(
        "SELECT id, kind, label, cohesion, size, representative_id, updated_at FROM clusters \
         WHERE ($1::text IS NULL OR kind = $1) \
         ORDER BY updated_at DESC LIMIT $2",
    )
    .bind(&params.kind)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "kind": r.get::<String, _>("kind"),
                "label": r.get::<String, _>("label"),
                "cohesion": r.get::<f32, _>("cohesion"),
                "size": r.get::<i32, _>("size"),
                "representative_id": r.get::<Option<Uuid>, _>("representative_id"),
                "updated_at": r.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// GET /clusters/{id} — one cluster and its members. Unit members carry their
/// statement and image members their file name and capture time, so the group
/// is readable in a single call.
pub async fn get_cluster(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let cluster = sqlx::query(
        "SELECT id, kind, label, cohesion, size, representative_id, created_at, updated_at \
         FROM clusters WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("cluster {id}")))?;

    let members = sqlx::query(
        "SELECT m.member_kind, m.member_id, m.sim, u.statement AS unit_statement, \
                a.original_filename, i.taken_at, i.caption \
         FROM cluster_members m \
         LEFT JOIN atomic_units u ON m.member_kind = 'unit' AND u.id = m.member_id \
         LEFT JOIN images i ON m.member_kind = 'image' AND i.id = m.member_id \
         LEFT JOIN artifacts a ON a.id = i.artifact_id \
         WHERE m.cluster_id = $1 ORDER BY m.sim DESC, i.taken_at NULLS LAST, m.member_id",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    let member_items: Vec<Value> = members
        .iter()
        .map(|r| {
            json!({
                "member_kind": r.get::<String, _>("member_kind"),
                "member_id": r.get::<Uuid, _>("member_id"),
                "sim": r.get::<f32, _>("sim"),
                "statement": r.get::<Option<String>, _>("unit_statement"),
                "filename": r.get::<Option<String>, _>("original_filename"),
                "taken_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("taken_at"),
                "caption": r.get::<Option<String>, _>("caption"),
            })
        })
        .collect();

    Ok(Json(json!({
        "id": cluster.get::<Uuid, _>("id"),
        "kind": cluster.get::<String, _>("kind"),
        "label": cluster.get::<String, _>("label"),
        "cohesion": cluster.get::<f32, _>("cohesion"),
        "size": cluster.get::<i32, _>("size"),
        "representative_id": cluster.get::<Option<Uuid>, _>("representative_id"),
        "created_at": cluster.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": cluster.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
        "members": member_items,
    })))
}
