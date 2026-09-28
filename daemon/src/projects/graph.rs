//! One project as a graph: the project, its folders and files, the entities
//! its files mention and how those relate, and the projects most like it.
//! The same shape as the whole-collection overview, so one view draws both.

use std::collections::{BTreeMap, HashSet};

use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::similarity;
use crate::error::ApiError;
use crate::library::{
    relations_among, GraphContains, GraphEntity, GraphFile, GraphFolder, GraphMention,
    GraphOverview, GraphProject, GraphSimilar,
};

/// Similar projects shown around this one.
const SIMILAR: usize = 5;

/// The graph of project `id`: at most `max_files` files (those with the most
/// read from them first) with the folders above them, and at most
/// `max_entities` entities (the most mentioned first).
pub async fn project_graph(
    pool: &PgPool,
    id: Uuid,
    max_files: i64,
    max_entities: i64,
    compare_max: usize,
) -> Result<GraphOverview, ApiError> {
    let row = sqlx::query(
        "SELECT p.id, p.name, p.source, \
                count(i.id) FILTER (WHERE i.item_kind = 'file') AS files \
         FROM projects p LEFT JOIN project_items i ON i.project_id = p.id \
         WHERE p.id = $1 GROUP BY p.id",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("project {id}")))?;
    let mut projects = vec![GraphProject {
        id,
        name: row.get("name"),
        source: row.get("source"),
        files: row.get("files"),
    }];

    // Files kept in Gather, the most read-from first. A file at two paths is
    // one node held by both folders.
    let items = sqlx::query(
        "SELECT i.id, i.parent_id, i.name, i.artifact_id, a.kind::text AS kind, \
                coalesce(u.units, 0) AS units, count(*) OVER () AS total \
         FROM project_items i \
         JOIN artifacts a ON a.id = i.artifact_id \
         LEFT JOIN LATERAL (SELECT count(*) AS units FROM atomic_unit_provenance p \
                             WHERE p.artifact_id = i.artifact_id) u ON true \
         WHERE i.project_id = $1 AND i.item_kind = 'file' \
         ORDER BY coalesce(u.units, 0) DESC, i.path \
         LIMIT $2",
    )
    .bind(id)
    .bind(max_files)
    .fetch_all(pool)
    .await?;
    let files_total: i64 = items.first().map(|r| r.get("total")).unwrap_or(0);

    // Folders above the files shown: walk up from each file's parent.
    let all_folders: BTreeMap<Uuid, (Option<Uuid>, String, String)> = sqlx::query(
        "SELECT id, parent_id, name, path FROM project_items \
         WHERE project_id = $1 AND item_kind = 'folder' AND status = 'folder'",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| {
        (
            r.get("id"),
            (r.get("parent_id"), r.get("name"), r.get("path")),
        )
    })
    .collect();

    let mut files: Vec<GraphFile> = Vec::new();
    let mut seen_files = HashSet::new();
    let mut contains = Vec::new();
    let mut folder_ids = HashSet::new();
    let hold = |parent: Option<Uuid>, child_type: &'static str, child: Uuid| GraphContains {
        parent_type: if parent.is_some() {
            "folder"
        } else {
            "project"
        },
        parent: parent.unwrap_or(id),
        child_type,
        child,
    };
    for r in &items {
        let artifact: Uuid = r.get("artifact_id");
        let parent: Option<Uuid> = r.get("parent_id");
        if seen_files.insert(artifact) {
            files.push(GraphFile {
                id: artifact,
                name: r.get("name"),
                kind: r.get("kind"),
                mentions: r.get("units"),
            });
        }
        contains.push(hold(parent, "file", artifact));
        let mut next = parent;
        while let Some(folder) = next {
            if !folder_ids.insert(folder) {
                break; // its ancestors are already in
            }
            let up = all_folders.get(&folder).and_then(|f| f.0);
            contains.push(hold(up, "folder", folder));
            next = up;
        }
    }
    let folders: Vec<GraphFolder> = all_folders
        .iter()
        .filter(|(fid, _)| folder_ids.contains(fid))
        .map(|(fid, (_, name, path))| GraphFolder {
            id: *fid,
            project_id: id,
            name: name.clone(),
            path: path.clone(),
        })
        .collect();

    // Entities the shown files mention, the most mentioned first.
    let artifact_ids: Vec<Uuid> = files.iter().map(|f| f.id).collect();
    let pairs = sqlx::query(
        "WITH units AS ( \
             SELECT DISTINCT pv.artifact_id, pv.atomic_unit_id FROM atomic_unit_provenance pv \
              WHERE pv.artifact_id = ANY($1)), \
         touched AS ( \
             SELECT un.artifact_id, un.atomic_unit_id, u.subject_entity_id AS entity_id \
               FROM units un JOIN atomic_units u ON u.id = un.atomic_unit_id \
              WHERE u.status IN ('active', 'disputed') AND u.subject_entity_id IS NOT NULL \
             UNION \
             SELECT un.artifact_id, un.atomic_unit_id, y.entity_id \
               FROM units un JOIN relationships r ON r.atomic_unit_id = un.atomic_unit_id \
              CROSS JOIN LATERAL (VALUES (r.source_entity_id), (r.target_entity_id)) y(entity_id) \
              WHERE r.status = 'active'), \
         pairs AS ( \
             SELECT artifact_id, entity_id, count(DISTINCT atomic_unit_id)::bigint AS n \
               FROM touched GROUP BY 1, 2), \
         ranked AS ( \
             SELECT e.id, e.name, e.kind::text AS kind, sum(p.n)::bigint AS weight \
               FROM pairs p JOIN entities e ON e.id = p.entity_id \
                AND e.merged_into_entity_id IS NULL \
              GROUP BY e.id ORDER BY weight DESC, e.name LIMIT $2) \
         SELECT r.id, r.name, r.kind, r.weight, p.artifact_id, p.n, \
                (SELECT count(DISTINCT entity_id) FROM pairs) AS total \
         FROM ranked r JOIN pairs p ON p.entity_id = r.id \
         ORDER BY r.weight DESC, r.name, p.artifact_id",
    )
    .bind(&artifact_ids)
    .bind(max_entities)
    .fetch_all(pool)
    .await?;
    let entity_total: i64 = pairs.first().map(|r| r.get("total")).unwrap_or(0);
    let mut entities: Vec<GraphEntity> = Vec::new();
    let mut mentions = Vec::with_capacity(pairs.len());
    for r in &pairs {
        let eid: Uuid = r.get("id");
        if entities.last().map(|e| e.id) != Some(eid) {
            entities.push(GraphEntity {
                id: eid,
                name: r.get("name"),
                kind: r.get("kind"),
                weight: r.get("weight"),
            });
        }
        mentions.push(GraphMention {
            file_id: r.get("artifact_id"),
            entity_id: eid,
            count: r.get("n"),
        });
    }
    let entity_ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
    let relations = relations_among(pool, &entity_ids).await?;

    // The projects most like this one, linked to it.
    let loaded = similarity::load(pool, compare_max).await?;
    let ranked = similarity::rank(id, &loaded, SIMILAR).unwrap_or_default();
    let ranked = similarity::with_examples(pool, id, ranked).await?;
    let mut similar = Vec::new();
    for s in ranked {
        let files = sqlx::query_scalar(
            "SELECT count(*) FROM project_items WHERE project_id = $1 AND item_kind = 'file'",
        )
        .bind(s.project_id)
        .fetch_one(pool)
        .await?;
        let source = sqlx::query_scalar("SELECT source FROM projects WHERE id = $1")
            .bind(s.project_id)
            .fetch_one(pool)
            .await?;
        projects.push(GraphProject {
            id: s.project_id,
            name: s.name,
            source,
            files,
        });
        similar.push(GraphSimilar {
            a: id,
            b: s.project_id,
            score: s.comparison.score,
            reasons: s.comparison.reasons,
        });
    }

    Ok(GraphOverview {
        truncated: files_total > items.len() as i64 || entity_total > entities.len() as i64,
        entities,
        files,
        relations,
        mentions,
        projects,
        folders,
        contains,
        similar,
        entity_total,
    })
}
