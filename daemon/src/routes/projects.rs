//! Projects over REST: create one, send it files one at a time with their
//! paths, or unpack a `.zip`; list projects and read one's folder tree.

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::error::ApiError;
use crate::projects::{self, paths, store, Budget, ImportReport, ItemResult};
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
    /// Why unpacking a `.zip` among the files stopped early, if it did.
    pub stopped: Option<String>,
}

/// POST /projects/{id}/files — multipart. Each file part may be preceded by
/// a text part named `path` holding its path inside the project
/// (`docs/plan.md`); without one, the part's file name is used as the path.
/// A `.zip` is unpacked where it sits. Text parts that describe what the
/// sender didn't send:
/// - `left_out`: a folder left out whole (`web/node_modules`); only the
///   folder names Gather leaves out are taken;
/// - `withheld`: a file not sent because it looks like it holds keys or
///   passwords (`.env`); only paths Gather wouldn't read are taken;
/// - `folder`: an empty folder, so the tree keeps it.
pub async fn add_files(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<FilesResponse>), ApiError> {
    store::exists(&state.pool, id).await?;
    // One request at a time per project, so two can't both pass the check
    // for a path that clashes with the other's.
    let _lock = projects::lock(id).await;
    let job_id = create_job(&state.pool, "rest").await?;
    let mut budget = Budget::new(&state.config);
    let mut files = Vec::new();
    let read = read_parts(&state, id, job_id, multipart, &mut budget, &mut files).await;
    // Every way out finishes the job, so none is left "processing".
    let outcome = match read {
        Ok(0) => Err(ApiError::BadRequest(
            "multipart body contained no file parts".into(),
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    };
    let ok = outcome.is_ok() && files.iter().all(|f| f.status != "failed");
    let summary = match &outcome {
        Ok(()) => json!({ "project": id, "files": files.len() }),
        Err(e) => json!({ "project": id, "files": files.len(), "error": e.to_string() }),
    };
    finish_job(&state.pool, job_id, ok, summary).await?;
    outcome?;
    Ok((
        StatusCode::ACCEPTED,
        Json(FilesResponse {
            project_id: id,
            job_id,
            files,
            stopped: budget.stopped,
        }),
    ))
}

/// The parts of one `POST /projects/{id}/files`, taken in order. Returns how
/// many parts there were.
async fn read_parts(
    state: &AppState,
    id: Uuid,
    job_id: Uuid,
    mut multipart: Multipart,
    budget: &mut Budget,
    files: &mut Vec<ItemResult>,
) -> Result<usize, ApiError> {
    let max_bytes = state.config.max_upload_mb * 1024 * 1024;
    let mut pending_path: Option<String> = None;
    let mut parts = 0;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed multipart body: {e}")))?
    {
        let part = field.name().unwrap_or("file").to_string();
        let described = ["path", "left_out", "withheld", "folder"].contains(&part.as_str());
        if field.file_name().is_none() && described {
            let bytes = read_part(field, &part, 8 * 1024, None).await?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if part == "path" {
                pending_path = Some(text);
                continue;
            }
            parts += 1;
            let path = paths::normalize(&text);
            match (part.as_str(), path) {
                (_, Err(reason)) => files.push(ItemResult::skipped(&text, reason)),
                ("left_out", Ok(folder)) => match paths::left_out_reason(paths::file_name(&folder))
                {
                    Some(reason) => {
                        files.push(projects::left_out(state, id, &folder, reason).await?)
                    }
                    None => files.push(ItemResult::skipped(
                        &text,
                        "not a folder Gather leaves out; send its files instead",
                    )),
                },
                ("withheld", Ok(path)) => files.push(projects::withheld(state, id, &path).await?),
                (_, Ok(folder)) => {
                    if let Err(ApiError::BadRequest(clash)) =
                        store::ensure_folder(&state.pool, id, &folder).await
                    {
                        files.push(ItemResult::skipped(&folder, &clash));
                    }
                }
            }
            continue;
        }
        parts += 1;
        let path = pending_path
            .take()
            .or_else(|| field.file_name().map(String::from))
            .unwrap_or_else(|| "unnamed".to_string());
        let declared = field
            .content_type()
            .map(String::from)
            .filter(|t| t != "application/octet-stream");
        let bytes = read_part(field, &part, max_bytes, None).await?;
        projects::add_file(state, id, job_id, &path, declared, bytes, 0, budget, files).await?;
    }
    Ok(parts)
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
