//! Automatic import: an inbox folder, and Claude Code's own session folder.
//!
//! Everything here reads local folders the user chose and writes to the local
//! database; nothing is fetched and nothing leaves the computer.
//!
//! * **Inbox** (`GATHER_INBOX_DIR`): drop an export or a document in it. Chat
//!   exports (ChatGPT, Claude, Gemini, Grok, Copilot, Perplexity, or Gather's
//!   generic format) are recognised by their shape, as JSON files or inside a
//!   `.zip`; Perplexity Markdown becomes a conversation; documents (`.md`,
//!   `.pdf`, `.docx`, `.txt`, …) are added as files. What was read moves to
//!   `done/`; what couldn't be, to `failed/` with a note saying why. Nothing
//!   is ever deleted.
//! * **Claude Code** (`GATHER_CLAUDE_CODE_DIR`): its `*.jsonl` session
//!   transcripts. A session is imported once it has been quiet for a moment,
//!   and read again when it grows, adding only the messages it doesn't have.
//!
//! One background task looks at both every `GATHER_AUTOIMPORT_INTERVAL_SECS`.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::adapters::{self, claude_code};
use crate::error::ApiError;
use crate::routes::ingest::{
    chat_export_core, create_job, finish_job, ingest_one_file, insert_messages,
    persist_conversations, sha256_hex, store_artifact, ChatExportRequest,
};
use crate::AppState;

/// A dropped file is left alone until it has stopped changing for this long
/// (it may still be being copied or downloaded).
const SETTLE: Duration = Duration::from_secs(5);
/// A Claude Code session is imported once it has been quiet for this long.
const SESSION_QUIET: Duration = Duration::from_secs(20);
/// Files taken per look; more than this and the next look comes soon.
const MAX_FILES_PER_SCAN: usize = 25;
const BACKLOG_INTERVAL: Duration = Duration::from_secs(2);
const MAX_ZIP_ENTRIES: usize = 5_000;
const MAX_DEPTH: usize = 4;
const INBOX_DONE: &str = "done";
const INBOX_FAILED: &str = "failed";

pub async fn worker_loop(state: AppState) {
    let config = state.config.clone();
    if config.inbox_dir.is_none() && config.claude_code_dir.is_none() {
        return;
    }
    if let Some(dir) = &config.inbox_dir {
        tracing::info!(dir = %dir.display(), "auto-import: watching the inbox folder");
    }
    if let Some(dir) = &config.claude_code_dir {
        tracing::info!(dir = %dir.display(), "auto-import: watching Claude Code sessions");
    }
    let idle = Duration::from_secs(config.autoimport_interval_secs);
    loop {
        let mut backlog = false;
        if let Some(dir) = &config.inbox_dir {
            match scan_inbox(&state, dir).await {
                Ok(more) => backlog |= more,
                Err(e) => tracing::warn!(error = %e, "auto-import: the inbox could not be read"),
            }
        }
        if let Some(dir) = &config.claude_code_dir {
            match scan_claude_code(&state, dir).await {
                Ok(more) => backlog |= more,
                Err(e) => {
                    tracing::warn!(error = %e, "auto-import: Claude Code sessions could not be read")
                }
            }
        }
        tokio::time::sleep(if backlog { BACKLOG_INTERVAL } else { idle }).await;
    }
}

