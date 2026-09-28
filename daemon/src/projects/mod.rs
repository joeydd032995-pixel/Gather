//! Projects: a folder or `.zip` uploaded as a whole and kept as the tree it
//! came in (project → folders → files).
//!
//! Each file Gather can read becomes an ordinary artifact through the same
//! path as a single upload, so extraction, deduplication, entities and the
//! graph work exactly as for any file. The project only adds where each file
//! sat, and an honest record of every file it held: read, the same as a file
//! already in Gather, or skipped with the reason.

pub mod archive;
pub mod paths;
pub mod store;

use serde::Serialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::ingest::ingest_one_file;
use crate::AppState;

use store::FileOutcome;

/// What happened to one file sent to a project.
#[derive(Debug, Clone, Serialize)]
pub struct ItemResult {
    pub path: String,
    /// `ingested`, `deduplicated`, `skipped`, `failed`, or `ignored` (tooling
    /// and clutter such as `.git` or `.DS_Store`, not recorded at all).
    pub status: String,
    pub kind: Option<String>,
    pub artifact_id: Option<Uuid>,
    pub detail: Option<String>,
    pub segments: usize,
}

impl ItemResult {
    fn new(path: &str, status: &str, detail: Option<String>) -> Self {
        ItemResult {
            path: path.to_string(),
            status: status.to_string(),
            kind: None,
            artifact_id: None,
            detail,
            segments: 0,
        }
    }
}

/// Record `outcome` for the file at `path`. A path that clashes with the
/// tree (a file where a folder is, or the other way round) can't be
/// recorded; that comes back as the reason, so the file is reported skipped
/// rather than failing the whole upload.
async fn record(
    state: &AppState,
    project: Uuid,
    path: &str,
    outcome: &FileOutcome,
) -> Result<Option<String>, ApiError> {
    match store::record_file(&state.pool, project, path, outcome).await {
        Ok(_) => Ok(None),
        Err(ApiError::BadRequest(reason)) => Ok(Some(reason)),
        Err(e) => Err(e),
    }
}

async fn skip(
    state: &AppState,
    project: Uuid,
    path: &str,
    status: &'static str,
    reason: String,
    byte_size: Option<i64>,
) -> Result<ItemResult, ApiError> {
    let outcome = FileOutcome {
        status,
        artifact_id: None,
        detail: Some(reason.clone()),
        byte_size,
    };
    Ok(match record(state, project, path, &outcome).await? {
        None => ItemResult::new(path, status, Some(reason)),
        Some(clash) => ItemResult::new(path, "skipped", Some(clash)),
    })
}

/// Read one file into `project` at `raw_path` (relative to the project).
pub async fn add_file(
    state: &AppState,
    project: Uuid,
    job_id: Uuid,
    raw_path: &str,
    declared_type: Option<String>,
    bytes: &[u8],
) -> Result<ItemResult, ApiError> {
    let path = match paths::normalize(raw_path) {
        Ok(p) => p,
        Err(reason) => return Ok(ItemResult::new(raw_path, "skipped", Some(reason.into()))),
    };
    let size = Some(bytes.len() as i64);
    match paths::screen(&path) {
        paths::Screen::Ignore => return Ok(ItemResult::new(&path, "ignored", None)),
        paths::Screen::Skip(reason) => {
            return skip(state, project, &path, "skipped", reason.into(), size).await
        }
        paths::Screen::Read => {}
    }
    if let Some(reason) = store::clash(&state.pool, project, &path).await? {
        return Ok(ItemResult::new(&path, "skipped", Some(reason)));
    }
    if bytes.is_empty() {
        return skip(
            state,
            project,
            &path,
            "skipped",
            "the file is empty".into(),
            size,
        )
        .await;
    }
    let name = paths::file_name(&path);
    match ingest_one_file(state, job_id, "file", name, declared_type, bytes).await {
        Ok(r) => {
            let status = if r.deduplicated {
                "deduplicated"
            } else {
                "ingested"
            };
            let outcome = FileOutcome {
                status,
                artifact_id: r.artifact_id,
                detail: None,
                byte_size: size,
            };
            if let Some(clash) = record(state, project, &path, &outcome).await? {
                return Ok(ItemResult::new(&path, "skipped", Some(clash)));
            }
            Ok(ItemResult {
                path,
                status: status.to_string(),
                kind: r.kind,
                artifact_id: r.artifact_id,
                detail: None,
                segments: r.segments,
            })
        }
        Err(ApiError::UnsupportedMedia(_)) => {
            let reason = "Gather can't read this kind of file yet".to_string();
            skip(state, project, &path, "skipped", reason, size).await
        }
        Err(ApiError::BadRequest(reason) | ApiError::PayloadTooLarge(reason)) => {
            skip(state, project, &path, "skipped", reason, size).await
        }
        Err(e) => {
            tracing::error!(error = %e, path, "project file could not be stored");
            skip(
                state,
                project,
                &path,
                "failed",
                "it couldn't be stored; try adding it again".into(),
                size,
            )
            .await
        }
    }
}

/// The result of unpacking a project `.zip`.
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub project: store::ProjectSummary,
    pub files: Vec<ItemResult>,
    /// Why unpacking stopped early, if it did.
    pub stopped: Option<String>,
}

/// Unpack `zip` into a new project. Its name is `name`, else the folder the
/// archive holds, else the archive's file name.
pub async fn import_zip(
    state: &AppState,
    job_id: Uuid,
    name: Option<String>,
    archive_name: &str,
    zip: Vec<u8>,
) -> Result<ImportReport, ApiError> {
    let limits = archive::Limits {
        max_files: state.config.project_max_files,
        max_file_bytes: (state.config.max_upload_mb * 1024 * 1024) as u64,
        max_total_bytes: (state.config.project_max_mb * 1024 * 1024) as u64,
    };
    let mut opened = archive::open(zip, limits).map_err(ApiError::BadRequest)?;
    let stem = paths::file_name(&archive_name.replace('\\', "/"))
        .trim_end_matches(".zip")
        .trim_end_matches(".ZIP")
        .to_string();
    let name = name
        .filter(|n| !n.trim().is_empty())
        .or(opened.root.clone())
        .unwrap_or(if stem.is_empty() {
            "Project".into()
        } else {
            stem
        });
    let project = store::create(&state.pool, &name, "zip").await?;
    let mut files = Vec::new();
    let mut stopped = None;
    while let Some(entry) = opened.entries.recv().await {
        match entry {
            archive::Entry::Folder(path) => {
                store::ensure_folder(&state.pool, project.id, &path).await?;
            }
            archive::Entry::File { path, bytes } => {
                let declared = mime_guess::from_path(&path)
                    .first()
                    .map(|m| m.essence_str().to_string());
                files.push(add_file(state, project.id, job_id, &path, declared, &bytes).await?);
            }
            archive::Entry::Skipped { path, reason, size } => {
                let size = size.map(|s| s as i64);
                match paths::normalize(&path) {
                    Ok(p) => {
                        files.push(skip(state, project.id, &p, "skipped", reason, size).await?)
                    }
                    Err(_) => files.push(ItemResult::new(&path, "skipped", Some(reason))),
                }
            }
            archive::Entry::Stopped(reason) => stopped = Some(reason),
        }
    }
    let project = store::summary(&state.pool, project.id).await?;
    Ok(ImportReport {
        project,
        files,
        stopped,
    })
}
