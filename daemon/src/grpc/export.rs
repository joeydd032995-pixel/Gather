//! gRPC ExportService — streams the same gather-bundle-v1 NDJSON the REST
//! endpoints produce/accept, in 64 KiB chunks.

use std::pin::Pin;

use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use super::{pb, status_from};
use crate::routes::export::{
    bundle_stream, import_bundle_file, private_bundle_file, write_bundle_chunk, BundleLimit,
};
use crate::AppState;
use tokio::io::AsyncWriteExt;

pub struct ExportApi {
    pub state: AppState,
}

#[tonic::async_trait]
impl pb::export_service_server::ExportService for ExportApi {
    type ExportBundleStream =
        Pin<Box<dyn Stream<Item = Result<pb::BundleChunk, Status>> + Send + 'static>>;

    async fn export_bundle(
        &self,
        _request: Request<pb::ExportBundleRequest>,
    ) -> Result<Response<Self::ExportBundleStream>, Status> {
        let stream = bundle_stream(&self.state.pool).await.map_err(status_from)?;
        Ok(Response::new(Box::pin(stream.map(|chunk| {
            chunk
                .map(|data| pb::BundleChunk { data })
                .map_err(status_from)
        }))))
    }

    async fn import_bundle(
        &self,
        request: Request<Streaming<pb::BundleChunk>>,
    ) -> Result<Response<pb::ImportBundleResponse>, Status> {
        let mut stream = request.into_inner();
        let temporary = private_bundle_file().map_err(status_from)?;
        let mut file = tokio::fs::File::from_std(
            temporary
                .reopen()
                .map_err(|error| Status::internal(error.to_string()))?,
        );
        let mut limit = BundleLimit::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            write_bundle_chunk(&mut file, &mut limit, &chunk.data)
                .await
                .map_err(status_from)?;
        }
        file.flush()
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        let counts = import_bundle_file(&self.state.pool, &temporary)
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
