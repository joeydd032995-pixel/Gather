//! gRPC IngestService — delegates to the shared ingestion cores in
//! `routes::ingest`, so REST and gRPC persist identically.

use tonic::{Request, Response, Status, Streaming};

use super::convert::artifact_kind_from_pb;
use super::{pb, status_from};
use crate::routes::ingest::{
    agent_log_core, chat_export_core, create_job, finish_job, ingest_one_file, AgentLogRequest,
    ChatExportRequest,
};
use crate::AppState;

pub struct IngestApi {
    pub state: AppState,
}

#[tonic::async_trait]
impl pb::ingest_service_server::IngestService for IngestApi {
    async fn ingest_chat_export(
        &self,
        request: Request<pb::IngestChatExportRequest>,
    ) -> Result<Response<pb::IngestResponse>, Status> {
        let req = request.into_inner();
        let data: serde_json::Value = serde_json::from_slice(&req.data_json)
            .map_err(|e| Status::invalid_argument(format!("data_json is not valid JSON: {e}")))?;
        let core_req = ChatExportRequest {
            platform: req.platform,
            data,
            filename: Some(req.filename).filter(|f| !f.is_empty()),
        };
        let out = chat_export_core(&self.state, core_req, "grpc")
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::IngestResponse {
            job_id: out.job_id.to_string(),
            artifact_id: out.artifact_id.to_string(),
            deduplicated: out.deduplicated,
            conversations: out.conversations as i32,
            messages: out.messages as i32,
        }))
    }

    async fn ingest_agent_log(
        &self,
        request: Request<pb::IngestAgentLogRequest>,
    ) -> Result<Response<pb::IngestResponse>, Status> {
        let req = request.into_inner();
        let core_req = AgentLogRequest {
            platform: req.platform,
            jsonl: req.jsonl,
            session_id: Some(req.session_id).filter(|s| !s.is_empty()),
            title: Some(req.title).filter(|t| !t.is_empty()),
        };
        let out = agent_log_core(&self.state, core_req, "grpc")
            .await
            .map_err(status_from)?;
        Ok(Response::new(pb::IngestResponse {
            job_id: out.job_id.to_string(),
            artifact_id: out.artifact_id.to_string(),
            deduplicated: out.deduplicated,
            conversations: out.conversations as i32,
            messages: out.messages as i32,
        }))
    }

    async fn ingest_file(
        &self,
        request: Request<Streaming<pb::IngestFileChunk>>,
    ) -> Result<Response<pb::IngestFileResponse>, Status> {
        let mut stream = request.into_inner();

        let mut meta: Option<pb::ingest_file_chunk::Meta> = None;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.message().await? {
            match chunk.payload {
                Some(pb::ingest_file_chunk::Payload::Meta(m)) => {
                    if meta.is_some() {
                        return Err(Status::invalid_argument("duplicate meta chunk"));
                    }
                    meta = Some(m);
                }
                Some(pb::ingest_file_chunk::Payload::Data(d)) => {
                    if meta.is_none() {
                        return Err(Status::invalid_argument("first chunk must carry file meta"));
                    }
                    bytes.extend_from_slice(&d);
                }
                None => return Err(Status::invalid_argument("empty chunk")),
            }
        }
        let meta = meta.ok_or_else(|| Status::invalid_argument("stream carried no meta"))?;
        if bytes.is_empty() {
            return Err(Status::invalid_argument("stream carried no file data"));
        }

        // Explicit kind override maps to the multipart part-name contract.
        let part_name = pb::ArtifactKind::try_from(meta.kind_override)
            .ok()
            .and_then(artifact_kind_from_pb)
            .unwrap_or("file");
        let filename = if meta.filename.is_empty() {
            "unnamed".to_string()
        } else {
            meta.filename
        };
        let media_type = Some(meta.media_type).filter(|m| !m.is_empty());

        let job_id = create_job(&self.state.pool, "grpc")
            .await
            .map_err(status_from)?;
        let result = ingest_one_file(
            &self.state,
            job_id,
            part_name,
            &filename,
            media_type,
            &bytes,
        )
        .await;
        match result {
            Ok(file) => {
                metrics::counter!(
                    "gather_ingest_files_total",
                    "kind" => file.kind.clone().unwrap_or_else(|| "unknown".into()),
                    "status" => file.status.clone()
                )
                .increment(1);
                finish_job(
                    &self.state.pool,
                    job_id,
                    true,
                    serde_json::json!({"files": 1, "accepted": 1, "rejected": 0}),
                )
                .await
                .map_err(status_from)?;
                Ok(Response::new(pb::IngestFileResponse {
                    job_id: job_id.to_string(),
                    artifact_id: file
                        .artifact_id
                        .map(|id| id.to_string())
                        .unwrap_or_default(),
                    kind: super::convert::artifact_kind_to_pb(file.kind.as_deref().unwrap_or(""))
                        as i32,
                    deduplicated: file.deduplicated,
                    segments: file.segments as i32,
                }))
            }
            Err(e) => {
                metrics::counter!(
                    "gather_ingest_files_total",
                    "kind" => "unknown",
                    "status" => "rejected"
                )
                .increment(1);
                finish_job(
                    &self.state.pool,
                    job_id,
                    false,
                    serde_json::json!({"files": 1, "accepted": 0, "rejected": 1}),
                )
                .await
                .map_err(status_from)?;
                Err(status_from(e))
            }
        }
    }
}
