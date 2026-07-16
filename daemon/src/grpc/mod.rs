//! Local gRPC API (write-up §4): tonic server on a second loopback listener,
//! mirroring the REST surface 1:1 per `proto/gather/v1/gather.proto`.
//! Nontrivial logic is shared with REST (ingestion cores, contradiction
//! resolution, bundle build/import, search core); simple reads issue the
//! same SQL and map straight into proto types.

// tonic mandates `Result<_, Status>` (~176 bytes) throughout its API; boxing
// the error in our helpers would just add conversion noise at every call site.
#![allow(clippy::result_large_err)]

pub mod auth;
pub mod contradictions;
pub mod convert;
pub mod export;
pub mod ingest;
pub mod query;

/// Generated protobuf/tonic types for `package gather.v1`.
pub mod pb {
    tonic::include_proto!("gather.v1");
}

use tonic::transport::Server;

use crate::error::ApiError;
use crate::AppState;

use pb::contradiction_service_server::ContradictionServiceServer;
use pb::export_service_server::ExportServiceServer;
use pb::ingest_service_server::IngestServiceServer;
use pb::query_service_server::QueryServiceServer;

/// Map the shared ApiError onto gRPC status codes.
pub(crate) fn status_from(error: ApiError) -> tonic::Status {
    match &error {
        ApiError::BadRequest(m) | ApiError::UnsupportedMedia(m) => {
            tonic::Status::invalid_argument(m.clone())
        }
        ApiError::NotFound(m) => tonic::Status::not_found(m.clone()),
        ApiError::Unauthorized => tonic::Status::unauthenticated("missing or invalid token"),
        ApiError::Db(e) => {
            tracing::error!(error = %e, "grpc database error");
            tonic::Status::internal("database error")
        }
        ApiError::Internal(e) => {
            tracing::error!(error = %e, "grpc internal error");
            tonic::Status::internal("internal error")
        }
    }
}

/// Serve the gRPC API until shutdown. Spawned from main when
/// GATHER_GRPC_ENABLED; the bind address obeys the same loopback-only policy
/// as the HTTP listener (enforced in Config::from_env).
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = state.config.grpc_bind_addr;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        auth = state.config.api_token.is_some(),
        "gRPC listening"
    );
    serve_on(state, listener).await
}

/// Serve on an already-bound listener; tests bind 127.0.0.1:0 and read
/// `local_addr()` before handing the listener over.
pub async fn serve_on(state: AppState, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let interceptor = auth::BearerInterceptor::new(state.config.api_token.clone());

    Server::builder()
        .add_service(IngestServiceServer::with_interceptor(
            ingest::IngestApi {
                state: state.clone(),
            },
            interceptor.clone(),
        ))
        .add_service(QueryServiceServer::with_interceptor(
            query::QueryApi {
                state: state.clone(),
            },
            interceptor.clone(),
        ))
        .add_service(ContradictionServiceServer::with_interceptor(
            contradictions::ContradictionApi {
                state: state.clone(),
            },
            interceptor.clone(),
        ))
        .add_service(ExportServiceServer::with_interceptor(
            export::ExportApi { state },
            interceptor,
        ))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            crate::shutdown_signal(),
        )
        .await?;
    Ok(())
}
