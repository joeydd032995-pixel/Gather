//! Read models for browsing what is stored: per-artifact processing summaries,
//! an artifact's readable content, and the whole-collection graph overview.
//! Shared by the REST routes (`routes::library`) and gRPC (`grpc::query`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;

/// Units that still count as knowledge: not superseded or retracted.
const LIVE_UNIT: &str = "u.status IN ('active', 'disputed')";

/// How far along an artifact is, and how much was extracted from it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ArtifactProgress {
    /// Live atomic units extracted from this artifact.
    pub unit_count: i64,
    /// `processing` while text extraction, OCR or unit extraction is still
    /// pending; `failed` when text extraction or OCR failed; else `done`.
    pub status: &'static str,
}

/// Progress for each of `ids` (every id gets an entry).
pub async fn artifact_progress(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, ArtifactProgress>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        SELECT a.id,
               (SELECT count(DISTINCT p.atomic_unit_id)
                  FROM atomic_unit_provenance p
                  JOIN atomic_units u ON u.id = p.atomic_unit_id
                 WHERE p.artifact_id = a.id AND {LIVE_UNIT}) AS unit_count,
               CASE
                 WHEN d.extraction_status = 'failed' OR i.ocr_status = 'failed' THEN 'failed'
                 WHEN d.extraction_status IN ('pending', 'processing')
                   OR i.ocr_status IN ('pending', 'processing')
                   OR EXISTS (SELECT 1 FROM document_segments s
                               WHERE s.document_id = d.id AND s.units_extracted_at IS NULL)
                   OR EXISTS (SELECT 1 FROM conversations c
                                JOIN messages m ON m.conversation_id = c.id
                               WHERE c.artifact_id = a.id AND m.units_extracted_at IS NULL)
                   OR (i.units_extracted_at IS NULL AND i.ocr_status = 'completed'
                       AND length(trim(coalesce(i.ocr_text, ''))) > 0)
                 THEN 'processing'
                 ELSE 'done'
               END AS status
        FROM artifacts a
        LEFT JOIN documents d ON d.artifact_id = a.id
        LEFT JOIN images i ON i.artifact_id = a.id
        WHERE a.id = ANY($1)
        "#
    )))
    .bind(ids)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let status = match r.get::<String, _>("status").as_str() {
                "failed" => "failed",
                "processing" => "processing",
                _ => "done",
            };
            (
                r.get::<Uuid, _>("id"),
                ArtifactProgress {
                    unit_count: r.get("unit_count"),
                    status,
                },
            )
        })
        .collect())
}

/// One readable piece of an artifact: a document segment, a chat message, or
/// an image's recognized text.
#[derive(Debug, Clone, Serialize)]
pub struct Passage {
    pub seq: i64,
    pub heading: Option<String>,
    pub page: Option<i32>,
    /// Chat role (`user`, `assistant`, …) for messages.
    pub role: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactContent {
    /// `document`, `conversation`, `image`, or `none` (nothing readable).
    pub source: &'static str,
    pub items: Vec<Passage>,
    pub total: i64,
}

/// The readable text of an artifact, in order, one page at a time.
pub async fn artifact_content(
    pool: &PgPool,
    id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<ArtifactContent, ApiError> {
    let exists: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM artifacts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::NotFound(format!("artifact {id}")));
    }

    let document: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM documents WHERE artifact_id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    if let Some((document_id,)) = document {
        let total: i64 =
            sqlx::query_scalar("SELECT count(*) FROM document_segments WHERE document_id = $1")
                .bind(document_id)
                .fetch_one(pool)
                .await?;
        let items = sqlx::query(
            r#"SELECT seq, page, heading, content FROM document_segments
               WHERE document_id = $1 ORDER BY seq LIMIT $2 OFFSET $3"#,
        )
        .bind(document_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| Passage {
            seq: r.get::<i32, _>("seq") as i64,
            heading: r.get("heading"),
            page: r.get("page"),
            role: None,
            text: r.get("content"),
        })
        .collect();
        return Ok(ArtifactContent {
            source: "document",
            items,
            total,
        });
    }

