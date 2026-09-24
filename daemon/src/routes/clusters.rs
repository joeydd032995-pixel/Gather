//! Read surface for clusters (autonomous pipeline, Phases B and D): the
//! "arrangement" of the brain into entity, topic and photo groups. The cores
//! are shared by REST and gRPC.

use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

#[derive(Deserialize)]
pub struct ClusterListParams {
    /// Optional filter: 'entity', 'topic', 'photo_dup', 'album' or 'photo_topic'.
    pub kind: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ClusterSummary {
    pub id: Uuid,
    pub kind: String,
    pub label: String,
    pub cohesion: f32,
    pub size: i32,
    /// The member a UI should show for the group, when it has one.
    pub representative_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ClusterMember {
    pub member_kind: String,
    pub member_id: Uuid,
    pub sim: f32,
    /// Unit members: the statement.
    pub statement: Option<String>,
    /// Image members: file name, capture time and caption.
    pub filename: Option<String>,
    pub taken_at: Option<DateTime<Utc>>,
    pub caption: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClusterDetail {
    #[serde(flatten)]
    pub summary: ClusterSummary,
    pub created_at: DateTime<Utc>,
    pub members: Vec<ClusterMember>,
}

fn summary_from(r: &sqlx::postgres::PgRow) -> ClusterSummary {
    ClusterSummary {
        id: r.get("id"),
        kind: r.get("kind"),
        label: r.get("label"),
        cohesion: r.get("cohesion"),
        size: r.get("size"),
        representative_id: r.get("representative_id"),
        updated_at: r.get("updated_at"),
    }
}

/// Clusters newest-updated first, optionally filtered by kind.
pub async fn list_clusters_core(
    pool: &PgPool,
    kind: Option<&str>,
    limit: i64,
) -> Result<Vec<ClusterSummary>, ApiError> {
    let rows = sqlx::query(
        "SELECT id, kind, label, cohesion, size, representative_id, updated_at FROM clusters \
         WHERE ($1::text IS NULL OR kind = $1) \
         ORDER BY updated_at DESC LIMIT $2",
    )
    .bind(kind)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(summary_from).collect())
}

/// GET /clusters
pub async fn list_clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterListParams>,
) -> Result<Json<Value>, ApiError> {
    let items = list_clusters_core(
        &state.pool,
        params.kind.as_deref(),
        params.limit.unwrap_or(100),
    )
    .await?;
    Ok(Json(json!({ "items": items })))
}

/// One cluster and its members. Unit members carry their statement and image
/// members their file name, capture time and caption, so the group is
/// readable in a single call.
pub async fn get_cluster_core(pool: &PgPool, id: Uuid) -> Result<ClusterDetail, ApiError> {
    let cluster = sqlx::query(
        "SELECT id, kind, label, cohesion, size, representative_id, created_at, updated_at \
         FROM clusters WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
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
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| ClusterMember {
        member_kind: r.get("member_kind"),
        member_id: r.get("member_id"),
        sim: r.get("sim"),
        statement: r.get("unit_statement"),
        filename: r.get("original_filename"),
        taken_at: r.get("taken_at"),
        caption: r.get("caption"),
    })
    .collect();

    Ok(ClusterDetail {
        summary: summary_from(&cluster),
        created_at: cluster.get("created_at"),
        members,
    })
}

/// GET /clusters/{id}
pub async fn get_cluster(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ClusterDetail>, ApiError> {
    Ok(Json(get_cluster_core(&state.pool, id).await?))
}
