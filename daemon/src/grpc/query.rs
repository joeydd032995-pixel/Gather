//! gRPC QueryService — same SQL as `routes::query`, mapped into proto types;
//! semantic search shares the REST core (including server-side embedding).

use std::time::Instant;

use sqlx::Row;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::convert::{
    artifact_kind_from_pb, artifact_kind_to_pb, opt, prost_struct, timestamp, unit_kind_from_pb,
    unit_kind_to_pb, unit_status_from_pb, unit_status_to_pb,
};
use super::{pb, status_from};
use crate::routes::query::{search_core, SemanticSearchRequest};
use crate::AppState;

pub struct QueryApi {
    pub state: AppState,
}

fn parse_uuid(raw: &str, field: &str) -> Result<Uuid, Status> {
    raw.parse()
        .map_err(|_| Status::invalid_argument(format!("{field} is not a valid UUID")))
}

fn clamp_limit(limit: i32, default: i64, max: i64) -> i64 {
    if limit <= 0 {
        default
    } else {
        (limit as i64).min(max)
    }
}

fn artifact_from_row(row: &sqlx::postgres::PgRow) -> pb::Artifact {
    pb::Artifact {
        id: row.get::<Uuid, _>("id").to_string(),
        kind: artifact_kind_to_pb(row.get("kind")) as i32,
        source_platform: row.get("source_platform"),
        source_format_version: opt(row.get("source_format_version")),
        original_filename: opt(row.get("original_filename")),
        media_type: opt(row.get("media_type")),
        byte_size: row.get("byte_size"),
        content_hash: row.get::<String, _>("content_hash").trim().to_string(),
        version: row.get("version"),
        supersedes_artifact_id: row
            .get::<Option<Uuid>, _>("supersedes_artifact_id")
            .map(|u| u.to_string())
            .unwrap_or_default(),
        source_created_at: timestamp(row.get("source_created_at")),
        ingested_at: timestamp(Some(row.get("ingested_at"))),
        metadata: prost_struct(&row.get::<serde_json::Value, _>("metadata")),
    }
}

const ARTIFACT_COLUMNS: &str = "id, kind::text AS kind, source_platform, source_format_version, \
     original_filename, media_type, byte_size, content_hash, version, \
     supersedes_artifact_id, source_created_at, ingested_at, metadata";

