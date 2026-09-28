//! Persistence for projects and their folder/file trees.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use super::paths;
use crate::error::ApiError;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub id: Uuid,
    pub name: String,
    /// `folder` or `zip`.
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub folders: i64,
    pub files: i64,
    pub ingested: i64,
    pub deduplicated: i64,
    /// Files kept as they are, without text read from them.
    pub stored: i64,
    pub skipped: i64,
    pub failed: i64,
    /// Folders left out whole (version control, dependencies, caches).
    pub left_out: i64,
    /// Total size of the project's files, as uploaded.
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    /// `folder` or `file`.
    pub item_kind: String,
    pub name: String,
    pub path: String,
    pub depth: i32,
    /// `folder`, `ingested`, `deduplicated`, `skipped` or `failed`.
    pub status: String,
    pub detail: Option<String>,
    pub byte_size: Option<i64>,
    pub artifact_id: Option<Uuid>,
    pub artifact_kind: Option<String>,
    /// Statements extracted from the file so far.
    pub units: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub project: ProjectSummary,
    /// Every folder and file, parents before children (ordered by path).
    pub items: Vec<ItemView>,
}

/// What happened to one file of a project.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub status: &'static str,
    pub artifact_id: Option<Uuid>,
    pub detail: Option<String>,
    pub byte_size: Option<i64>,
}

pub async fn create(pool: &PgPool, name: &str, source: &str) -> Result<ProjectSummary, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("a project needs a name".into()));
    }
    if name.len() > 255 {
        return Err(ApiError::BadRequest("the project name is too long".into()));
    }
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO projects (name, source) VALUES ($1, $2) RETURNING id")
            .bind(name)
            .bind(source)
            .fetch_one(pool)
            .await?;
    summary(pool, id).await
}

pub async fn exists(pool: &PgPool, id: Uuid) -> Result<(), ApiError> {
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::NotFound(format!("project {id}")))
}

/// The folder at `path` and every folder above it, created as needed.
/// Returns the folder's id.
async fn folder_chain(
    conn: &mut PgConnection,
    project: Uuid,
    path: &str,
) -> Result<Uuid, ApiError> {
    let mut parent: Option<Uuid> = None;
    let mut chain = paths::ancestors(path);
    chain.push(path);
    for (depth, folder) in chain.into_iter().enumerate() {
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO project_items (project_id, parent_id, item_kind, name, path, depth, status) \
             VALUES ($1, $2, 'folder', $3, $4, $5, 'folder') \
             ON CONFLICT (project_id, path) DO UPDATE SET name = EXCLUDED.name \
               WHERE project_items.item_kind = 'folder' \
             RETURNING id",
        )
        .bind(project)
        .bind(parent)
        .bind(paths::file_name(folder))
        .bind(folder)
        .bind(depth as i32)
        .fetch_optional(&mut *conn)
        .await?;
        parent = Some(id.ok_or_else(|| {
            ApiError::BadRequest(format!(
                "'{folder}' is a file in this project, not a folder"
            ))
        })?);
    }
    Ok(parent.expect("a chain has at least one folder"))
}

pub async fn ensure_folder(pool: &PgPool, project: Uuid, path: &str) -> Result<Uuid, ApiError> {
    let mut tx = pool.begin().await?;
    let id = folder_chain(&mut tx, project, path).await?;
    tx.commit().await?;
    Ok(id)
}

/// Why nothing new can sit at `path`: a folder above it is already a file,
/// or `path` itself is already the other kind of item (`as_folder` says which
/// kind is wanted). Checked before a file is read, so a clash doesn't store
/// the file for nothing; [`record_file`] and [`ensure_folder`] still refuse
/// one that appears in between.
pub async fn clash(
    pool: &PgPool,
    project: Uuid,
    path: &str,
    as_folder: bool,
) -> Result<Option<String>, ApiError> {
    let ancestors: Vec<String> = paths::ancestors(path)
        .into_iter()
        .map(String::from)
        .collect();
    let other_kind = if as_folder { "file" } else { "folder" };
    let found: Option<(String, String)> = sqlx::query_as(
        "SELECT path, item_kind FROM project_items \
         WHERE project_id = $1 \
           AND ((item_kind = 'file' AND path = ANY($2)) OR (item_kind = $4 AND path = $3)) \
         ORDER BY depth LIMIT 1",
    )
    .bind(project)
    .bind(&ancestors)
    .bind(path)
    .bind(other_kind)
    .fetch_optional(pool)
    .await?;
    Ok(found.map(|(at, kind)| {
        if kind == "file" {
            format!("'{at}' is a file in this project, not a folder")
        } else {
            format!("'{at}' is a folder in this project, not a file")
        }
    }))
}

/// Record the folder at `path` as left out whole, for `reason`, creating the
/// folders above it. `Err(BadRequest)` if `path` is a file in the project.
pub async fn record_left_out(
    pool: &PgPool,
    project: Uuid,
    path: &str,
    reason: &str,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    let ancestors = paths::ancestors(path);
    let parent = match ancestors.last() {
        Some(folder) => Some(folder_chain(&mut tx, project, folder).await?),
        None => None,
    };
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO project_items \
           (project_id, parent_id, item_kind, name, path, depth, status, detail) \
         VALUES ($1, $2, 'folder', $3, $4, $5, 'skipped', $6) \
         ON CONFLICT (project_id, path) DO UPDATE SET status = 'skipped', detail = EXCLUDED.detail \
           WHERE project_items.item_kind = 'folder' \
         RETURNING id",
    )
    .bind(project)
    .bind(parent)
    .bind(paths::file_name(path))
    .bind(path)
    .bind(ancestors.len() as i32)
    .bind(reason)
    .fetch_optional(&mut *tx)
    .await?;
    if id.is_none() {
        return Err(ApiError::BadRequest(format!(
            "'{path}' is a file in this project, not a folder"
        )));
    }
    tx.commit().await?;
    Ok(())
}

