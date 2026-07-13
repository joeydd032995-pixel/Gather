use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Unified API error type. Everything a handler can fail with maps to a
/// stable JSON error envelope: {"error": {"code": "...", "message": "..."}}.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported media type: {0}")]
    UnsupportedMedia(String),
    #[error("database error")]
    Db(#[from] sqlx::Error),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::UnsupportedMedia(_) => {
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type")
            }
            ApiError::Db(_) => (StatusCode::INTERNAL_SERVER_ERROR, "database_error"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        // Never leak internal error details to the client; log them instead.
        let message = match &self {
            ApiError::Db(e) => {
                tracing::error!(error = %e, "database error");
                "database error".to_string()
            }
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        };
        let body = Json(json!({ "error": { "code": code, "message": message } }));
        (status, body).into_response()
    }
}
