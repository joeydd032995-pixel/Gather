pub mod clusters;
pub mod contradictions;
pub mod entities;
pub mod export;
pub mod feedback;
pub mod health;
pub mod ingest;
pub mod library;
pub mod photos;
pub mod projects;
pub mod query;
pub mod safety;
pub mod tuning;

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::{auth, AppState};

/// The only browser origins allowed to call the loopback API: the Tauri
/// webview and the Vite dev server. Everything else is cross-origin-blocked
/// even on localhost.
const ALLOWED_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
    "http://localhost:1420",
];

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        // ingestion
        .route("/ingest/chat-export", post(ingest::ingest_chat_export))
        .route("/ingest/agent-log", post(ingest::ingest_agent_log))
        .route("/ingest/files", post(ingest::ingest_files))
        // projects: a folder or .zip kept as its tree
        .route(
            "/projects",
            get(projects::list_projects).post(projects::create_project),
        )
        .route("/projects/import", post(projects::import_project))
        .route(
            "/projects/{id}",
            get(projects::get_project).delete(projects::delete_project),
        )
        .route("/projects/{id}/files", post(projects::add_files))
        .route("/projects/{id}/graph", get(projects::project_graph))
        .route("/projects/{id}/similar", get(projects::similar_projects))
        // query
        .route("/artifacts", get(query::list_artifacts))
        .route(
            "/artifacts/{id}",
            get(query::get_artifact).merge(delete(safety::delete_artifact)),
        )
        .route("/artifacts/{id}/retract", post(safety::retract_artifact))
        .route("/artifacts/{id}/derivations", post(safety::add_derivation))
        .route(
            "/artifacts/{id}/conclusions",
            get(safety::artifact_conclusions),
        )
        .route("/artifacts/{id}/content", get(library::artifact_content))
        .route("/graph", get(library::graph_overview))
        .route("/atomic-units", get(query::list_atomic_units))
        .route("/entities/{id}/graph", get(query::entity_graph))
        .route("/search/semantic", post(query::semantic_search))
        // entity resolution — aliases, suggested duplicates, merges.
        // The static /merge-suggestions path is registered before the dynamic
        // /{id} so it is not swallowed as an entity id.
        .route("/entities", get(entities::list_entities))
        .route(
            "/entities/merge-suggestions",
            get(entities::list_merge_suggestions),
        )
        .route("/entities/{id}", get(entities::get_entity))
        .route("/entities/{id}/merge", post(entities::merge_entity))
        .route("/entities/{id}/unmerge", post(entities::unmerge_entity))
        .route(
            "/entities/{id}/merge-suggestions/dismiss",
            post(entities::dismiss_merge_suggestion),
        )
        .route("/entities/{id}/aliases", post(entities::add_alias))
        // export / import
        .route("/export", get(export::export_bundle))
        .route(
            "/import",
            post(export::import_bundle).layer(DefaultBodyLimit::disable()),
        )
        // contradiction review
        .route("/contradictions", get(contradictions::list_contradictions))
        .route(
            "/contradictions/explained-away",
            get(safety::list_explained_away),
        )
        .route(
            "/contradictions/explained-away/{id}/confirm",
            post(safety::confirm_explained_away),
        )
        .route(
            "/contradictions/explained-away/{id}/agree",
            post(safety::agree_explained_away),
        )
        .route(
            "/contradictions/{id}",
            get(contradictions::get_contradiction),
        )
        .route(
            "/contradictions/{id}/resolve",
            post(contradictions::resolve_contradiction),
        )
        .route(
            "/contradictions/{id}/annotations",
            post(contradictions::annotate_contradiction),
        )
        // feedback loop — reversible corrections + the optional review tray
        .route("/units/{id}", patch(feedback::edit_unit))
        .route("/units/{id}/confirm", post(feedback::confirm_unit))
        .route("/units/{id}/reject", post(feedback::reject_unit))
        .route("/units/{id}/restore", post(feedback::restore_unit))
        .route("/review", get(feedback::list_review))
        .route("/review/{id}/resolve", post(feedback::resolve_review))
        .route("/review/{id}/accept", post(feedback::accept_review))
        .route("/review/{id}/reject", post(feedback::reject_review))
        // active learning — learned thresholds, their history, and reset
        .route("/tuning", get(tuning::get_tuning))
        .route("/tuning/reset", post(tuning::reset_tuning))
        // clustering — the entity/topic "arrangement" surface
        .route("/clusters", get(clusters::list_clusters))
        .route("/clusters/{id}", get(clusters::get_cluster))
        // photos — thumbnails for duplicate groups, albums and visual topics
        .route("/images/{id}/thumbnail", get(photos::thumbnail))
        .route("/images/{id}/not-duplicate", post(safety::not_duplicate))
        // semantic safety — certificates, reasons, and what a retraction touched
        .route("/certificates", get(safety::list_certificates))
        .route("/certificates/{id}", get(safety::get_certificate))
        .route("/certificates/{id}/chain", get(safety::certificate_chain))
        .route(
            "/certificates/{id}/affected",
            get(safety::certificate_affected),
        )
        .route("/safety/summary", get(safety::safety_summary))
        .route("/units/{id}/support", get(safety::unit_support))
        .route("/units/{id}/revisions", post(safety::unit_revision))
        .route("/status", get(health::status))
        .route("/reread", post(health::start_reread))
        .route("/reread/cancel", post(health::cancel_reread))
        // Layer order: the last .layer() added is outermost (runs first), so
        // auth runs before the rate limiter. That way unauthenticated requests
        // are rejected without charging the shared bucket, and a flood of them
        // can't starve authenticated clients into 429s. When no token is
        // configured the auth layer passes everything through, so the limiter
        // still bounds a runaway local client.
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    let max_body = state.config.max_upload_mb * 1024 * 1024;

    let cors = CorsLayer::new()
        .allow_origin(
            ALLOWED_ORIGINS
                .iter()
                .map(|o| HeaderValue::from_static(o))
                .collect::<Vec<_>>(),
        )
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
        ]);

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(health::metrics))
        .nest("/api/v1", api)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_http_metrics,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .layer(DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn(require_trusted_origin))
        .with_state(state)
}