/// Record (or replace) the file at `path`, creating the folders above it.
pub async fn record_file(
    pool: &PgPool,
    project: Uuid,
    path: &str,
    outcome: &FileOutcome,
) -> Result<Uuid, ApiError> {
    let mut tx = pool.begin().await?;
    let ancestors = paths::ancestors(path);
    let parent = match ancestors.last() {
        Some(folder) => Some(folder_chain(&mut tx, project, folder).await?),
        None => None,
    };
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO project_items \
           (project_id, parent_id, item_kind, name, path, depth, artifact_id, status, detail, byte_size) \
         VALUES ($1, $2, 'file', $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (project_id, path) DO UPDATE SET \
           parent_id = EXCLUDED.parent_id, artifact_id = EXCLUDED.artifact_id, \
           status = EXCLUDED.status, detail = EXCLUDED.detail, byte_size = EXCLUDED.byte_size \
           WHERE project_items.item_kind = 'file' \
         RETURNING id",
    )
    .bind(project)
    .bind(parent)
    .bind(paths::file_name(path))
    .bind(path)
    .bind(ancestors.len() as i32)
    .bind(outcome.artifact_id)
    .bind(outcome.status)
    .bind(&outcome.detail)
    .bind(outcome.byte_size)
    .fetch_optional(&mut *tx)
    .await?;
    let id = id.ok_or_else(|| {
        ApiError::BadRequest(format!("'{path}' is a folder in this project, not a file"))
    })?;
    sqlx::query("UPDATE projects SET updated_at = now() WHERE id = $1")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

const SUMMARY: &str = "SELECT p.id, p.name, p.source, p.created_at, p.updated_at, \
       count(i.id) FILTER (WHERE i.status = 'folder') AS folders, \
       count(i.id) FILTER (WHERE i.item_kind = 'file') AS files, \
       count(i.id) FILTER (WHERE i.status = 'ingested') AS ingested, \
       count(i.id) FILTER (WHERE i.status = 'deduplicated') AS deduplicated, \
       count(i.id) FILTER (WHERE i.status = 'stored') AS stored, \
       count(i.id) FILTER (WHERE i.item_kind = 'file' AND i.status = 'skipped') AS skipped, \
       count(i.id) FILTER (WHERE i.status = 'failed') AS failed, \
       count(i.id) FILTER (WHERE i.item_kind = 'folder' AND i.status = 'skipped') AS left_out, \
       coalesce(sum(i.byte_size) FILTER (WHERE i.item_kind = 'file'), 0)::bigint AS bytes \
     FROM projects p LEFT JOIN project_items i ON i.project_id = p.id \
     WHERE ($1::uuid IS NULL OR p.id = $1) \
     GROUP BY p.id ORDER BY p.updated_at DESC, p.id";

fn summary_row(r: &sqlx::postgres::PgRow) -> ProjectSummary {
    ProjectSummary {
        id: r.get("id"),
        name: r.get("name"),
        source: r.get("source"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        folders: r.get("folders"),
        files: r.get("files"),
        ingested: r.get("ingested"),
        deduplicated: r.get("deduplicated"),
        stored: r.get("stored"),
        skipped: r.get("skipped"),
        failed: r.get("failed"),
        left_out: r.get("left_out"),
        bytes: r.get("bytes"),
    }
}

pub async fn summary(pool: &PgPool, id: Uuid) -> Result<ProjectSummary, ApiError> {
    let row = sqlx::query(SUMMARY)
        .bind(Some(id))
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("project {id}")))?;
    Ok(summary_row(&row))
}

pub async fn list(pool: &PgPool) -> Result<Vec<ProjectSummary>, ApiError> {
    let rows = sqlx::query(SUMMARY)
        .bind(None::<Uuid>)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(summary_row).collect())
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<ProjectDetail, ApiError> {
    let project = summary(pool, id).await?;
    let rows = sqlx::query(
        "SELECT i.id, i.parent_id, i.item_kind, i.name, i.path, i.depth, i.status, i.detail, \
                i.byte_size, i.artifact_id, a.kind::text AS artifact_kind, \
                coalesce(u.units, 0) AS units \
         FROM project_items i \
         LEFT JOIN artifacts a ON a.id = i.artifact_id \
         LEFT JOIN LATERAL ( \
             SELECT count(DISTINCT pr.atomic_unit_id) AS units \
             FROM atomic_unit_provenance pr WHERE pr.artifact_id = i.artifact_id \
         ) u ON i.artifact_id IS NOT NULL \
         WHERE i.project_id = $1 \
         ORDER BY i.path COLLATE \"C\"",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| ItemView {
            id: r.get("id"),
            parent_id: r.get("parent_id"),
            item_kind: r.get("item_kind"),
            name: r.get("name"),
            path: r.get("path"),
            depth: r.get("depth"),
            status: r.get("status"),
            detail: r.get("detail"),
            byte_size: r.get("byte_size"),
            artifact_id: r.get("artifact_id"),
            artifact_kind: r.get("artifact_kind"),
            units: r.get("units"),
        })
        .collect();
    Ok(ProjectDetail { project, items })
}

/// Remove a project's tree. The files stay in Gather.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<(), ApiError> {
    let n = sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound(format!("project {id}")));
    }
    Ok(())
}