/// What is set up and what has been imported, for `GET /status`.
pub async fn summary(pool: &PgPool, state: &AppState) -> Result<Value, sqlx::Error> {
    let c = &state.config;
    let counts = sqlx::query(
        r#"
        SELECT count(*) FILTER (WHERE kind = 'claude_code' AND status = 'imported'
                        AND session_id IS NOT NULL) AS sessions,
               count(*) FILTER (WHERE kind = 'inbox' AND status = 'imported') AS inbox_done,
               count(*) FILTER (WHERE status IN ('failed', 'unrecognized')) AS needs_attention
        FROM import_sources
        "#,
    )
    .fetch_one(pool)
    .await?;
    let recent = sqlx::query(
        "SELECT path, kind, status, detail, updated_at FROM import_sources
         ORDER BY updated_at DESC LIMIT 5",
    )
    .fetch_all(pool)
    .await?;
    let recent: Vec<Value> = recent
        .iter()
        .map(|r| {
            let path: String = r.get("path");
            json!({
                "name": Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()),
                "kind": r.get::<String, _>("kind"),
                "status": r.get::<String, _>("status"),
                "detail": r.get::<Option<String>, _>("detail"),
                "at": r.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
            })
        })
        .collect();
    Ok(json!({
        "inbox_dir": c.inbox_dir.as_ref().map(|p| p.display().to_string()),
        "claude_code_dir": c.claude_code_dir.as_ref().map(|p| p.display().to_string()),
        "sessions": counts.get::<i64, _>("sessions"),
        "inbox_done": counts.get::<i64, _>("inbox_done"),
        "needs_attention": counts.get::<i64, _>("needs_attention"),
        "recent": recent,
    }))
}

// ---------------------------------------------------------------------------
// Bookkeeping
// ---------------------------------------------------------------------------

struct Seen {
    size: i64,
    mtime_ns: i64,
}

