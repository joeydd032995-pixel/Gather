//! Projects: a folder or `.zip` uploaded as a whole and kept as the tree it
//! came in (project → folders → files).
//!
//! Every file becomes an ordinary artifact through the same path as a single
//! upload, so extraction, deduplication, entities and the graph work exactly
//! as for any file. A file Gather can't read text from is still kept, as it
//! is. A `.zip` inside a project is unpacked where it sits, as a folder of its
//! contents. The project adds where each file sat, and an honest record of
//! every file it held: read, the same as a file already in Gather, kept as it
//! is, or skipped with the reason. Folders left out whole (version-control
//! history, installed dependencies, tool caches) are in the tree too.

pub mod archive;
pub mod graph;
pub mod paths;
pub mod similarity;
pub mod store;

use serde::Serialize;
use uuid::Uuid;

use crate::config::Config;
use crate::error::ApiError;
use crate::extract::formats;
use crate::routes::ingest::{ingest_one_file, FileResult};
use crate::AppState;

use store::FileOutcome;

/// Most archives a file may sit inside and still be unpacked: a `.zip` in a
/// `.zip` in a `.zip` is unpacked, one deeper is kept as it is.
const MAX_NESTING: usize = 3;

/// What happened to one file sent to a project.
#[derive(Debug, Clone, Serialize)]
pub struct ItemResult {
    pub path: String,
    /// `ingested`, `deduplicated`, `stored` (kept as it is, no text read),
    /// `skipped`, `failed`, `left_out` (a folder left out whole; `path` is
    /// the folder), or `ignored` (clutter such as `.DS_Store`, not recorded).
    pub status: String,
    pub kind: Option<String>,
    pub artifact_id: Option<Uuid>,
    pub detail: Option<String>,
    pub segments: usize,
}

impl ItemResult {
    pub fn skipped(path: &str, reason: &str) -> Self {
        ItemResult::new(path, "skipped", Some(reason.to_string()))
    }

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

/// How much more one request may unpack from archives, shared by every
/// `.zip` in it however deeply nested, so archives inside archives can't
/// multiply the limits.
pub struct Budget {
    files: usize,
    bytes: u64,
    max_file_bytes: u64,
    /// Why unpacking stopped early, once it has.
    pub stopped: Option<String>,
}

impl Budget {
    pub fn new(config: &Config) -> Self {
        Budget {
            files: config.project_max_files,
            bytes: (config.project_max_mb * 1024 * 1024) as u64,
            max_file_bytes: (config.max_upload_mb * 1024 * 1024) as u64,
            stopped: None,
        }
    }

    fn limits(&self) -> archive::Limits {
        archive::Limits {
            max_files: self.files,
            max_file_bytes: self.max_file_bytes,
            max_total_bytes: self.bytes,
        }
    }

