use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// Liveness: the process is up and serving.
pub async fn healthz() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "gather-daemon",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Readiness: the database is reachable and pgvector is installed.
pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match crate::db::readiness_check(&state.pool).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ready" }))),
        Err(e) => {
            tracing::warn!(error = %e, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable", "reason": e.to_string() })),
            )
        }
    }
}

/// What the daemon is running with and how far reading has got, for the
/// app's Settings page: whether a local AI model is set up, and how much is
/// still queued.
pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, crate::error::ApiError> {
    let reading = crate::extract::backlog(&state.pool).await?;
    let c = &state.config;
    // Models only when Ollama is in use: configured defaults mean nothing
    // without it.
    let on = state.ollama.is_some();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "ai": {
            "enabled": on,
            "url": c.ollama_url.as_ref().filter(|_| on),
            "model": c.ollama_model.as_ref().filter(|_| on),
            "embed_model": on.then(|| c.ollama_embed_model.clone()),
        },
        "reading": reading,
    })))
}

/// Prometheus exposition endpoint.
pub async fn metrics(State(state): State<AppState>) -> String {
    state.metrics.render()
}