    let total: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id
           WHERE c.artifact_id = $1"#,
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    if total > 0 {
        let rows = sqlx::query(
            r#"SELECT c.title, m.role, m.content
               FROM messages m JOIN conversations c ON c.id = m.conversation_id
               WHERE c.artifact_id = $1
               ORDER BY c.started_at NULLS LAST, c.id, m.seq
               LIMIT $2 OFFSET $3"#,
        )
        .bind(id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        let items = rows
            .iter()
            .enumerate()
            .map(|(i, r)| Passage {
                seq: offset + i as i64,
                heading: r.get("title"),
                page: None,
                role: Some(r.get("role")),
                text: r.get("content"),
            })
            .collect();
        return Ok(ArtifactContent {
            source: "conversation",
            items,
            total,
        });
    }

    let image = sqlx::query("SELECT caption, ocr_text FROM images WHERE artifact_id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    if let Some(r) = image {
        let text = r
            .get::<Option<String>, _>("ocr_text")
            .filter(|t| !t.trim().is_empty());
        let total = i64::from(text.is_some());
        let items: Vec<Passage> = text
            .into_iter()
            .filter(|_| offset == 0)
            .map(|text| Passage {
                seq: 0,
                heading: r.get("caption"),
                page: None,
                role: None,
                text,
            })
            .collect();
        return Ok(ArtifactContent {
            source: "image",
            items,
            total,
        });
    }

    Ok(ArtifactContent {
        source: "none",
        items: Vec::new(),
        total: 0,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphEntity {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    /// Relationships plus units about this entity: how central it is.
    pub weight: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphFile {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    /// Units from this file that touch the entities in the overview.
    pub mentions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphRelation {
    pub source: Uuid,
    pub target: Uuid,
    pub relation_type: String,
    /// How many relationship rows assert this edge.
    pub count: i64,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphMention {
    pub file_id: Uuid,
    pub entity_id: Uuid,
    pub count: i64,
}

/// A project (a folder or .zip uploaded as a whole).
#[derive(Debug, Clone, Serialize)]
pub struct GraphProject {
    pub id: Uuid,
    pub name: String,
    /// `folder` or `zip`.
    pub source: String,
    pub files: i64,
}

/// A folder inside a project (a `project_items` row).
#[derive(Debug, Clone, Serialize)]
pub struct GraphFolder {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub path: String,
}

/// A project or folder holding a folder or file.
#[derive(Debug, Clone, Serialize)]
pub struct GraphContains {
    /// `project` or `folder`.
    pub parent_type: &'static str,
    pub parent: Uuid,
    /// `folder` or `file` (a file's id is its artifact's).
    pub child_type: &'static str,
    pub child: Uuid,
}

/// Two projects that are alike, and why.
#[derive(Debug, Clone, Serialize)]
pub struct GraphSimilar {
    pub a: Uuid,
    pub b: Uuid,
    /// 0 to 1.
    pub score: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphOverview {
    pub entities: Vec<GraphEntity>,
    pub files: Vec<GraphFile>,
    pub relations: Vec<GraphRelation>,
    pub mentions: Vec<GraphMention>,
    pub projects: Vec<GraphProject>,
    pub folders: Vec<GraphFolder>,
    pub contains: Vec<GraphContains>,
    pub similar: Vec<GraphSimilar>,
    /// Connected entities in the whole collection (the overview shows the
    /// top `max_entities` of them by weight).
    pub entity_total: i64,
    pub truncated: bool,
}

/// The most connected entities, the relationships among them, and (when
/// `max_files` > 0) the files those entities were extracted from.
pub async fn graph_overview(
    pool: &PgPool,
    max_entities: i64,
    max_files: i64,
    max_projects: i64,
    compare_max: usize,
) -> Result<GraphOverview, ApiError> {
    let ranked = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        WITH weight AS (
            SELECT id, sum(n)::bigint AS weight FROM (
                SELECT source_entity_id AS id, count(*) AS n
                  FROM relationships WHERE status = 'active' GROUP BY 1
                UNION ALL
                SELECT target_entity_id, count(*)
                  FROM relationships WHERE status = 'active' GROUP BY 1
                UNION ALL
                SELECT u.subject_entity_id, count(*)
                  FROM atomic_units u
                 WHERE u.subject_entity_id IS NOT NULL AND {LIVE_UNIT} GROUP BY 1
            ) x GROUP BY id
        )
        SELECT e.id, e.name, e.kind::text AS kind, w.weight,
               count(*) OVER () AS total
        FROM weight w
        JOIN entities e ON e.id = w.id AND e.merged_into_entity_id IS NULL
        ORDER BY w.weight DESC, e.name
        LIMIT $1
        "#
    )))
    .bind(max_entities)
    .fetch_all(pool)
    .await?;

    let entity_total = ranked.first().map(|r| r.get("total")).unwrap_or(0);
    let entities: Vec<GraphEntity> = ranked
        .iter()
        .map(|r| GraphEntity {
            id: r.get("id"),
            name: r.get("name"),
            kind: r.get("kind"),
            weight: r.get("weight"),
        })
        .collect();
    let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();

    let relations = relations_among(pool, &ids).await?;

    let (files, mentions) = if max_files > 0 && !ids.is_empty() {
        file_mentions(pool, &ids, max_files).await?
    } else {
        (Vec::new(), Vec::new())
    };
    let (projects, contains, similar) = if max_projects > 0 {
        projects_overview(pool, &files, max_projects, compare_max).await?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(GraphOverview {
        truncated: entity_total > entities.len() as i64,
        entities,
        files,
        relations,
        mentions,
        projects,
        folders: Vec::new(),
        contains,
        similar,
        entity_total,
    })
}

/// Active relationships among `ids`, one edge per type and direction.
pub async fn relations_among(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<GraphRelation>, ApiError> {
    Ok(sqlx::query(
        r#"
        SELECT source_entity_id, target_entity_id, relation_type,
               count(*)::bigint AS n, max(confidence) AS confidence
        FROM relationships
        WHERE status = 'active'
          AND source_entity_id = ANY($1) AND target_entity_id = ANY($1)
        GROUP BY 1, 2, 3
        ORDER BY n DESC
        "#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| GraphRelation {
        source: r.get("source_entity_id"),
        target: r.get("target_entity_id"),
        relation_type: r.get("relation_type"),
        count: r.get("n"),
        confidence: r.get("confidence"),
    })
    .collect())
}

/// Similar projects linked per project in the overview.
const OVERVIEW_SIMILAR: usize = 3;

/// The projects for the overview (the `max_projects` most recently
/// changed), which of the overview's `files` each holds, and which projects
/// are alike (compared among those shown, weighted by rarity across the
/// `compare_max` most recent).
async fn projects_overview(
    pool: &PgPool,
    files: &[GraphFile],
    max_projects: i64,
    compare_max: usize,
) -> Result<(Vec<GraphProject>, Vec<GraphContains>, Vec<GraphSimilar>), ApiError> {
    let projects: Vec<GraphProject> = sqlx::query(
        "SELECT p.id, p.name, p.source, \
                count(i.id) FILTER (WHERE i.item_kind = 'file') AS files \
         FROM projects p LEFT JOIN project_items i ON i.project_id = p.id \
         GROUP BY p.id ORDER BY p.updated_at DESC, p.id LIMIT $1",
    )
    .bind(max_projects)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| GraphProject {
        id: r.get("id"),
        name: r.get("name"),
        source: r.get("source"),
        files: r.get("files"),
    })
    .collect();
    if projects.is_empty() {
        return Ok((projects, Vec::new(), Vec::new()));
    }
    let project_ids: Vec<Uuid> = projects.iter().map(|p| p.id).collect();
    let file_ids: Vec<Uuid> = files.iter().map(|f| f.id).collect();
    let contains = sqlx::query(
        "SELECT DISTINCT project_id, artifact_id FROM project_items \
         WHERE project_id = ANY($1) AND artifact_id = ANY($2) AND item_kind = 'file'",
    )
    .bind(&project_ids)
    .bind(&file_ids)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| GraphContains {
        parent_type: "project",
        parent: r.get("project_id"),
        child_type: "file",
        child: r.get("artifact_id"),
    })
    .collect();
    let similar = crate::projects::similarity::cached_pairs(
        pool,
        compare_max,
        &project_ids,
        OVERVIEW_SIMILAR,
    )
    .await?
    .iter()
    .map(|p| GraphSimilar {
        a: p.a,
        b: p.b,
        score: p.score,
        reasons: p.reasons.clone(),
    })
    .collect();
    Ok((projects, contains, similar))
}

/// Files that units about `entity_ids` came from, the top `max_files` by
/// mention count, with their per-entity mention counts.
async fn file_mentions(
    pool: &PgPool,
    entity_ids: &[Uuid],
    max_files: i64,
) -> Result<(Vec<GraphFile>, Vec<GraphMention>), ApiError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        WITH touched AS (
            SELECT u.id AS unit_id, u.subject_entity_id AS entity_id
              FROM atomic_units u
             WHERE u.subject_entity_id = ANY($1) AND {LIVE_UNIT}
            UNION
            SELECT r.atomic_unit_id, x.entity_id
              FROM relationships r
             CROSS JOIN LATERAL (VALUES (r.source_entity_id), (r.target_entity_id)) x(entity_id)
             WHERE r.atomic_unit_id IS NOT NULL AND r.status = 'active'
               AND x.entity_id = ANY($1)
        ),
        pairs AS (
            SELECT p.artifact_id, t.entity_id, count(DISTINCT t.unit_id)::bigint AS n
              FROM touched t
              JOIN atomic_unit_provenance p ON p.atomic_unit_id = t.unit_id
             GROUP BY 1, 2
        ),
        top_files AS (
            SELECT artifact_id, sum(n)::bigint AS mentions
              FROM pairs GROUP BY 1
             ORDER BY mentions DESC, artifact_id
             LIMIT $2
        )
        SELECT f.artifact_id, f.mentions, a.kind::text AS kind,
               coalesce(a.original_filename, c.title, a.source_platform) AS name,
               pr.entity_id, pr.n
        FROM top_files f
        JOIN artifacts a ON a.id = f.artifact_id
        LEFT JOIN LATERAL (SELECT title FROM conversations
                            WHERE artifact_id = a.id AND title IS NOT NULL
                            ORDER BY started_at NULLS LAST LIMIT 1) c ON true
        JOIN pairs pr ON pr.artifact_id = f.artifact_id
        ORDER BY f.mentions DESC, f.artifact_id
        "#
    )))
    .bind(entity_ids)
    .bind(max_files)
    .fetch_all(pool)
    .await?;

