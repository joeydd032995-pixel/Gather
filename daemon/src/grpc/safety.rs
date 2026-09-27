//! gRPC SafetyService: the same cores as the REST safety routes.

use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::convert::{non_empty, parse_uuid, prost_struct, timestamp};
use super::{pb, status_from};
use crate::safety::service;
use crate::safety::store::{self, CertificateFilter, CertificateView};
use crate::AppState;

pub struct SafetyApi {
    pub state: AppState,
}

fn ids(v: &[Uuid]) -> Vec<String> {
    v.iter().map(Uuid::to_string).collect()
}

fn opt_uuid(s: String, field: &str) -> Result<Option<Uuid>, Status> {
    non_empty(s).map(|s| parse_uuid(&s, field)).transpose()
}

pub fn certificate_to_pb(c: CertificateView) -> pb::Certificate {
    pb::Certificate {
        id: c.id.to_string(),
        conclusion_kind: c.conclusion_kind,
        conclusion_key: c.conclusion_key,
        conclusion_id: c.conclusion_id.map(|u| u.to_string()).unwrap_or_default(),
        subject_ids: ids(&c.subject_ids),
        rule_id: c.rule_id,
        rule_version: c.rule_version,
        decision: c.decision,
        outcome: c.outcome,
        evidence_class: c.evidence_class,
        source_artifact_ids: ids(&c.source_artifact_ids),
        source_family_ids: ids(&c.source_family_ids),
        model_version: c.model_version.unwrap_or_default(),
        reason_codes: c.reason_codes,
        reasons: c
            .reasons
            .into_iter()
            .map(|r| pb::CertificateReason {
                code: r.code,
                text: r.text,
            })
            .collect(),
        explanation: c.explanation,
        evidence_digest: c.evidence_digest,
        detail: prost_struct(&serde_json::json!({
            "inputs": c.inputs,
            "config": c.config,
            "scope": c.scope,
            "temporal": c.temporal,
            "predicates": c.predicates,
        })),
        created_at: timestamp(Some(c.created_at)),
        superseded_at: timestamp(c.superseded_at),
        retracted_at: timestamp(c.retracted_at),
        status_reason: c.status_reason.unwrap_or_default(),
        caused_by: c.caused_by.map(|u| u.to_string()).unwrap_or_default(),
    }
}

#[tonic::async_trait]
impl pb::safety_service_server::SafetyService for SafetyApi {
    async fn get_certificate(
        &self,
        request: Request<pb::GetCertificateRequest>,
    ) -> Result<Response<pb::Certificate>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        let c = store::get(&self.state.pool, id)
            .await
            .map_err(status_from)?;
        Ok(Response::new(certificate_to_pb(c)))
    }

    async fn list_certificates(
        &self,
        request: Request<pb::ListCertificatesRequest>,
    ) -> Result<Response<pb::ListCertificatesResponse>, Status> {
        let r = request.into_inner();
        let filter = CertificateFilter {
            conclusion_id: opt_uuid(r.conclusion_id, "conclusion_id")?,
            subject_id: opt_uuid(r.subject_id, "subject_id")?,
            artifact_id: opt_uuid(r.artifact_id, "artifact_id")?,
            reason: non_empty(r.reason),
            outcome: non_empty(r.outcome),
            kind: non_empty(r.kind),
            rule: non_empty(r.rule),
            live: Some(r.live),
            limit: (r.limit > 0).then_some(i64::from(r.limit)),
            offset: Some(i64::from(r.offset.max(0))),
        };
        let items = store::list(&self.state.pool, &filter)
            .await
            .map_err(status_from)?
            .into_iter()
            .map(certificate_to_pb)
            .collect();
        Ok(Response::new(pb::ListCertificatesResponse { items }))
    }

    async fn list_affected(
        &self,
        request: Request<pb::GetCertificateRequest>,
    ) -> Result<Response<pb::ListCertificatesResponse>, Status> {
        let id = parse_uuid(&request.into_inner().id, "id")?;
        let items = store::affected_by(&self.state.pool, id)
            .await
            .map_err(status_from)?
            .into_iter()
            .map(certificate_to_pb)
            .collect();
        Ok(Response::new(pb::ListCertificatesResponse { items }))
    }

    async fn get_safety_summary(
        &self,
        _request: Request<pb::GetSafetySummaryRequest>,
    ) -> Result<Response<pb::SafetySummary>, Status> {
        let s = store::summary(&self.state.pool)
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::SafetySummary {
            by_outcome: prost_struct(&s.by_outcome),
            review_by_reason: prost_struct(&s.review_by_reason),
            blocked_by_reason: prost_struct(&s.blocked_by_reason),
            by_kind: prost_struct(&s.by_kind),
        }))
    }

    async fn retract_artifact(
        &self,
        request: Request<pb::RetractArtifactRequest>,
    ) -> Result<Response<pb::RetractionReport>, Status> {
        let r = request.into_inner();
        let id = parse_uuid(&r.artifact_id, "artifact_id")?;
        let rep =
            service::retract_artifact(&self.state.pool, id, non_empty(r.reason), r.delete, None)
                .await
                .map_err(status_from)?;
        Ok(Response::new(pb::RetractionReport {
            event_certificate: rep
                .event_certificate
                .map(|u| u.to_string())
                .unwrap_or_default(),
            units_retracted: ids(&rep.units_retracted),
            certificates_withdrawn: ids(&rep.certificates_withdrawn),
            contradictions_withdrawn: rep.contradictions_withdrawn,
            supersessions_reverted: rep.supersessions_reverted,
            images_ungrouped: rep.images_ungrouped,
            deleted: rep.deleted,
        }))
    }

    async fn mark_not_duplicate(
        &self,
        request: Request<pb::MarkNotDuplicateRequest>,
    ) -> Result<Response<pb::MarkNotDuplicateResponse>, Status> {
        let r = request.into_inner();
        let a = parse_uuid(&r.image_id, "image_id")?;
        let b = parse_uuid(&r.other_id, "other_id")?;
        let event = service::mark_not_duplicate(&self.state.pool, a, b, non_empty(r.note))
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::MarkNotDuplicateResponse {
            certificate: event.to_string(),
        }))
    }
}
