//! gRPC ExportService — streams the same gather-bundle-v1 NDJSON the REST
//! endpoints produce/accept, in 64 KiB chunks.

use std::pin::Pin;

use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use super::{pb, status_from};
use crate::routes::export::{build_bundle, import_bundle_core};
use crate::AppState;

pub struct ExportApi {
    pub state: AppState,
}

const CHUNK_BYTES: usize = 64 * 1024;

#[tonic::async_trait]
impl pb::export_service_server::ExportService for ExportApi {
    type ExportBundleStream =
        Pin<Box<dyn Stream<Item = Result<pb::BundleChunk, Status>> + Send + 'static>>;

    async fn export_bundle(
        &self,
        _request: Request<pb::ExportBundleRequest>,
    ) -> Result<Response<Self::ExportBundleStream>, Status> {
        let bundle = build_bundle(&self.state.pool).await.map_err(status_from)?;
        let bytes = bundle.into_bytes();
        let chunks: Vec<Result<pb::BundleChunk, Status>> = bytes
            .chunks(CHUNK_BYTES)
            .map(|c| Ok(pb::BundleChunk { data: c.to_vec() }))
            .collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn import_bundle(
        &self,
        request: Request<Streaming<pb::BundleChunk>>,
    ) -> Result<Response<pb::ImportBundleResponse>, Status> {
        let mut stream = request.into_inner();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?.data);
        }
        let body = String::from_utf8(bytes)
            .map_err(|_| Status::invalid_argument("bundle is not valid UTF-8"))?;
        let counts = import_bundle_core(&self.state.pool, &body)
            .await
            .map_err(status_from)?;

        Ok(Response::new(pb::ImportBundleResponse {
            tables: counts
                .iter()
                .map(|(table, v)| pb::import_bundle_response::TableCount {
                    table: table.clone(),
                    in_bundle: v.get("in_bundle").and_then(|n| n.as_i64()).unwrap_or(0),
                    inserted: v.get("inserted").and_then(|n| n.as_i64()).unwrap_or(0),
                })
                .collect(),
        }))
    }
}
