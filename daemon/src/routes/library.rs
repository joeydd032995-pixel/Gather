//! Browsing endpoints: an artifact's readable content and the whole-collection
//! graph overview (`crate::library` holds the queries).

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::library::{self, ArtifactContent, ArtifactDigest, GraphOverview};
use crate::AppState;

#[derive(Deserialize)]
pub struct ContentParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// GET /api/v1/artifacts/{id}/content
pub async fn artifact_content(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<ContentParams>,
) -> Result<Json<ArtifactContent>, ApiError> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    Ok(Json(
        library::artifact_content(&state.pool, id, limit, offset).await?,
    ))
}

/// GET /api/v1/artifacts/{id}/digest
pub async fn artifact_digest(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ArtifactDigest>, ApiError> {
    Ok(Json(library::artifact_digest(&state.pool, id).await?))
}

#[derive(Deserialize)]
pub struct OverviewParams {
    pub max_entities: Option<i64>,
    /// 0 leaves files out.
    pub max_files: Option<i64>,
    /// Most projects shown (default 150, max 1000; 0 leaves them out).
    pub max_projects: Option<i64>,
    /// false leaves projects out, like `max_projects=0`.
    pub projects: Option<bool>,
}

/// GET /api/v1/graph
pub async fn graph_overview(
    State(state): State<AppState>,
    Query(params): Query<OverviewParams>,
) -> Result<Json<GraphOverview>, ApiError> {
    let max_entities = params.max_entities.unwrap_or(150).clamp(1, 1000);
    let max_files = params.max_files.unwrap_or(100).clamp(0, 1000);
    Ok(Json(
        library::graph_overview(
            &state.pool,
            max_entities,
            max_files,
            if params.projects == Some(false) {
                0
            } else {
                params.max_projects.unwrap_or(150).clamp(0, 1000)
            },
            state.config.project_compare_max,
        )
        .await?,
    ))
}
