//! Bearer-token interceptor: same token and constant-time comparison as the
//! REST middleware; only enforced when a token is configured.

use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::auth::constant_time_eq;

#[derive(Clone)]
pub struct BearerInterceptor {
    expected: Option<String>,
}

impl BearerInterceptor {
    pub fn new(expected: Option<String>) -> Self {
        Self { expected }
    }
}

impl Interceptor for BearerInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
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
        Ok(request)
    }
}
