//! gRPC services for the autonomous pipeline — Feedback, Cluster, Tuning and
//! Photo. Each RPC is a thin mapping over the same core its REST route calls
//! (routes::{feedback, clusters, tuning, photos}), so the two API surfaces
//! cannot diverge in behaviour.

use tonic::{Request, Response, Status};

use super::convert::{non_empty, parse_uuid, prost_struct, timestamp};
use super::{pb, status_from};
use crate::routes::{clusters, feedback, photos, tuning};
use crate::AppState;

/// Default page size when a request leaves `limit` at 0.
const DEFAULT_LIMIT: i64 = 100;

fn limit_or_default(limit: i32) -> i64 {
    if limit > 0 {
        i64::from(limit)
    } else {
        DEFAULT_LIMIT
    }
}

fn opt_uuid(id: Option<uuid::Uuid>) -> String {
    id.map(|u| u.to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// FeedbackService

pub struct FeedbackApi {
    pub state: AppState,
}

fn unit_response(unit_id: uuid::Uuid, status: &str, statement: String) -> pb::UnitActionResponse {
    pb::UnitActionResponse {
        unit_id: unit_id.to_string(),
        status: status.to_string(),
        statement,
    }
}

fn outcome_to_pb(o: feedback::ReviewOutcome) -> pb::ReviewOutcome {
    pb::ReviewOutcome {
        id: o.id.to_string(),
        action: o.action.to_string(),
        unit_id: opt_uuid(o.unit_id),
        winner_id: opt_uuid(o.winner),
        loser_id: opt_uuid(o.loser),
        pair: o
            .pair
            .map(|p| p.iter().map(ToString::to_string).collect())
            .unwrap_or_default(),
    }
}

#[tonic::async_trait]
impl pb::feedback_service_server::FeedbackService for FeedbackApi {
    async fn reject_unit(
        &self,
        request: Request<pb::UnitActionRequest>,
    ) -> Result<Response<pb::UnitActionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.unit_id, "unit_id")?;
        feedback::reject_unit_core(&self.state.pool, id, non_empty(req.note).as_deref())
            .await
            .map_err(status_from)?;
        Ok(Response::new(unit_response(id, "retracted", String::new())))
    }

    async fn restore_unit(
        &self,
        request: Request<pb::UnitActionRequest>,
    ) -> Result<Response<pb::UnitActionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.unit_id, "unit_id")?;
        feedback::restore_unit_core(&self.state.pool, id, non_empty(req.note).as_deref())
            .await
            .map_err(status_from)?;
        Ok(Response::new(unit_response(id, "active", String::new())))
    }

    async fn confirm_unit(
        &self,
        request: Request<pb::UnitActionRequest>,
    ) -> Result<Response<pb::UnitActionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.unit_id, "unit_id")?;
        let status =
            feedback::confirm_unit_core(&self.state.pool, id, non_empty(req.note).as_deref())
                .await
                .map_err(status_from)?;
        Ok(Response::new(unit_response(id, &status, String::new())))
    }

    async fn edit_unit(
        &self,
        request: Request<pb::EditUnitRequest>,
    ) -> Result<Response<pb::UnitActionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.unit_id, "unit_id")?;
        let (statement, status) = feedback::edit_unit_core(
            &self.state.pool,
            id,
            &req.statement,
            non_empty(req.note).as_deref(),
        )
        .await
        .map_err(status_from)?;
        Ok(Response::new(unit_response(id, &status, statement)))
    }

    async fn list_review(
        &self,
        request: Request<pb::ListReviewRequest>,
    ) -> Result<Response<pb::ListReviewResponse>, Status> {
        let limit = limit_or_default(request.into_inner().limit);
        let items = feedback::list_review_core(&self.state.pool, limit)
            .await
            .map_err(status_from)?
            .into_iter()
            .map(|e| pb::ReviewEntry {
                id: e.id.to_string(),
                target_kind: e.target_kind,
                target_id: e.target_id.to_string(),
                reason: e.reason,
                info_gain: e.info_gain,
                signals: prost_struct(&e.signals),
                statement: e.statement.unwrap_or_default(),
                created_at: timestamp(Some(e.created_at)),
                a_name: e.a_name.unwrap_or_default(),
                b_name: e.b_name.unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(pb::ListReviewResponse { items }))
    }

    async fn accept_review(
        &self,
        request: Request<pb::ReviewActionRequest>,
    ) -> Result<Response<pb::ReviewOutcome>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.id, "id")?;
        let outcome = feedback::accept_review_core(&self.state.pool, id, non_empty(req.note))
            .await
            .map_err(status_from)?;
        Ok(Response::new(outcome_to_pb(outcome)))
    }

    async fn reject_review(
        &self,
        request: Request<pb::ReviewActionRequest>,
    ) -> Result<Response<pb::ReviewOutcome>, Status> {
        let req = request.into_inner();
        let id = parse_uuid(&req.id, "id")?;
        let outcome = feedback::reject_review_core(&self.state.pool, id, non_empty(req.note))
            .await
            .map_err(status_from)?;
        Ok(Response::new(outcome_to_pb(outcome)))
    }

    async fn resolve_review(
        &self,
        request: Request<pb::ReviewActionRequest>,
    ) -> Result<Response<pb::ReviewOutcome>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        feedback::resolve_review_core(&self.state.pool, id)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::ReviewOutcome {
            id: id.to_string(),
            action: "resolved".to_string(),
            ..pb::ReviewOutcome::default()
        }))
    }
}

