//! gRPC EntityService — mirrors the REST entity-resolution surface
//! (routes/entities.rs). Merge, dismiss, and add-alias call the shared
//! crate::entities cores, so persistence semantics stay identical across the
//! two API surfaces; list/detail re-issue the same SQL and map into proto.

use sqlx::Row;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::convert::timestamp;
use super::{pb, status_from};
use crate::entities;
use crate::AppState;

pub struct EntityApi {
    pub state: AppState,
}

fn parse_uuid(raw: &str, field: &str) -> Result<Uuid, Status> {
    raw.parse()
        .map_err(|_| Status::invalid_argument(format!("{field} is not a valid UUID")))
}

fn entity_ref_to_pb(e: entities::EntityRef) -> pb::Entity {
    pb::Entity {
        id: e.id.to_string(),
        name: e.name,
        kind: e.kind,
        description: String::new(),
    }
}

async fn load_entity_detail(state: &AppState, id: Uuid) -> Result<pb::EntityDetail, Status> {
    let row = sqlx::query(
        "SELECT id, name, kind::text AS kind, description, merged_into_entity_id, created_at \
         FROM entities WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| status_from(e.into()))?
    .ok_or_else(|| Status::not_found(format!("entity {id}")))?;

    let aliases: Vec<String> =
        sqlx::query_scalar("SELECT alias FROM entity_aliases WHERE entity_id = $1 ORDER BY alias")
            .bind(id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| status_from(e.into()))?;

    let audit_rows = sqlx::query(
        "SELECT action, actor, note, created_at, winner_entity_id, loser_entity_id \
         FROM entity_merge_audit \
         WHERE winner_entity_id = $1 OR loser_entity_id = $1 ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| status_from(e.into()))?;

    let audit = audit_rows
        .iter()
        .map(|a| pb::EntityMergeAuditEntry {
            action: a.get("action"),
            actor: a.get("actor"),
            note: a.get::<Option<String>, _>("note").unwrap_or_default(),
            winner_entity_id: a.get::<Uuid, _>("winner_entity_id").to_string(),
            loser_entity_id: a.get::<Uuid, _>("loser_entity_id").to_string(),
            created_at: timestamp(Some(a.get("created_at"))),
        })
        .collect();

    Ok(pb::EntityDetail {
        id: row.get::<Uuid, _>("id").to_string(),
        name: row.get("name"),
        kind: row.get("kind"),
        description: row
            .get::<Option<String>, _>("description")
            .unwrap_or_default(),
        merged_into_entity_id: row
            .get::<Option<Uuid>, _>("merged_into_entity_id")
            .map(|u| u.to_string())
            .unwrap_or_default(),
        created_at: timestamp(Some(row.get("created_at"))),
        aliases,
        audit,
    })
}

#[tonic::async_trait]
impl pb::entity_service_server::EntityService for EntityApi {
    async fn list_entities(
        &self,
        request: Request<pb::ListEntitiesRequest>,
    ) -> Result<Response<pb::ListEntitiesResponse>, Status> {
        let req = request.into_inner();
        let limit = if req.limit <= 0 {
            50
        } else {
            (req.limit as i64).min(500)
        };
        let offset = req.offset.max(0) as i64;
        let q = Some(req.q).filter(|s| !s.is_empty());
        let kind = Some(req.kind).filter(|s| !s.is_empty());

        let rows = sqlx::query(
            r#"
            SELECT e.id, e.name, e.kind::text AS kind, e.description,
                   e.merged_into_entity_id, e.created_at,
                   (SELECT count(*) FROM entity_aliases a WHERE a.entity_id = e.id) AS alias_count,
                   (SELECT count(*) FROM relationships r
                     WHERE r.source_entity_id = e.id OR r.target_entity_id = e.id) AS edge_count
            FROM entities e
            WHERE ($1 OR e.merged_into_entity_id IS NULL)
              AND ($2::text IS NULL OR e.name ILIKE '%' || $2 || '%')
              AND ($3::text IS NULL OR e.kind::text = $3)
            ORDER BY e.name
            LIMIT $4 OFFSET $5
            "#,
        )
        .bind(req.include_merged)
        .bind(&q)
        .bind(&kind)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.state.pool)
        .await
        .map_err(|e| status_from(e.into()))?;

        let items = rows
            .iter()
            .map(|r| pb::EntitySummary {
                id: r.get::<Uuid, _>("id").to_string(),
                name: r.get("name"),
                kind: r.get("kind"),
                description: r
                    .get::<Option<String>, _>("description")
                    .unwrap_or_default(),
                merged_into_entity_id: r
                    .get::<Option<Uuid>, _>("merged_into_entity_id")
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
                alias_count: r.get("alias_count"),
                edge_count: r.get("edge_count"),
                created_at: timestamp(Some(r.get("created_at"))),
            })
            .collect();

        Ok(Response::new(pb::ListEntitiesResponse { items }))
    }

    async fn list_merge_suggestions(
        &self,
        request: Request<pb::ListMergeSuggestionsRequest>,
    ) -> Result<Response<pb::ListMergeSuggestionsResponse>, Status> {
        let req = request.into_inner();
        // A non-finite threshold (NaN) survives clamp and makes every
        // `score < threshold` comparison false, so merge_suggestions would
        // clone every pair before applying the limit — reject it up front.
        if !req.threshold.is_finite() {
            return Err(Status::invalid_argument(
                "threshold must be a finite number",
            ));
        }
        let threshold = if req.threshold <= 0.0 {
            entities::DEFAULT_THRESHOLD
        } else {
            req.threshold.clamp(0.0, 1.0)
        };
        let limit = if req.limit <= 0 {
            50
        } else {
            (req.limit as i64).min(500)
        };

        let suggestions = entities::merge_suggestions(&self.state.pool, threshold, limit)
            .await
            .map_err(status_from)?;

        let items = suggestions
            .into_iter()
            .map(|s| pb::SuggestedMerge {
                a: Some(entity_ref_to_pb(s.a)),
                b: Some(entity_ref_to_pb(s.b)),
                score: s.score,
                method: s.method.to_string(),
            })
            .collect();

        Ok(Response::new(pb::ListMergeSuggestionsResponse {
            items,
            threshold,
        }))
    }

    async fn get_entity(
        &self,
        request: Request<pb::GetEntityRequest>,
    ) -> Result<Response<pb::EntityDetail>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        Ok(Response::new(load_entity_detail(&self.state, id).await?))
    }

    async fn merge_entities(
        &self,
        request: Request<pb::MergeEntitiesRequest>,
    ) -> Result<Response<pb::MergeOutcome>, Status> {
        let req = request.into_inner();
        let winner_id = parse_uuid(&req.winner_id, "winner_id")?;
        let loser_id = parse_uuid(&req.loser_id, "loser_id")?;
        let outcome = entities::merge_entities(
            &self.state.pool,
            winner_id,
            loser_id,
            Some(req.note).filter(|n| !n.is_empty()),
            Some(req.actor).filter(|a| !a.is_empty()),
        )
        .await
        .map_err(status_from)?;

        Ok(Response::new(pb::MergeOutcome {
            winner_id: outcome.winner_id.to_string(),
            loser_id: outcome.loser_id.to_string(),
            winner_name: outcome.winner_name,
            loser_name: outcome.loser_name,
            aliases_added: outcome.aliases_added,
            units_repointed: outcome.units_repointed,
            units_requeued_for_scan: outcome.units_requeued_for_scan,
            relationships_repointed: outcome.relationships_repointed,
            relationships_dropped: outcome.relationships_dropped,
            descendants_flattened: outcome.descendants_flattened,
        }))
    }

    async fn unmerge_entity(
        &self,
        request: Request<pb::UnmergeEntityRequest>,
    ) -> Result<Response<pb::UnmergeOutcome>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.entity_id, "entity_id")?;
        let outcome = entities::unmerge_entity(
            &self.state.pool,
            id,
            Some(req.note).filter(|n| !n.is_empty()),
            Some(req.actor).filter(|a| !a.is_empty()),
        )
        .await
        .map_err(status_from)?;
        Ok(Response::new(pb::UnmergeOutcome {
            winner_id: outcome.winner_id.to_string(),
            loser_id: outcome.loser_id.to_string(),
            units_restored: outcome.units_restored,
            relationships_restored: outcome.relationships_restored,
            aliases_restored: outcome.aliases_restored,
            descendants_restored: outcome.descendants_restored,
        }))
    }

    async fn dismiss_merge_suggestion(
        &self,
        request: Request<pb::DismissMergeSuggestionRequest>,
    ) -> Result<Response<pb::DismissMergeSuggestionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.id, "id")?;
        let other_id = parse_uuid(&req.other_id, "other_id")?;
        entities::dismiss_suggestion(
            &self.state.pool,
            id,
            other_id,
            Some(req.note).filter(|n| !n.is_empty()),
            Some(req.actor).filter(|a| !a.is_empty()),
        )
        .await
        .map_err(status_from)?;
        Ok(Response::new(pb::DismissMergeSuggestionResponse {}))
    }

    async fn add_alias(
        &self,
        request: Request<pb::AddAliasRequest>,
    ) -> Result<Response<pb::AddAliasResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.entity_id, "entity_id")?;
        let added = entities::add_alias(&self.state.pool, id, &req.alias)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::AddAliasResponse { added }))
    }
}
