//! Minimal gRPC smoke client for a running daemon — the gRPC analogue of
//! `curl http://127.0.0.1:7601/healthz` for downstream-agent authors.
//!
//! Usage:
//!   cargo run --example grpc_smoke
//!   GATHER_GRPC_ADDR=http://127.0.0.1:7602 GATHER_API_TOKEN=... \
//!     cargo run --example grpc_smoke

// tonic interceptors must return Result<_, tonic::Status> (~176 bytes).
#![allow(clippy::result_large_err)]

use gather_daemon::grpc::pb;
use pb::export_service_client::ExportServiceClient;
use pb::query_service_client::QueryServiceClient;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr =
        std::env::var("GATHER_GRPC_ADDR").unwrap_or_else(|_| "http://127.0.0.1:7602".to_string());
    let token = std::env::var("GATHER_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    let channel = Channel::from_shared(addr.clone())?.connect().await?;
    let bearer: Option<MetadataValue<_>> = match token {
        Some(t) => Some(format!("Bearer {t}").parse()?),
        None => None,
    };
    let auth = move |mut req: Request<()>| {
        if let Some(bearer) = &bearer {
            req.metadata_mut().insert("authorization", bearer.clone());
        }
        Ok(req)
    };

    let mut query = QueryServiceClient::with_interceptor(channel.clone(), auth.clone());
    let artifacts = query
        .list_artifacts(pb::ListArtifactsRequest {
            limit: 5,
            ..Default::default()
        })
        .await?
        .into_inner();
    println!("gRPC OK at {addr}");
    println!("latest artifacts ({} shown):", artifacts.items.len());
    for a in &artifacts.items {
        println!(
            "  {}  kind={:?}  platform={}  {} bytes",
            a.id,
            pb::ArtifactKind::try_from(a.kind).unwrap_or_default(),
            a.source_platform,
            a.byte_size
        );
    }

    let mut export = ExportServiceClient::with_interceptor(channel, auth);
    let mut stream = export
        .export_bundle(pb::ExportBundleRequest {})
        .await?
        .into_inner();
    let mut bytes = 0usize;
    while let Some(chunk) = stream.message().await? {
        bytes += chunk.data.len();
    }
    println!("export bundle streamed: {bytes} bytes");
    Ok(())
}
