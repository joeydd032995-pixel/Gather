//! gRPC ContradictionService — resolution/annotation share the REST cores;
//! list/get map the same SQL into proto types.

use sqlx::Row;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::convert::{
    contradiction_status_from_pb, contradiction_status_to_pb, opt, timestamp, unit_kind_to_pb,
    unit_status_to_pb,
};
use super::query::load_provenance;
use super::{pb, status_from};
use crate::routes::contradictions::{annotate_core, resolve_core, AnnotateRequest, ResolveRequest};
use crate::AppState;

pub struct ContradictionApi {
    pub state: AppState,
}

fn parse_uuid(raw: &str, field: &str) -> Result<Uuid, Status> {
    raw.parse()
        .map_err(|_| Status::invalid_argument(format!("{field} is not a valid UUID")))
}

/// Load one unit as a proto message, with full provenance.
async fn load_unit(state: &AppState, id: Uuid) -> Result<pb::AtomicUnit, Status> {
    let row = sqlx::query(
        r#"
        SELECT id, kind::text AS kind, statement, confidence,
               extraction_method::text AS extraction_method, extraction_model,
               valid_from, valid_to, status::text AS status,
               subject_entity_id, superseded_by_unit_id
        FROM atomic_units WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| status_from(e.into()))?;

    let provenance = load_provenance(state, &[id]).await?;
    Ok(pb::AtomicUnit {
        id: id.to_string(),
        kind: unit_kind_to_pb(row.get("kind")) as i32,
        statement: row.get("statement"),
        confidence: row.get("confidence"),
        extraction_method: row.get("extraction_method"),
        extraction_model: opt(row.get("extraction_model")),
        valid_from: timestamp(row.get("valid_from")),
        valid_to: timestamp(row.get("valid_to")),
        status: unit_status_to_pb(row.get("status")) as i32,
        subject_entity_id: row
            .get::<Option<Uuid>, _>("subject_entity_id")
            .map(|u| u.to_string())
            .unwrap_or_default(),
        superseded_by_unit_id: row
            .get::<Option<Uuid>, _>("superseded_by_unit_id")
            .map(|u| u.to_string())
            .unwrap_or_default(),
        provenance: provenance.into_iter().map(|(_, p)| p).collect(),
    })
}

async fn load_contradiction(state: &AppState, id: Uuid) -> Result<pb::Contradiction, Status> {
    let row = sqlx::query(
        r#"
        SELECT id, unit_a_id, unit_b_id, score, detection_method, explanation,
               status::text AS status, detected_at, resolved_at, resolved_by,
               resolution_note
        FROM contradictions WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| status_from(e.into()))?
    .ok_or_else(|| Status::not_found(format!("contradiction {id}")))?;

    let unit_a = load_unit(state, row.get("unit_a_id")).await?;
    let unit_b = load_unit(state, row.get("unit_b_id")).await?;

    Ok(pb::Contradiction {
        id: id.to_string(),
        unit_a: Some(unit_a),
        unit_b: Some(unit_b),
        score: row.get("score"),
        detection_method: row.get("detection_method"),
        explanation: opt(row.get("explanation")),
        status: contradiction_status_to_pb(row.get("status")) as i32,
        detected_at: timestamp(Some(row.get("detected_at"))),
        resolved_at: timestamp(row.get("resolved_at")),
        resolved_by: opt(row.get("resolved_by")),
        resolution_note: opt(row.get("resolution_note")),
    })
}

#[tonic::async_trait]
impl pb::contradiction_service_server::ContradictionService for ContradictionApi {
    async fn list_contradictions(
        &self,
        request: Request<pb::ListContradictionsRequest>,
    ) -> Result<Response<pb::ListContradictionsResponse>, Status> {
        let req = request.into_inner();
        let status = pb::ContradictionStatus::try_from(req.status)
            .ok()
            .and_then(contradiction_status_from_pb);
        let limit = if req.limit <= 0 {
            50
        } else {
            (req.limit as i64).min(500)
        };
        let offset = req.offset.max(0) as i64;

        let ids: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT id FROM contradictions
            WHERE ($1::contradiction_status IS NULL OR status = $1::contradiction_status)
            ORDER BY score DESC, detected_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(status)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?;

        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            items.push(load_contradiction(&self.state, id).await?);
        }
        Ok(Response::new(pb::ListContradictionsResponse { items }))
    }

    async fn get_contradiction(
        &self,
        request: Request<pb::GetContradictionRequest>,
    ) -> Result<Response<pb::Contradiction>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        Ok(Response::new(load_contradiction(&self.state, id).await?))
    }

    async fn resolve_contradiction(
        &self,
        request: Request<pb::ResolveContradictionRequest>,
    ) -> Result<Response<pb::Contradiction>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.id, "id")?;
        let resolution = pb::ContradictionStatus::try_from(req.resolution)
            .ok()
            .and_then(contradiction_status_from_pb)
            .filter(|r| *r != "open")
            .ok_or_else(|| {
                Status::invalid_argument(
                    "resolution must be RESOLVED_A, RESOLVED_B, BOTH_VALID or DISMISSED",
                )
            })?;
        resolve_core(
            &self.state.pool,
            id,
            ResolveRequest {
                resolution: resolution.to_string(),
                note: Some(req.note).filter(|n| !n.is_empty()),
                actor: Some(req.actor).filter(|a| !a.is_empty()),
            },
        )
        .await
        .map_err(status_from)?;
        Ok(Response::new(load_contradiction(&self.state, id).await?))
    }

    async fn annotate_contradiction(
        &self,
        request: Request<pb::AnnotateContradictionRequest>,
    ) -> Result<Response<pb::AnnotateContradictionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.id, "id")?;
        let (audit_id, _actor) = annotate_core(
            &self.state.pool,
            id,
            AnnotateRequest {
                note: req.note,
                actor: Some(req.actor).filter(|a| !a.is_empty()),
            },
        )
        .await
        .map_err(status_from)?;
        Ok(Response::new(pb::AnnotateContradictionResponse {
            audit_id: audit_id.to_string(),
        }))
    }
}
