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

/// Prometheus exposition endpoint.
pub async fn metrics(State(state): State<AppState>) -> String {
    state.metrics.render()
}