fn mtime_ns(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn quiet_for(time: SystemTime, quiet: Duration) -> bool {
    SystemTime::now()
        .duration_since(time)
        .map(|age| age >= quiet)
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
async fn record(
    pool: &PgPool,
    kind: &str,
    path: &Path,
    seen: &Seen,
    status: &str,
    session_id: Option<&str>,
    messages: usize,
    detail: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO import_sources
            (path, kind, status, size, mtime_ns, session_id, messages, detail, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
        ON CONFLICT (path) DO UPDATE SET
            kind = EXCLUDED.kind, status = EXCLUDED.status, size = EXCLUDED.size,
            mtime_ns = EXCLUDED.mtime_ns, session_id = EXCLUDED.session_id,
            messages = EXCLUDED.messages, detail = EXCLUDED.detail, updated_at = now()
        "#,
    )
    .bind(path.to_string_lossy().as_ref())
    .bind(kind)
    .bind(status)
    .bind(seen.size)
    .bind(seen.mtime_ns)
    .bind(session_id)
    .bind(messages as i32)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// Errors that a retry won't cure: the file itself is the problem. Anything
/// else (the database, the disk) leaves the file where it is for the next look.
fn is_permanent(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::BadRequest(_) | ApiError::UnsupportedMedia(_) | ApiError::PayloadTooLarge(_)
    )
}

fn max_bytes(state: &AppState) -> u64 {
    (state.config.max_upload_mb as u64).saturating_mul(1024 * 1024)
}

// ---------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------

enum Outcome {
    Imported(String),
    Unrecognized(String),
}

fn is_partial_download(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with('.')
        || name.starts_with('~')
        || [".part", ".crdownload", ".download", ".tmp", ".partial"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// Look at the inbox once. True when more files are waiting than one look takes.
/// Public so integration tests can drive it.
pub async fn scan_inbox(state: &AppState, dir: &Path) -> anyhow::Result<bool> {
    if !dir.is_dir() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut files: Vec<(PathBuf, String, Seen)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        // Files only: folders (done/, failed/) and links are left alone.
        if !entry
            .file_type()
            .await
            .map(|t| t.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_partial_download(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if !quiet_for(modified, SETTLE) {
            continue; // still being written
        }
        files.push((
            entry.path(),
            name,
            Seen {
                size: meta.len() as i64,
                mtime_ns: mtime_ns(modified),
            },
        ));
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));

    for (index, (path, name, seen)) in files.iter().enumerate() {
        if index >= MAX_FILES_PER_SCAN {
            return Ok(true);
        }
        handle_inbox_file(state, dir, path, name, seen).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

async fn handle_inbox_file(state: &AppState, dir: &Path, path: &Path, name: &str, seen: &Seen) {
    let pool = &state.pool;
    let result = process_inbox_file(state, path, name).await;
    // A file that was replaced or added to while it was being read is not the
    // one that was read: leave it in place to be read again (what was read is
    // deduplicated by content), instead of filing away bytes never imported.
    if !still_the_same(path, seen).await {
        tracing::info!(
            file = name,
            "auto-import: changed while being read; will read again"
        );
        return;
    }
    let (status, detail, destination, note) = match result {
        Ok(Outcome::Imported(detail)) => ("imported", detail, INBOX_DONE, None),
        Ok(Outcome::Unrecognized(why)) => ("unrecognized", why.clone(), INBOX_FAILED, Some(why)),
        Err(e) if is_permanent(&e) => {
            let why = e.to_string();
            ("failed", why.clone(), INBOX_FAILED, Some(why))
        }
        Err(e) => {
            // The database or disk, not the file: try again on the next look.
            tracing::warn!(file = name, error = %e, "auto-import: will retry");
            return;
        }
    };
    tracing::info!(file = name, status, detail = %detail, "auto-import: inbox file");
    if let Err(e) = record(pool, "inbox", path, seen, status, None, 0, Some(&detail)).await {
        tracing::warn!(error = %e, "auto-import: could not record an inbox file");
    }
    if let Err(e) = move_aside(dir, destination, path, name, note.as_deref()).await {
        tracing::warn!(file = name, error = %e, "auto-import: could not move a file out of the inbox");
    }
}

/// Whether `path` still has the size and modification time it had when it was
/// picked up.
async fn still_the_same(path: &Path, seen: &Seen) -> bool {
    match tokio::fs::metadata(path).await {
        Ok(meta) => {
            meta.len() as i64 == seen.size
                && meta
                    .modified()
                    .map(mtime_ns)
                    .is_ok_and(|t| t == seen.mtime_ns)
        }
        Err(_) => false,
    }
}

/// Claim a name in `target_dir` that nothing else has: `name` if it is free,
/// else `stem-<time>.ext`, else with a counter. The file is created empty, so
/// the name is held (`create_new` is atomic) until the move replaces it.
async fn reserve_name(target_dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (name.to_string(), String::new()),
    };
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for attempt in 0..1000u32 {
        let candidate = match attempt {
            0 => name.to_string(),
            1 => format!("{stem}-{stamp}{ext}"),
            n => format!("{stem}-{stamp}-{n}{ext}"),
        };
        let path = target_dir.join(candidate);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no unused name in the destination folder",
    ))
}

/// Move `path` into `dir/destination/`, never overwriting, with the reason in
/// a `.txt` beside it when there is one.
async fn move_aside(
    dir: &Path,
    destination: &str,
    path: &Path,
    name: &str,
    note: Option<&str>,
) -> std::io::Result<()> {
    let target_dir = dir.join(destination);
    tokio::fs::create_dir_all(&target_dir).await?;
    let target = reserve_name(&target_dir, name).await?;
    if let Err(e) = tokio::fs::rename(path, &target).await {
        let _ = tokio::fs::remove_file(&target).await; // the empty placeholder
        return Err(e);
    }
    if let Some(note) = note {
        let mut sidecar = target.clone().into_os_string();
        sidecar.push(".why.txt");
        tokio::fs::write(PathBuf::from(sidecar), format!("{note}\n")).await?;
    }
    Ok(())
}

async fn process_inbox_file(
    state: &AppState,
    path: &Path,
    name: &str,
) -> Result<Outcome, ApiError> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "zip" {
        return import_zip(state, path, name).await;
    }
    let bytes = read_capped(path, max_bytes(state), name).await?;
    match ext.as_str() {
        "json" => import_json(state, bytes, name).await,
        "jsonl" | "ndjson" => import_jsonl(state, &bytes, name).await,
        _ => import_document(state, bytes, name).await,
    }
}

async fn read_capped(path: &Path, cap: u64, name: &str) -> Result<Vec<u8>, ApiError> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("opening {name}: {e}")))?;
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("reading {name}: {e}")))?;
    if bytes.len() as u64 > cap {
        return Err(too_large(name, cap));
    }
    Ok(bytes)
}

fn too_large(name: &str, cap: u64) -> ApiError {
    ApiError::PayloadTooLarge(format!(
        "{name} is larger than {} MB, the most Gather reads from one file on this computer \
         (GATHER_MAX_UPLOAD_MB)",
        cap / (1024 * 1024)
    ))
}