    let mut files: Vec<GraphFile> = Vec::new();
    let mut mentions = Vec::with_capacity(rows.len());
    for r in &rows {
        let file_id: Uuid = r.get("artifact_id");
        if files.last().map(|f| f.id) != Some(file_id) {
            files.push(GraphFile {
                id: file_id,
                name: r.get("name"),
                kind: r.get("kind"),
                mentions: r.get("mentions"),
            });
        }
        mentions.push(GraphMention {
            file_id,
            entity_id: r.get("entity_id"),
            count: r.get("n"),
        });
    }
    Ok((files, mentions))
}

/// A document's digest: what it is about and the sentences that say the most.
#[derive(Debug, Serialize)]
pub struct ArtifactDigest {
    pub artifact_id: Uuid,
    /// `extractive`, or `llm:<model>` when a local model reworded it.
    pub method: String,
    pub summary: String,
    pub key_points: Value,
    pub topics: Value,
    pub outline: Value,
    pub takeaways: Value,
    pub open_questions: Value,
    pub stats: Value,
    pub created_at: DateTime<Utc>,
}

/// The digest of `id`, or NotFound while it is not ready (or the file was
/// retracted).
pub async fn artifact_digest(pool: &PgPool, id: Uuid) -> Result<ArtifactDigest, ApiError> {
    let row = sqlx::query(
        "SELECT g.artifact_id, g.method, g.summary, g.key_points, g.topics, g.outline,
                g.takeaways, g.open_questions, g.stats, g.created_at
           FROM document_digests g
           JOIN artifacts a ON a.id = g.artifact_id
          WHERE g.artifact_id = $1 AND a.retracted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("no digest for this file (yet)".to_string()))?;
    Ok(ArtifactDigest {
        artifact_id: row.get("artifact_id"),
        method: row.get("method"),
        summary: row.get("summary"),
        key_points: row.get("key_points"),
        topics: row.get("topics"),
        outline: row.get("outline"),
        takeaways: row.get("takeaways"),
        open_questions: row.get("open_questions"),
        stats: row.get("stats"),
        created_at: row.get("created_at"),
    })
}