    /// Take one unpacked file of `size` bytes; `false` once the budget is
    /// spent, with the reason recorded.
    fn take(&mut self, size: u64) -> bool {
        if self.files == 0 || size > self.bytes {
            self.stopped.get_or_insert_with(|| {
                "the project's unpacking limit was reached; the rest wasn't read".to_string()
            });
            return false;
        }
        self.files -= 1;
        self.bytes -= size;
        true
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

/// Record the folder at `path` as left out whole, for `reason`.
pub async fn left_out(
    state: &AppState,
    project: Uuid,
    path: &str,
    reason: &str,
) -> Result<ItemResult, ApiError> {
    Ok(
        match store::record_left_out(&state.pool, project, path, reason).await {
            Ok(()) => ItemResult::new(path, "left_out", Some(reason.to_string())),
            Err(ApiError::BadRequest(clash)) => ItemResult::new(path, "skipped", Some(clash)),
            Err(e) => return Err(e),
        },
    )
}

/// A file the sender didn't send because Gather wouldn't read it anyway (it
/// looks like it holds keys or passwords): recorded as skipped, with why.
/// Any other path must be sent, not withheld.
pub async fn withheld(state: &AppState, project: Uuid, path: &str) -> Result<ItemResult, ApiError> {
    match paths::screen(path) {
        paths::Screen::Skip(reason) => {
            skip(state, project, path, "skipped", reason.into(), None).await
        }
        _ => Ok(ItemResult::skipped(
            path,
            "only files Gather wouldn't read can be withheld; send this one",
        )),
    }
}

/// Hold `project` for one request: requests adding to the same project run
/// one after another, so a path checked for clashes stays clear until it is
/// recorded. The daemon is the only writer to its database, so a lock in
/// this process is enough.
pub async fn lock(project: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex, Weak};
    static LOCKS: LazyLock<Mutex<HashMap<Uuid, Weak<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    let lock = {
        let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, held| held.strong_count() > 0);
        match locks.get(&project).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(project, Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

/// Add one file to `project` at `raw_path` (relative to the project),
/// pushing what happened to it, or to each file in it when it is a `.zip`,
/// onto `out`. `nesting` is how many archives it came out of.
#[allow(clippy::too_many_arguments)]
pub async fn add_file(
    state: &AppState,
    project: Uuid,
    job_id: Uuid,
    raw_path: &str,
    declared_type: Option<String>,
    bytes: Vec<u8>,
    nesting: usize,
    budget: &mut Budget,
    out: &mut Vec<ItemResult>,
) -> Result<(), ApiError> {
    let path = match paths::normalize(raw_path) {
        Ok(p) => p,
        Err(reason) => {
            out.push(ItemResult::new(raw_path, "skipped", Some(reason.into())));
            return Ok(());
        }
    };
    let size = Some(bytes.len() as i64);
    match paths::screen(&path) {
        paths::Screen::Read => {}
        paths::Screen::Ignore => {
            out.push(ItemResult::new(&path, "ignored", None));
            return Ok(());
        }
        paths::Screen::LeftOut { folder, reason } => {
            out.push(left_out(state, project, &folder, reason).await?);
            return Ok(());
        }
        paths::Screen::Skip(reason) => {
            out.push(skip(state, project, &path, "skipped", reason.into(), size).await?);
            return Ok(());
        }
    }
    let unpack = paths::is_zip(&path) && nesting < MAX_NESTING && !bytes.is_empty();
    if let Some(reason) = store::clash(&state.pool, project, &path, unpack).await? {
        out.push(ItemResult::new(&path, "skipped", Some(reason)));
        return Ok(());
    }
    if bytes.is_empty() {
        out.push(
            skip(
                state,
                project,
                &path,
                "skipped",
                "the file is empty".into(),
                size,
            )
            .await?,
        );
        return Ok(());
    }
    if unpack {
        return unpack_nested(
            state,
            project,
            job_id,
            path,
            declared_type,
            bytes,
            nesting,
            budget,
            out,
        )
        .await;
    }
    let note = (paths::is_zip(&path))
        .then(|| format!("a .zip inside {MAX_NESTING} others; kept as it is, not unpacked"));
    out.push(read_file(state, project, job_id, &path, declared_type, &bytes, note).await?);
    Ok(())
}

/// Read one file into Gather and record it. A file whose text Gather can't
/// read is kept as it is (`stored`) rather than left out: every file in a
/// project counts. `note` explains why a file is only kept, when that isn't
/// simply its kind.
async fn read_file(
    state: &AppState,
    project: Uuid,
    job_id: Uuid,
    path: &str,
    declared_type: Option<String>,
    bytes: &[u8],
    note: Option<String>,
) -> Result<ItemResult, ApiError> {
    let name = paths::file_name(path);
    let size = Some(bytes.len() as i64);
    let (kind, detail) = match note {
        Some(note) => ("file_other", Some(note)),
        None => {
            match ingest_one_file(state, job_id, "file", name, declared_type.clone(), bytes).await {
                Ok(r) => return finish(state, project, path, r, None, size).await,
                // Not a kind Gather knows by name: text is read as text, anything
                // else is kept as it is.
                Err(ApiError::UnsupportedMedia(_)) if formats::looks_like_text(bytes) => {
                    ("document_text", None)
                }
                Err(ApiError::UnsupportedMedia(_)) => ("file_other", None),
                // A known kind whose text couldn't be read (a damaged Word file,
                // binary content under a text name): the file is still kept.
                Err(ApiError::BadRequest(reason)) => (
                    "file_other",
                    Some(format!(
                        "kept as it is; its text couldn't be read: {reason}"
                    )),
                ),
                Err(e) => return not_stored(state, project, path, e, size).await,
            }
        }
    };
    match ingest_one_file(state, job_id, kind, name, declared_type, bytes).await {
        Ok(r) => finish(state, project, path, r, detail, size).await,
        Err(e) => not_stored(state, project, path, e, size).await,
    }
}

async fn finish(
    state: &AppState,
    project: Uuid,
    path: &str,
    r: FileResult,
    detail: Option<String>,
    size: Option<i64>,
) -> Result<ItemResult, ApiError> {
    let status = if r.deduplicated {
        "deduplicated"
    } else if r.kind.as_deref() == Some("file_other") {
        "stored"
    } else {
        "ingested"
    };
    let outcome = FileOutcome {
        status,
        artifact_id: r.artifact_id,
        detail: detail.clone(),
        byte_size: size,
    };
    if let Some(clash) = record(state, project, path, &outcome).await? {
        return Ok(ItemResult::new(path, "skipped", Some(clash)));
    }
    Ok(ItemResult {
        path: path.to_string(),
        status: status.to_string(),
        kind: r.kind,
        artifact_id: r.artifact_id,
        detail,
        segments: r.segments,
    })
}

async fn not_stored(
    state: &AppState,
    project: Uuid,
    path: &str,
    error: ApiError,
    size: Option<i64>,
) -> Result<ItemResult, ApiError> {
    match error {
        ApiError::BadRequest(reason)
        | ApiError::PayloadTooLarge(reason)
        | ApiError::UnsupportedMedia(reason) => {
            skip(state, project, path, "skipped", reason, size).await
        }
        e => {
            tracing::error!(error = %e, path, "project file could not be stored");
            let reason = "it couldn't be stored; try adding it again".to_string();
            skip(state, project, path, "failed", reason, size).await
        }
    }
}

fn join(base: Option<&str>, path: &str) -> String {
    match base {
        Some(base) => format!("{base}/{path}"),
        None => path.to_string(),
    }
}

/// Unpack the `.zip` at `path` into a folder of the same name, where it sits.
/// One that can't be unpacked (damaged, or the limits already spent) is kept
/// as it is instead.
#[allow(clippy::too_many_arguments)]
async fn unpack_nested(
    state: &AppState,
    project: Uuid,
    job_id: Uuid,
    path: String,
    declared_type: Option<String>,
    bytes: Vec<u8>,
    nesting: usize,
    budget: &mut Budget,
    out: &mut Vec<ItemResult>,
) -> Result<(), ApiError> {
    // Checked on the borrowed bytes first, so one that isn't a zip after all
    // can still be kept as it is without holding a second copy.
    let note = if budget.stopped.is_some() {
        Some("the project's unpacking limit was reached; kept as it is, not unpacked".to_string())
    } else {
        archive::check(&bytes)
            .err()
            .map(|reason| format!("couldn't be unpacked ({reason}); kept as it is"))
    };
    if let Some(note) = note {
        let kept = read_file(
            state,
            project,
            job_id,
            &path,
            declared_type,
            &bytes,
            Some(note),
        );
        out.push(kept.await?);
        return Ok(());
    }
    let opened = match archive::open(bytes, budget.limits()) {
        Ok(opened) => opened,
        // `check` passed, so this can't happen; say so rather than panic.
        Err(reason) => {
            out.push(skip(state, project, &path, "skipped", reason, None).await?);
            return Ok(());
        }
    };
    if let Err(ApiError::BadRequest(clash)) =
        store::ensure_folder(&state.pool, project, &path).await
    {
        out.push(ItemResult::new(&path, "skipped", Some(clash)));
        return Ok(());
    }
    // A zip holding one folder (`data.zip` → `data/…`) is unpacked without
    // repeating it: its contents go straight into the `data.zip` folder.
    unpack_into(
        state,
        project,
        job_id,
        Some(&path),
        opened,
        nesting + 1,
        budget,
        out,
    )
    .await
}

/// Take every entry of an opened archive into `project` under `base` (the
/// project root when `None`). Entries are at `nesting` archives deep.
#[allow(clippy::too_many_arguments)]
async fn unpack_into(
    state: &AppState,
    project: Uuid,
    job_id: Uuid,
    base: Option<&str>,
    mut opened: archive::Opened,
    nesting: usize,
    budget: &mut Budget,
    out: &mut Vec<ItemResult>,
) -> Result<(), ApiError> {
    while let Some(entry) = opened.entries.recv().await {
        match entry {
            archive::Entry::Folder(path) => {
                let path = join(base, &path);
                if let Err(ApiError::BadRequest(clash)) =
                    store::ensure_folder(&state.pool, project, &path).await
                {
                    out.push(ItemResult::new(&path, "skipped", Some(clash)));
                }
            }
            archive::Entry::File { path, bytes } => {
                if !budget.take(bytes.len() as u64) {
                    break; // dropping the receiver stops the archive's thread
                }
                let path = join(base, &path);
                let declared = mime_guess::from_path(&path)
                    .first()
                    .map(|m| m.essence_str().to_string());
                // Boxed: a zip inside this one comes back here.
                Box::pin(add_file(
                    state, project, job_id, &path, declared, bytes, nesting, budget, out,
                ))
                .await?;
            }
            archive::Entry::Skipped { path, reason, size } => {
                let size = size.map(|s| s as i64);
                match paths::normalize(&join(base, &path)) {
                    Ok(p) => out.push(skip(state, project, &p, "skipped", reason, size).await?),
                    Err(_) => out.push(ItemResult::new(&path, "skipped", Some(reason))),
                }
            }
            archive::Entry::LeftOut { folder, reason } => {
                out.push(left_out(state, project, &join(base, &folder), reason).await?);
            }
            archive::Entry::Stopped(reason) => {
                budget.stopped.get_or_insert(reason);
            }
        }
    }
    Ok(())
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
    let mut budget = Budget::new(&state.config);
    let opened = archive::open(zip, budget.limits()).map_err(ApiError::BadRequest)?;
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
    unpack_into(
        state,
        project.id,
        job_id,
        None,
        opened,
        1,
        &mut budget,
        &mut files,
    )
    .await?;
    let project = store::summary(&state.pool, project.id).await?;
    Ok(ImportReport {
        project,
        files,
        stopped: budget.stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_to_one_project_take_turns() {
        let a = Uuid::new_v4();
        let held = lock(a).await;
        // Another project isn't held up.
        let other =
            tokio::time::timeout(std::time::Duration::from_millis(100), lock(Uuid::new_v4()));
        assert!(other.await.is_ok());
        // The same project waits until the first request is done.
        let same = tokio::time::timeout(std::time::Duration::from_millis(100), lock(a));
        assert!(same.await.is_err());
        drop(held);
        let again = tokio::time::timeout(std::time::Duration::from_millis(100), lock(a));
        assert!(again.await.is_ok());
    }
}
