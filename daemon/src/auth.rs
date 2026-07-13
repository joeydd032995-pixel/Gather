use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::ApiError;
use crate::AppState;

/// Bearer-token guard for /api/v1.
///
/// The desktop flow is OS-user bound: the daemon generates a random token at
/// first run, stores it in the OS keychain under the current OS user, and the
/// Tauri UI reads it from the same keychain — so only processes running as
/// that OS user can call the API. The daemon side of that contract is this
/// middleware plus the GATHER_API_TOKEN environment variable (injected by the
/// supervisor from the keychain). When no token is configured (e.g. local
/// docker-compose development), the API is open — but only ever on loopback.
pub async fn require_bearer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(expected) = &state.config.api_token {
        let presented = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match presented {
            Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {}
            _ => return Err(ApiError::Unauthorized),
        }
    }
    Ok(next.run(request).await)
}

/// Constant-time comparison to keep the token check timing-safe.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret1"));
        assert!(constant_time_eq(b"", b""));
    }
}
