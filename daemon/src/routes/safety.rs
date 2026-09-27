//! Semantic-safety surface: inference certificates, why something was (or
//! was not) concluded automatically, and the user decisions and retractions
//! that feed back into it. Cores are shared with gRPC.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::ApiError;
use crate::safety::explained;
use crate::safety::service::{self, RetractionReport};
use crate::safety::store::{self, CertificateFilter, CertificateView};
use crate::AppState;

/// GET /certificates — filter by conclusion, subject, source artifact,
/// reason code, outcome, kind or rule.
pub async fn list_certificates(
    State(state): State<AppState>,
    Query(filter): Query<CertificateFilter>,
) -> Result<Json<Value>, ApiError> {
    let items = store::list(&state.pool, &filter).await?;
    Ok(Json(json!({ "items": items })))
}

/// GET /certificates/{id}
pub async fn get_certificate(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<CertificateView>, ApiError> {
    Ok(Json(store::get(&state.pool, id).await?))
}

/// GET /certificates/{id}/chain — what caused it to be withdrawn, its
/// history for the same conclusion, and what it caused in turn.
pub async fn certificate_chain(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(store::chain(&state.pool, id).await?))
}

/// GET /certificates/{id}/affected — conclusions withdrawn because of this
/// one (a split, a "not a duplicate", a removed source).
pub async fn certificate_affected(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let items = store::affected_by(&state.pool, id).await?;
    Ok(Json(json!({ "items": items })))
}

/// GET /safety/summary — counts by outcome and by reason code.
pub async fn safety_summary(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        serde_json::to_value(store::summary(&state.pool).await?)
            .map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

/// GET /artifacts/{id}/conclusions — everything concluded from a source.
pub async fn artifact_conclusions(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(mut filter): Query<CertificateFilter>,
) -> Result<Json<Value>, ApiError> {
    filter.artifact_id = Some(id);
    let items = store::list(&state.pool, &filter).await?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize, Default)]
pub struct RetractRequest {
    pub reason: Option<String>,
    /// Also delete the artifact (and its stored bytes).
    #[serde(default)]
    pub delete: bool,
    pub actor: Option<String>,
}

/// POST /artifacts/{id}/retract
pub async fn retract_artifact(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<RetractRequest>>,
) -> Result<Json<RetractionReport>, ApiError> {
    let req = body.map(|b| b.0).unwrap_or_default();
    Ok(Json(
        service::retract_artifact(&state.pool, id, req.reason, req.delete, req.actor).await?,
    ))
}

/// DELETE /artifacts/{id} — retract, then delete.
pub async fn delete_artifact(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RetractionReport>, ApiError> {
    Ok(Json(
        service::retract_artifact(&state.pool, id, None, true, None).await?,
    ))
}

#[derive(Deserialize)]
pub struct DerivationRequest {
    pub parent_id: Uuid,
    /// copy | summary | export | reingest | version | correction | other
    pub kind: String,
}

/// POST /artifacts/{id}/derivations — declare that this artifact was derived
/// from another (it will not count as independent corroboration).
pub async fn add_derivation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<DerivationRequest>,
) -> Result<Json<Value>, ApiError> {
    let withdrawn = service::add_derivation(&state.pool, id, req.parent_id, &req.kind).await?;
    Ok(Json(json!({
        "child_id": id,
        "parent_id": req.parent_id,
        "kind": req.kind,
        "certificates_withdrawn": withdrawn,
    })))
}

/// GET /units/{id}/support — sources, source families and the confidence
/// independent support justifies.
pub async fn unit_support(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(service::unit_support(&state.pool, id).await?))
}

#[derive(Deserialize)]
pub struct RevisionRequest {
    pub statement: String,
    pub value: Option<String>,
    pub model_version: String,
}

/// POST /units/{id}/revisions — record a newer extractor's reading of this
/// unit's source; disagreement is kept and routed to review, never applied.
pub async fn unit_revision(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RevisionRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.statement.trim().is_empty() || req.model_version.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "statement and model_version are required".into(),
        ));
    }
    Ok(Json(
        service::apply_extraction_revision(
            &state.pool,
            id,
            req.statement.trim(),
            req.value,
            req.model_version.trim(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
pub struct NotDuplicateRequest {
    pub other_id: Uuid,
    pub note: Option<String>,
}

/// POST /images/{id}/not-duplicate
pub async fn not_duplicate(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<NotDuplicateRequest>,
) -> Result<Json<Value>, ApiError> {
    let event = service::mark_not_duplicate(&state.pool, id, req.other_id, req.note).await?;
    Ok(Json(json!({
        "image_id": id,
        "other_id": req.other_id,
        "certificate": event,
    })))
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// GET /contradictions/explained-away — flagged pairs Gather decided are not
/// contradictions, with the reason, for a person to spot-check.
pub async fn list_explained_away(
    State(state): State<AppState>,
    Query(q): Query<PageQuery>,
) -> Result<Json<explained::ExplainedAwayPage>, ApiError> {
    Ok(Json(
        explained::list(&state.pool, q.limit.unwrap_or(100), q.offset.unwrap_or(0)).await?,
    ))
}

#[derive(Deserialize, Default)]
pub struct VerdictRequest {
    pub note: Option<String>,
}

/// POST /contradictions/explained-away/{certificate}/confirm — "this is a
/// real conflict": report it, and never explain it away again.
pub async fn confirm_explained_away(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<VerdictRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    Ok(Json(explained::confirm(&state.pool, id, note).await?))
}

/// POST /contradictions/explained-away/{certificate}/agree — "the
/// explanation is right": the pair counts as not a conflict.
pub async fn agree_explained_away(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<VerdictRequest>>,
) -> Result<Json<Value>, ApiError> {
    let note = body.and_then(|b| b.0.note);
    Ok(Json(explained::agree(&state.pool, id, note).await?))
}