async fn import_json(state: &AppState, bytes: Vec<u8>, name: &str) -> Result<Outcome, ApiError> {
    let data: Value = tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("parsing {name}: {e}")))?
        .map_err(|e| ApiError::BadRequest(format!("{name} is not valid JSON: {e}")))?;
    let Some(platform) = adapters::sniff::platform_of(&data) else {
        return Ok(Outcome::Unrecognized(format!(
            "{name} is JSON, but not an export Gather knows (ChatGPT, Claude, Gemini, Grok, \
             Copilot, Perplexity, or the generic format)"
        )));
    };
    // A file can have the right shape and no messages in it (an empty
    // export, or a layout this version can't read yet). Filing that away as
    // imported would hide the problem: say so instead.
    let parsed = adapters::normalize(platform, &data)
        .map_err(|e| ApiError::BadRequest(format!("{name}: {e}")))?;
    let messages: usize = parsed.conversations.iter().map(|c| c.messages.len()).sum();
    if messages == 0 {
        return Ok(Outcome::Unrecognized(format!(
            "{name} looks like a {platform} export, but no messages could be read from it \
             ({} conversations found). If it has conversations in it, its layout may be one \
             this version of Gather doesn't read yet.",
            parsed.conversations.len()
        )));
    }
    drop(parsed);
    let response = chat_export_core(
        state,
        ChatExportRequest {
            platform: platform.to_string(),
            data,
            filename: Some(name.to_string()),
        },
        "inbox",
    )
    .await?;
    Ok(Outcome::Imported(describe_chat(platform, &response)))
}

fn describe_chat(platform: &str, r: &crate::routes::ingest::ChatExportResponse) -> String {
    if r.deduplicated {
        format!("{platform}: already in Gather")
    } else {
        format!(
            "{platform}: {} conversations, {} messages",
            r.conversations, r.messages
        )
    }
}

async fn import_jsonl(state: &AppState, bytes: &[u8], name: &str) -> Result<Outcome, ApiError> {
    let mut session = claude_code::parse_reader(bytes);
    if session.messages.is_empty() {
        return Ok(Outcome::Unrecognized(format!(
            "{name} has no Claude Code conversation in it"
        )));
    }
    if session.session_id.is_none() {
        session.session_id = Some(format!("file-{}", &sha256_hex(bytes)[..16]));
    }
    let added = import_claude_code_session(state, session).await?;
    Ok(Outcome::Imported(format!(
        "Claude Code session: {added} messages"
    )))
}

async fn import_document(
    state: &AppState,
    bytes: Vec<u8>,
    name: &str,
) -> Result<Outcome, ApiError> {
    let job_id = create_job(&state.pool, "inbox").await?;
    let result = ingest_one_file(state, job_id, "file", name, None, &bytes).await;
    let ok = result.is_ok();
    finish_job(&state.pool, job_id, ok, json!({ "file": name })).await?;
    match result {
        Ok(file) => Ok(Outcome::Imported(match file.status.as_str() {
            "deduplicated" => "already in Gather".to_string(),
            _ => file
                .detail
                .unwrap_or_else(|| format!("added as {}", file.kind.unwrap_or_default())),
        })),
        Err(ApiError::UnsupportedMedia(kind)) => Ok(Outcome::Unrecognized(format!(
            "{name}: Gather doesn't read {kind} files"
        ))),
        Err(e) => Err(e),
    }
}

// --- zip -------------------------------------------------------------------

/// The entries of a zip that could be a chat export: JSON, JSONL and Markdown
/// (other files in an export, like attachments and HTML, are not read).
fn list_zip(path: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("not a readable zip: {e}"))?;
    let mut names = Vec::new();
    for index in 0..archive.len().min(MAX_ZIP_ENTRIES) {
        let entry = archive.by_index(index).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        if [".json", ".jsonl", ".ndjson", ".md", ".markdown"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            names.push(name);
        }
    }
    Ok(names)
}

fn read_zip_entry(path: &Path, entry_name: &str, cap: u64) -> Result<Option<Vec<u8>>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let entry = archive.by_name(entry_name).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    // Bounded by what is actually read, whatever the header claims.
    entry
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok((bytes.len() as u64 <= cap).then_some(bytes))
}

