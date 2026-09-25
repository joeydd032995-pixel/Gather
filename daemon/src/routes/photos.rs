//! Photo read surface (autonomous pipeline, Phase D). Duplicate groups, albums
//! and visual topics are clusters (`GET /clusters?kind=photo_dup|album|photo_topic`);
//! this module adds the one thing a UI needs beyond that: small thumbnails,
//! rendered locally on demand.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

/// Longest side of a thumbnail, px.
const THUMBNAIL_SIZE: u32 = 256;
const THUMBNAIL_QUALITY: u8 = 80;

fn render_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    crate::photo::decode::render_jpeg(bytes, THUMBNAIL_SIZE, THUMBNAIL_QUALITY)
}

/// A JPEG thumbnail, at most 256 px on its long side. `UnsupportedMedia` when
/// the format can't be decoded locally (e.g. HEIC). Shared by REST and gRPC.
pub async fn thumbnail_core(pool: &PgPool, id: Uuid) -> Result<Vec<u8>, ApiError> {
    let bytes: Option<Option<Vec<u8>>> = sqlx::query_scalar(
        "SELECT a.raw_content FROM images i JOIN artifacts a ON a.id = i.artifact_id \
         WHERE i.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let bytes = bytes
        .ok_or_else(|| ApiError::NotFound(format!("image {id}")))?
        .ok_or_else(|| ApiError::NotFound(format!("image {id} has no stored bytes")))?;

    tokio::task::spawn_blocking(move || render_thumbnail(&bytes))
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| ApiError::UnsupportedMedia("image cannot be decoded locally".to_string()))
}

/// GET /images/{id}/thumbnail
pub async fn thumbnail(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let jpeg = thumbnail_core(&state.pool, id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        jpeg,
    ))
}
