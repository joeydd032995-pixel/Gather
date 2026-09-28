//! Projects over REST: create one, send it files one at a time with their
//! paths, or unpack a `.zip`; list projects and read one's folder tree.

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::error::ApiError;
use crate::projects::{self, store, ImportReport, ItemResult};
use crate::routes::ingest::{create_job, finish_job, read_part};
use crate::AppState;

#[derive(Deserialize)]
pub struct CreateRequest {
    pub name: String,
}

/// POST /projects — `{ "name": "Atlas" }`. Files are added afterwards with
/// `POST /projects/{id}/files`.
pub async fn create_project(
    State(state): State<AppState>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<store::ProjectSummary>), ApiError> {
    let project = store::create(&state.pool, &req.name, "folder").await?;
    Ok((StatusCode::CREATED, Json(project)))
}

/// GET /projects
pub async fn list_projects(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!({ "items": store::list(&state.pool).await? })))
}

/// GET /projects/{id} — the project and every folder and file in it.
pub async fn get_project(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<store::ProjectDetail>, ApiError> {
    Ok(Json(store::get(&state.pool, id).await?))
}

/// DELETE /projects/{id} — removes the project's tree; its files stay in
/// Gather.
pub async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    store::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct FilesResponse {
    pub project_id: Uuid,
    pub job_id: Uuid,
    pub files: Vec<ItemResult>,
}

/// POST /projects/{id}/files — multipart. Each file part may be preceded by
/// a text part named `path` holding its path inside the project
/// (`docs/plan.md`); without one, the part's file name is used as the path.
pub async fn add_files(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FilesResponse>), ApiError> {
    store::exists(&state.pool, id).await?;
    let max_bytes = state.config.max_upload_mb * 1024 * 1024;
    let job_id = create_job(&state.pool, "rest").await?;
    let mut pending_path: Option<String> = None;
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed multipart body: {e}")))?
    {
        let part = field.name().unwrap_or("file").to_string();
        if part == "path" && field.file_name().is_none() {
            let bytes = read_part(field, &part, 8 * 1024, None).await?;
            pending_path = Some(String::from_utf8_lossy(&bytes).into_owned());
            continue;
        }
        let path = pending_path
            .take()
            .or_else(|| field.file_name().map(String::from))
            .unwrap_or_else(|| "unnamed".to_string());
        let declared = field
            .content_type()
            .map(String::from)
            .filter(|t| t != "application/octet-stream");
        let bytes = read_part(field, &part, max_bytes, None).await?;
        files.push(projects::add_file(&state, id, job_id, &path, declared, &bytes).await?);
    }
    let ok = files.iter().all(|f| f.status != "failed");
    finish_job(
        &state.pool,
        job_id,
        ok,
        json!({ "project": id, "files": files.len() }),
    )
    .await?;
    if files.is_empty() {
        return Err(ApiError::BadRequest(
            "multipart body contained no file parts".into(),
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(FilesResponse {
            project_id: id,
            job_id,
            files,
        }),
    ))
}

/// POST /projects/import — multipart with one `.zip` part and, optionally, a
/// text part `name`. The archive becomes a new project, folders and all.
pub async fn import_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImportReport>), ApiError> {
    let max_bytes = state.config.max_upload_mb * 1024 * 1024;
    let mut name: Option<String> = None;
    let mut archive: Option<(String, Vec<u8>)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed multipart body: {e}")))?
    {
        let part = field.name().unwrap_or("file").to_string();
        if part == "name" && field.file_name().is_none() {
            let bytes = read_part(field, &part, 1024, None).await?;
            name = Some(String::from_utf8_lossy(&bytes).into_owned());
            continue;
        }
        if archive.is_some() {
            return Err(ApiError::BadRequest("send one .zip per import".into()));
        }
        let file_name = field.file_name().unwrap_or("project.zip").to_string();
        let bytes = read_part(field, &part, max_bytes, None).await?;
        archive = Some((file_name, bytes));
    }
    let (file_name, bytes) =
        archive.ok_or_else(|| ApiError::BadRequest("no .zip in the request".into()))?;
    let job_id = create_job(&state.pool, "rest").await?;
    let report = match projects::import_zip(&state, job_id, name, &file_name, bytes).await {
        Ok(r) => r,
        Err(e) => {
            finish_job(
                &state.pool,
                job_id,
                false,
                json!({ "error": e.to_string() }),
            )
            .await?;
            return Err(e);
        }
    };
    finish_job(
        &state.pool,
        job_id,
        report.files.iter().all(|f| f.status != "failed"),
        json!({ "project": report.project.id, "files": report.files.len() }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(report)))
}