async fn import_zip(state: &AppState, path: &Path, name: &str) -> Result<Outcome, ApiError> {
    let cap = max_bytes(state);
    let zip_path = path.to_path_buf();
    let names = tokio::task::spawn_blocking(move || list_zip(&zip_path))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("reading {name}: {e}")))?
        .map_err(|e| ApiError::BadRequest(format!("{name}: {e}")))?;

    let mut imported: Vec<String> = Vec::new();
    for entry_name in names {
        let zip_path = path.to_path_buf();
        let wanted = entry_name.clone();
        let bytes = tokio::task::spawn_blocking(move || read_zip_entry(&zip_path, &wanted, cap))
            .await
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("reading {name}: {e}")))?
            .map_err(|e| ApiError::BadRequest(format!("{name}: {entry_name}: {e}")))?;
        let Some(bytes) = bytes else {
            return Err(too_large(&format!("{entry_name} (in {name})"), cap));
        };
        let short = entry_name.rsplit('/').next().unwrap_or(&entry_name);
        let lower = short.to_ascii_lowercase();
        let outcome = if lower.ends_with(".json") {
            import_json(state, bytes, short).await?
        } else if lower.ends_with(".jsonl") || lower.ends_with(".ndjson") {
            import_jsonl(state, &bytes, short).await?
        } else {
            // Markdown: only a Perplexity export is a conversation; other
            // documents in a zip are not read here (Projects → Import .zip
            // is for a folder of documents).
            let text = String::from_utf8_lossy(&bytes);
            if adapters::perplexity_md::looks_like(&text) {
                import_document(state, bytes, short).await?
            } else {
                continue;
            }
        };
        if let Outcome::Imported(detail) = outcome {
            imported.push(format!("{short}: {detail}"));
        }
    }

    if imported.is_empty() {
        return Ok(Outcome::Unrecognized(format!(
            "{name} has no chat export Gather knows inside it. (To add a folder of documents, \
             use Projects → Import .zip.)"
        )));
    }
    Ok(Outcome::Imported(imported.join("; ")))
}

// ---------------------------------------------------------------------------
// Claude Code sessions
// ---------------------------------------------------------------------------

struct SessionFile {
    path: PathBuf,
    seen: Seen,
    modified: SystemTime,
}