#[tonic::async_trait]
impl pb::query_service_server::QueryService for QueryApi {
    async fn list_artifacts(
        &self,
        request: Request<pb::ListArtifactsRequest>,
    ) -> Result<Response<pb::ListArtifactsResponse>, Status> {
        let req = request.into_inner();
        let kind = pb::ArtifactKind::try_from(req.kind)
            .ok()
            .and_then(artifact_kind_from_pb);
        let platform = Some(req.source_platform).filter(|p| !p.is_empty());
        let limit = clamp_limit(req.limit, 50, 500);
        let offset = req.offset.max(0) as i64;

        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"
            SELECT {ARTIFACT_COLUMNS} FROM artifacts
            WHERE ($1::artifact_kind IS NULL OR kind = $1::artifact_kind)
              AND ($2::text IS NULL OR source_platform = $2)
            ORDER BY ingested_at DESC
            LIMIT $3 OFFSET $4
            "#
        )))
        .bind(kind)
        .bind(platform)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?;

        Ok(Response::new(pb::ListArtifactsResponse {
            items: rows.iter().map(artifact_from_row).collect(),
        }))
    }

    async fn get_artifact(
        &self,
        request: Request<pb::GetArtifactRequest>,
    ) -> Result<Response<pb::Artifact>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?
        .ok_or_else(|| Status::not_found(format!("artifact {id}")))?;
        Ok(Response::new(artifact_from_row(&row)))
    }

    async fn list_atomic_units(
        &self,
        request: Request<pb::ListAtomicUnitsRequest>,
    ) -> Result<Response<pb::ListAtomicUnitsResponse>, Status> {
        let req = request.into_inner();
        let kind = pb::UnitKind::try_from(req.kind)
            .ok()
            .and_then(unit_kind_from_pb);
        let status = pb::UnitStatus::try_from(req.status)
            .ok()
            .and_then(unit_status_from_pb);
        let subject = if req.subject_entity_id.is_empty() {
            None
        } else {
            Some(parse_uuid(&req.subject_entity_id, "subject_entity_id")?)
        };
        let limit = clamp_limit(req.limit, 50, 500);
        let offset = req.offset.max(0) as i64;

        let rows = sqlx::query(
            r#"
            SELECT u.id, u.kind::text AS kind, u.statement, u.confidence,
                   u.extraction_method::text AS extraction_method, u.extraction_model,
                   u.valid_from, u.valid_to, u.status::text AS status,
                   u.subject_entity_id, u.superseded_by_unit_id
            FROM atomic_units u
            WHERE ($1::unit_kind IS NULL OR u.kind = $1::unit_kind)
              AND ($2::unit_status IS NULL OR u.status = $2::unit_status)
              AND ($3::uuid IS NULL OR u.subject_entity_id = $3)
            ORDER BY u.created_at DESC
            LIMIT $4 OFFSET $5
            "#,
        )
        .bind(kind)
        .bind(status)
        .bind(subject)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?;

        let unit_ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
        let provenance = load_provenance(&self.state, &unit_ids).await?;

        let items = rows
            .iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                pb::AtomicUnit {
                    id: id.to_string(),
                    kind: unit_kind_to_pb(r.get("kind")) as i32,
                    statement: r.get("statement"),
                    confidence: r.get("confidence"),
                    extraction_method: r.get("extraction_method"),
                    extraction_model: opt(r.get("extraction_model")),
                    valid_from: timestamp(r.get("valid_from")),
                    valid_to: timestamp(r.get("valid_to")),
                    status: unit_status_to_pb(r.get("status")) as i32,
                    subject_entity_id: r
                        .get::<Option<Uuid>, _>("subject_entity_id")
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    superseded_by_unit_id: r
                        .get::<Option<Uuid>, _>("superseded_by_unit_id")
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    provenance: provenance
                        .iter()
                        .filter(|(unit_id, _)| *unit_id == id)
                        .map(|(_, p)| p.clone())
                        .collect(),
                }
            })
            .collect();
        Ok(Response::new(pb::ListAtomicUnitsResponse { items }))
    }

    async fn get_entity_graph(
        &self,
        request: Request<pb::GetEntityGraphRequest>,
    ) -> Result<Response<pb::GetEntityGraphResponse>, Status> {
        let req = request.into_inner();
        let entity_id = parse_uuid(&req.entity_id, "entity_id")?;
        let depth = req.depth.clamp(1, 5);
        let started = Instant::now();

        let root = sqlx::query(
            "SELECT id, name, kind::text AS kind, description FROM entities WHERE id = $1",
        )
        .bind(entity_id)
        .fetch_optional(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?
        .ok_or_else(|| Status::not_found(format!("entity {entity_id}")))?;

        let edges = sqlx::query(
            r#"SELECT depth, relationship_id, source_entity_id, target_entity_id,
                      relation_type, confidence
               FROM entity_neighborhood($1, $2)"#,
        )
        .bind(entity_id)
        .bind(depth)
        .fetch_all(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?;

        let mut node_ids: Vec<Uuid> = edges
            .iter()
            .flat_map(|e| {
                [
                    e.get::<Uuid, _>("source_entity_id"),
                    e.get::<Uuid, _>("target_entity_id"),
                ]
            })
            .collect();
        node_ids.push(entity_id);
        node_ids.sort();
        node_ids.dedup();

        let nodes = sqlx::query(
            "SELECT id, name, kind::text AS kind, description FROM entities WHERE id = ANY($1)",
        )
        .bind(&node_ids)
        .fetch_all(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?;

        let elapsed = started.elapsed();
        metrics::histogram!("gather_graph_query_duration_seconds").record(elapsed.as_secs_f64());

        let entity_from = |r: &sqlx::postgres::PgRow| pb::Entity {
            id: r.get::<Uuid, _>("id").to_string(),
            name: r.get("name"),
            kind: r.get("kind"),
            description: opt(r.get("description")),
        };
        Ok(Response::new(pb::GetEntityGraphResponse {
            root: Some(entity_from(&root)),
            nodes: nodes.iter().map(entity_from).collect(),
            edges: edges
                .iter()
                .map(|e| pb::Relationship {
                    id: e.get::<Uuid, _>("relationship_id").to_string(),
                    source_entity_id: e.get::<Uuid, _>("source_entity_id").to_string(),
                    target_entity_id: e.get::<Uuid, _>("target_entity_id").to_string(),
                    relation_type: e.get("relation_type"),
                    confidence: e.get("confidence"),
                    depth: e.get("depth"),
                })
                .collect(),
            query_ms: elapsed.as_millis() as u64,
        }))
    }

    async fn semantic_search(
        &self,
        request: Request<pb::SemanticSearchRequest>,
    ) -> Result<Response<pb::SemanticSearchResponse>, Status> {
        let req = request.into_inner();
        let core_req = SemanticSearchRequest {
            text: Some(req.text).filter(|t| !t.is_empty()),
            embedding: Some(req.embedding).filter(|e| !e.is_empty()),
            scope: Some(req.scope).filter(|s| !s.is_empty()),
            limit: Some(req.limit as i64).filter(|l| *l > 0),
        };
        let (scope, hits) = search_core(&self.state, core_req)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::SemanticSearchResponse {
            hits: hits
                .into_iter()
                .map(|h| pb::semantic_search_response::Hit {
                    id: h.id.to_string(),
                    scope: scope.clone(),
                    content: h.content,
                    score: h.score,
                    artifact_id: h.artifact_id.map(|u| u.to_string()).unwrap_or_default(),
                })
                .collect(),
        }))
    }
}

/// Batched provenance load for a set of units, joined to artifact context.
pub(crate) async fn load_provenance(
    state: &AppState,
    unit_ids: &[Uuid],
) -> Result<Vec<(Uuid, pb::Provenance)>, Status> {
    if unit_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT p.atomic_unit_id, p.artifact_id, p.message_id, p.document_segment_id,
               p.image_id, p.char_start, p.char_end, p.quote,
               a.kind::text AS artifact_kind, a.source_platform
        FROM atomic_unit_provenance p
        JOIN artifacts a ON a.id = p.artifact_id
        WHERE p.atomic_unit_id = ANY($1)
        ORDER BY p.created_at
        "#,
    )
    .bind(unit_ids)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| status_from(e.into()))?;

    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<Uuid, _>("atomic_unit_id"),
                pb::Provenance {
                    artifact_id: r.get::<Uuid, _>("artifact_id").to_string(),
                    artifact_kind: artifact_kind_to_pb(r.get("artifact_kind")) as i32,
                    source_platform: r.get("source_platform"),
                    message_id: r
                        .get::<Option<Uuid>, _>("message_id")
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    document_segment_id: r
                        .get::<Option<Uuid>, _>("document_segment_id")
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    image_id: r
                        .get::<Option<Uuid>, _>("image_id")
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    char_start: r.get::<Option<i32>, _>("char_start").unwrap_or(-1),
                    char_end: r.get::<Option<i32>, _>("char_end").unwrap_or(-1),
                    quote: opt(r.get("quote")),
                },
            )
        })
        .collect())
}
