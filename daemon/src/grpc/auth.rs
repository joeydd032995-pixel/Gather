//! Bearer-token interceptor: same token and constant-time comparison as the
//! REST middleware; only enforced when a token is configured. It also applies
//! the shared global rate limiter, so the gRPC surface honours the same
//! GATHER_RATE_LIMIT_RPS budget as REST.

use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::auth::constant_time_eq;
use crate::SharedRateLimiter;

#[derive(Clone)]
pub struct BearerInterceptor {
    expected: Option<String>,
    rate_limiter: Option<SharedRateLimiter>,
}

impl BearerInterceptor {
    pub fn new(expected: Option<String>, rate_limiter: Option<SharedRateLimiter>) -> Self {
        Self {
            expected,
            rate_limiter,
        }
    }
}

impl Interceptor for BearerInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        // Authenticate before the rate limiter so unauthenticated requests are
        // rejected without charging the shared bucket (mirrors the REST layer
        // order). When no token is configured, auth passes and the limiter
        // still bounds a runaway local client.
        if let Some(expected) = &self.expected {
            let presented = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match presented {
                Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {}
                _ => return Err(Status::unauthenticated("missing or invalid bearer token")),
            }
        }
        if let Some(limiter) = &self.rate_limiter {
            if limiter.check().is_err() {
                return Err(Status::resource_exhausted("rate limit exceeded"));
            }
        }
        Ok(request)
    }
}