// ---------------------------------------------------------------------------
// ClusterService

pub struct ClusterApi {
    pub state: AppState,
}

fn summary_to_pb(c: clusters::ClusterSummary) -> pb::ClusterSummary {
    pb::ClusterSummary {
        id: c.id.to_string(),
        kind: c.kind,
        label: c.label,
        cohesion: c.cohesion,
        size: c.size,
        representative_id: opt_uuid(c.representative_id),
        updated_at: timestamp(Some(c.updated_at)),
    }
}

#[tonic::async_trait]
impl pb::cluster_service_server::ClusterService for ClusterApi {
    async fn list_clusters(
        &self,
        request: Request<pb::ListClustersRequest>,
    ) -> Result<Response<pb::ListClustersResponse>, Status> {
        let req = request.into_inner();
        let kind = non_empty(req.kind);
        let items = clusters::list_clusters_core(
            &self.state.pool,
            kind.as_deref(),
            limit_or_default(req.limit),
            i64::from(req.offset.max(0)),
        )
        .await
        .map_err(status_from)?
        .into_iter()
        .map(summary_to_pb)
        .collect();
        Ok(Response::new(pb::ListClustersResponse { items }))
    }

    async fn get_cluster(
        &self,
        request: Request<pb::GetClusterRequest>,
    ) -> Result<Response<pb::ClusterDetail>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        let detail = clusters::get_cluster_core(&self.state.pool, id)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::ClusterDetail {
            summary: Some(summary_to_pb(detail.summary)),
            created_at: timestamp(Some(detail.created_at)),
            members: detail
                .members
                .into_iter()
                .map(|m| pb::ClusterMember {
                    member_kind: m.member_kind,
                    member_id: m.member_id.to_string(),
                    sim: m.sim,
                    statement: m.statement.unwrap_or_default(),
                    filename: m.filename.unwrap_or_default(),
                    taken_at: timestamp(m.taken_at),
                    caption: m.caption.unwrap_or_default(),
                    name: m.name.unwrap_or_default(),
                    merged_into: opt_uuid(m.merged_into),
                })
                .collect(),
        }))
    }
}

// ---------------------------------------------------------------------------
// TuningService

pub struct TuningApi {
    pub state: AppState,
}

#[tonic::async_trait]
impl pb::tuning_service_server::TuningService for TuningApi {
    async fn get_tuning(
        &self,
        _request: Request<pb::GetTuningRequest>,
    ) -> Result<Response<pb::TuningState>, Status> {
        let state = tuning::get_tuning_core(&self.state.pool, &self.state.config)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::TuningState {
            enabled: state.enabled,
            target_precision: state.target_precision,
            min_samples: u32::try_from(state.min_samples).unwrap_or(u32::MAX),
            thresholds: state
                .thresholds
                .into_iter()
                .map(|t| pb::TunedThreshold {
                    key: t.key.to_string(),
                    value: t.value,
                    default_value: t.default,
                    tuned: t.tuned,
                    bound_min: t.bounds.min,
                    bound_max: t.bounds.max,
                })
                .collect(),
            history: state
                .history
                .into_iter()
                .map(|h| pb::TuningChange {
                    key: h.key,
                    old_value: h.old_value,
                    new_value: h.new_value,
                    actor: h.actor,
                    reason: prost_struct(&h.reason),
                    created_at: timestamp(Some(h.created_at)),
                })
                .collect(),
        }))
    }

    async fn reset_tuning(
        &self,
        request: Request<pb::ResetTuningRequest>,
    ) -> Result<Response<pb::ResetTuningResponse>, Status> {
        let key = non_empty(request.into_inner().key);
        let reset = tuning::reset_tuning_core(&self.state.pool, key.as_deref())
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::ResetTuningResponse { reset }))
    }
}

// ---------------------------------------------------------------------------
// PhotoService

pub struct PhotoApi {
    pub state: AppState,
}

#[tonic::async_trait]
impl pb::photo_service_server::PhotoService for PhotoApi {
    async fn get_thumbnail(
        &self,
        request: Request<pb::GetThumbnailRequest>,
    ) -> Result<Response<pb::Thumbnail>, Status> {
        let id = parse_uuid(&request.into_inner().image_id, "image_id")?;
        let data = photos::thumbnail_core(&self.state.pool, id)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::Thumbnail {
            data,
            content_type: "image/jpeg".to_string(),
        }))
    }
}