/// Global request-rate guard for /api/v1. Returns 429 when the shared bucket
/// is empty; a no-op when GATHER_RATE_LIMIT_RPS=0 (limiter absent).
async fn rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, crate::error::ApiError> {
    if let Some(limiter) = &state.rate_limiter {
        if limiter.check().is_err() {
            return Err(crate::error::ApiError::TooManyRequests);
        }
    }
    Ok(next.run(request).await)
}

/// Per-route latency histogram feeding the Grafana dashboard.
async fn record_http_metrics(
    State(_state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let method = request.method().to_string();

    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let elapsed = started.elapsed().as_secs_f64();

    metrics::histogram!(
        "gather_http_request_duration_seconds",
        "method" => method,
        "path" => path,
        "status" => response.status().as_u16().to_string()
    )
    .record(elapsed);

    response
}

/// CORS response headers alone do not prevent a browser from sending a simple
/// cross-origin POST. Reject untrusted origins before a handler can mutate.
async fn require_trusted_origin(request: Request, next: Next) -> Response {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        let origins = request.headers().get_all(axum::http::header::ORIGIN);
        let values: Vec<_> = origins.iter().collect();
        if !values.is_empty()
            && (values.len() != 1
                || !values[0]
                    .to_str()
                    .map(|origin| ALLOWED_ORIGINS.contains(&origin))
                    .unwrap_or(false))
        {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({ "error": {
                    "code": "untrusted_origin",
                    "message": "untrusted browser origin"
                } })),
            )
                .into_response();
        }
    }
    next.run(request).await
}