/// Every `*.jsonl` under `root`, a few folders deep. Links are not followed.
fn collect_sessions(root: &Path) -> Vec<SessionFile> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<SessionFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    walk(&path, depth + 1, out);
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
            {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        out.push(SessionFile {
                            path,
                            seen: Seen {
                                size: meta.len() as i64,
                                mtime_ns: mtime_ns(modified),
                            },
                            modified,
                        });
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

/// Look at the sessions folder once. True when more are waiting than one look
/// takes. Public so integration tests can drive it.
pub async fn scan_claude_code(state: &AppState, root: &Path) -> anyhow::Result<bool> {
    if !root.is_dir() {
        return Ok(false); // Claude Code isn't used here (yet)
    }
    let root_owned = root.to_path_buf();
    let mut files = tokio::task::spawn_blocking(move || collect_sessions(&root_owned)).await?;
    // Newest first: what is being worked on shows up first.
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));

    let known: HashMap<String, (i64, i64)> =
        sqlx::query("SELECT path, size, mtime_ns FROM import_sources WHERE kind = 'claude_code'")
            .fetch_all(&state.pool)
            .await?
            .into_iter()
            .map(|r| (r.get("path"), (r.get("size"), r.get("mtime_ns"))))
            .collect();

    let mut taken = 0usize;
    for file in files {
        let key = file.path.to_string_lossy().to_string();
        if known.get(&key) == Some(&(file.seen.size, file.seen.mtime_ns)) {
            continue;
        }
        if !quiet_for(file.modified, SESSION_QUIET) {
            continue; // still being written
        }
        if taken >= MAX_FILES_PER_SCAN {
            return Ok(true);
        }
        taken += 1;
        import_session_file(state, &file).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

async fn import_session_file(state: &AppState, file: &SessionFile) {
    let path = file.path.clone();
    let parsed = tokio::task::spawn_blocking(move || {
        let f = std::fs::File::open(&path)?;
        Ok::<_, std::io::Error>(claude_code::parse_reader(std::io::BufReader::new(f)))
    })
    .await;
    let mut session = match parsed {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            tracing::warn!(path = %file.path.display(), error = %e, "auto-import: could not read a session");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto-import: session reading stopped");
            return;
        }
    };
    if session.messages.is_empty() {
        // Nothing said (a sub-agent log, or a session that never began):
        // remembered so it isn't read again until it changes.
        let _ = record(
            &state.pool,
            "claude_code",
            &file.path,
            &file.seen,
            "imported",
            None,
            0,
            Some("no conversation in it"),
        )
        .await;
        return;
    }
    if session.session_id.is_none() {
        session.session_id = file
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string());
    }
    let session_id = session.session_id.clone();
    let title = session.title.clone();
    match import_claude_code_session(state, session).await {
        Ok(added) => {
            if added > 0 {
                tracing::info!(
                    session = session_id.as_deref().unwrap_or(""),
                    messages = added,
                    "auto-import: Claude Code session"
                );
            }
            let detail = format!("{added} new messages");
            let _ = record(
                &state.pool,
                "claude_code",
                &file.path,
                &file.seen,
                "imported",
                session_id.as_deref(),
                added,
                Some(title.as_deref().unwrap_or(&detail)),
            )
            .await;
        }
        Err(e) if is_permanent(&e) => {
            let why = e.to_string();
            tracing::warn!(path = %file.path.display(), error = %why, "auto-import: session skipped");
            let _ = record(
                &state.pool,
                "claude_code",
                &file.path,
                &file.seen,
                "failed",
                session_id.as_deref(),
                0,
                Some(&why),
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(path = %file.path.display(), error = %e, "auto-import: will retry");
        }
    }
}

/// The conversation as stored: a first line naming the session, then one JSON
/// object per message, the text only. The session line keeps two sessions that
/// happen to say the same things (a copied or forked one) from being stored as
/// one artifact.
fn transcript(session_id: &str, messages: &[adapters::NormalizedMessage]) -> Vec<u8> {
    let mut out = json!({ "session_id": session_id }).to_string();
    out.push('\n');
    for m in messages {
        out.push_str(
            &json!({
                "role": m.role,
                "content": m.content,
                "created_at": m.created_at,
            })
            .to_string(),
        );
        out.push('\n');
    }
    out.into_bytes()
}

/// Add a Claude Code session to Gather, or bring an earlier import of it up
/// to date: a session already stored gets only the messages it lacks. Returns
/// how many messages were added.
pub(crate) async fn import_claude_code_session(
    state: &AppState,
    session: claude_code::Session,
) -> Result<usize, ApiError> {
    let Some(session_id) = session.session_id.clone() else {
        return Err(ApiError::BadRequest("session has no id".into()));
    };
    let text = transcript(&session_id, &session.messages);
    let mut tx = state.pool.begin().await?;
    // One writer per session, whichever door it came in by.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("claude_code:{session_id}"))
        .execute(&mut *tx)
        .await?;

    let existing: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, artifact_id FROM conversations
         WHERE source_platform = 'claude_code' AND external_id = $1
         ORDER BY started_at NULLS LAST LIMIT 1",
    )
    .bind(&session_id)
    .fetch_optional(&mut *tx)
    .await?;

    let added = match existing {
        None => {
            let job_id: Uuid = {
                let (id,): (Uuid,) = sqlx::query_as(
                    "INSERT INTO ingestion_jobs (source, status) VALUES ('autoimport', 'processing')
                     RETURNING id",
                )
                .fetch_one(&mut *tx)
                .await?;
                id
            };
            let stored = store_artifact(
                &mut tx,
                "agent_log",
                "claude_code",
                Some(claude_code::FORMAT),
                Some(&format!("{session_id}.jsonl")),
                Some("application/x-ndjson"),
                &text,
                session.started_at,
                job_id,
                json!({ "session_id": session_id, "cwd": session.cwd }),
            )
            .await?;
            // A conversation for this session is stored whether or not the
            // artifact was new: the session line makes the transcript unique
            // to it, but an artifact left over from an earlier import still
            // has to get its conversation.
            let conversation = adapters::NormalizedConversation {
                external_id: Some(session_id.clone()),
                title: session.title.clone(),
                model: session.model.clone(),
                started_at: session.started_at,
                ended_at: session.ended_at,
                messages: session.messages.clone(),
            };
            let (_, added) =
                persist_conversations(&mut tx, stored.id, "claude_code", &[conversation]).await?;
            if !stored.deduplicated {
                metrics::counter!("gather_ingest_artifacts_total", "kind" => "agent_log")
                    .increment(1);
            }
            sqlx::query(
                "UPDATE ingestion_jobs SET status = 'completed', finished_at = now(), stats = $2
                 WHERE id = $1",
            )
            .bind(job_id)
            .bind(json!({ "conversations": 1, "messages": added }))
            .execute(&mut *tx)
            .await?;
            added
        }
        Some((conversation_id, artifact_id)) => {
            let known: Vec<(String, Uuid)> = sqlx::query_as(
                "SELECT external_id, id FROM messages
                 WHERE conversation_id = $1 AND external_id IS NOT NULL",
            )
            .bind(conversation_id)
            .fetch_all(&mut *tx)
            .await?;
            let mut id_by_external: HashMap<String, Uuid> = known.into_iter().collect();
            let fresh: Vec<adapters::NormalizedMessage> = session
                .messages
                .iter()
                .filter(|m| {
                    m.external_id
                        .as_ref()
                        .map_or(true, |ext| !id_by_external.contains_key(ext))
                })
                .cloned()
                .collect();
            if fresh.is_empty() {
                0
            } else {
                let (next_seq,): (i32,) = sqlx::query_as(
                    "SELECT coalesce(max(seq), -1) + 1 FROM messages WHERE conversation_id = $1",
                )
                .bind(conversation_id)
                .fetch_one(&mut *tx)
                .await?;
                let added = insert_messages(
                    &mut tx,
                    conversation_id,
                    next_seq,
                    &fresh,
                    &mut id_by_external,
                )
                .await?;
                sqlx::query(
                    "UPDATE conversations
                     SET ended_at = greatest(ended_at, $2), title = coalesce(title, $3)
                     WHERE id = $1",
                )
                .bind(conversation_id)
                .bind(session.ended_at)
                .bind(&session.title)
                .execute(&mut *tx)
                .await?;
                // The stored transcript follows the session (unless another
                // artifact already holds exactly these bytes).
                sqlx::query(
                    "UPDATE artifacts SET raw_content = $2, byte_size = $3, content_hash = $4
                     WHERE id = $1
                       AND NOT EXISTS (SELECT 1 FROM artifacts WHERE content_hash = $4 AND id <> $1)",
                )
                .bind(artifact_id)
                .bind(&text)
                .bind(text.len() as i64)
                .bind(sha256_hex(&text))
                .execute(&mut *tx)
                .await?;
                added
            }
        }
    };
    tx.commit().await?;
    if added > 0 {
        metrics::counter!("gather_ingest_messages_total", "platform" => "claude_code")
            .increment(added as u64);
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_downloaded_and_hidden_files_are_left_alone() {
        for name in [
            ".DS_Store",
            "~$export.docx",
            "export.zip.crdownload",
            "export.zip.part",
            "notes.tmp",
        ] {
            assert!(is_partial_download(name), "{name}");
        }
        for name in ["conversations.json", "thread.md", "export-2026.zip"] {
            assert!(!is_partial_download(name), "{name}");
        }
    }

    #[test]
    fn only_permanent_errors_move_a_file_away() {
        assert!(is_permanent(&ApiError::BadRequest("x".into())));
        assert!(is_permanent(&ApiError::UnsupportedMedia("x".into())));
        assert!(is_permanent(&ApiError::PayloadTooLarge("x".into())));
        assert!(!is_permanent(&ApiError::Internal(anyhow::anyhow!("disk"))));
    }

    #[test]
    fn a_file_must_be_quiet_before_it_is_read() {
        assert!(quiet_for(
            SystemTime::now() - Duration::from_secs(60),
            SETTLE
        ));
        assert!(!quiet_for(SystemTime::now(), SETTLE));
        // A modification time in the future is not "quiet".
        assert!(!quiet_for(
            SystemTime::now() + Duration::from_secs(60),
            SETTLE
        ));
    }

    #[test]
    fn a_session_is_stored_as_its_text_only() {
        let session = claude_code::parse_str(
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-03-01T09:00:00Z","message":{"role":"user","content":"I use Postgres."}}"#,
        );
        let text = String::from_utf8(transcript("s", &session.messages)).unwrap();
        let mut lines = text.lines();
        let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["session_id"], "s");
        let line: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(lines.next().is_none());
        assert_eq!(line["role"], "user");
        assert_eq!(line["content"], "I use Postgres.");
    }

    #[tokio::test]
    async fn an_archived_file_never_takes_the_place_of_another() {
        let dir = std::env::temp_dir().join(format!("gather-reserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), "first").unwrap();

        // The plain name is taken, and so are the time-stamped ones as they
        // are handed out: each caller gets a name of its own.
        let mut names = std::collections::HashSet::new();
        for _ in 0..4 {
            let path = reserve_name(&dir, "a.json").await.unwrap();
            assert!(names.insert(path.clone()), "{path:?} handed out twice");
            assert_ne!(path, dir.join("a.json"));
            assert!(path.to_string_lossy().ends_with(".json"));
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("a.json")).unwrap(),
            "first"
        );
        // A free name is used as it is.
        assert_eq!(
            reserve_name(&dir, "fresh.md").await.unwrap(),
            dir.join("fresh.md")
        );
        // Names without an extension work too.
        assert!(reserve_name(&dir, "README").await.is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_file_that_changed_is_not_the_one_that_was_read() {
        let dir = std::env::temp_dir().join(format!("gather-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("export.json");
        std::fs::write(&path, "[]").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let seen = Seen {
            size: meta.len() as i64,
            mtime_ns: mtime_ns(meta.modified().unwrap()),
        };
        assert!(still_the_same(&path, &seen).await);
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        assert!(!still_the_same(&path, &seen).await, "size changed");
        std::fs::remove_file(&path).unwrap();
        assert!(!still_the_same(&path, &seen).await, "gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
        use std::io::Write;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, bytes) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn a_zip_lists_only_what_could_be_a_chat_export() {
        let dir = std::env::temp_dir().join(format!("gather-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("export.zip");
        write_zip(
            &path,
            &[
                ("conversations.json", b"[]"),
                ("Takeout/My Activity/Gemini Apps/MyActivity.json", b"[]"),
                ("chat.html", b"<html></html>"),
                ("images/photo.png", b"\x89PNG"),
                ("thread.md", b"# q"),
            ],
        );
        let names = list_zip(&path).unwrap();
        assert_eq!(
            names,
            [
                "conversations.json",
                "Takeout/My Activity/Gemini Apps/MyActivity.json",
                "thread.md"
            ]
        );
        // An entry larger than the cap is reported, not truncated.
        assert!(read_zip_entry(&path, "conversations.json", 1)
            .unwrap()
            .is_none());
        assert_eq!(
            read_zip_entry(&path, "conversations.json", 10)
                .unwrap()
                .unwrap(),
            b"[]"
        );
        assert!(list_zip(&dir.join("missing.zip")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sessions_are_found_a_few_folders_deep() {
        let root = std::env::temp_dir().join(format!("gather-sessions-{}", std::process::id()));
        let project = root.join("-home-user-app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("a.jsonl"), "{}\n").unwrap();
        std::fs::write(project.join("notes.txt"), "no").unwrap();
        std::fs::write(root.join("b.JSONL"), "{}\n").unwrap();
        let mut found: Vec<String> = collect_sessions(&root)
            .into_iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        found.sort();
        assert_eq!(found, ["a.jsonl", "b.JSONL"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
