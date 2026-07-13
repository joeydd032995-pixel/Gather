pub mod adapters;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod routes;

use std::sync::Arc;
use std::time::Duration;

use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub metrics: PrometheusHandle,
}

pub fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_http=info"));
    let fmt_layer = if json {
        tracing_subscriber::fmt::layer().json().boxed()
    } else {
        tracing_subscriber::fmt::layer().boxed()
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}

pub fn describe_metrics() {
    metrics::describe_counter!(
        "gather_ingest_files_total",
        "Uploaded files by artifact kind and outcome (accepted/deduplicated/rejected)"
    );
    metrics::describe_counter!(
        "gather_ingest_artifacts_total",
        "New (non-deduplicated) artifacts stored, by kind"
    );
    metrics::describe_counter!(
        "gather_ingest_messages_total",
        "Normalized messages ingested, by source platform"
    );
    metrics::describe_counter!(
        "gather_extraction_segments_total",
        "Document segments produced, by extraction tool"
    );
    metrics::describe_counter!(
        "gather_extraction_units_total",
        "Atomic units produced by the extraction pipeline, by method and status"
    );
    metrics::describe_counter!(
        "gather_contradictions_resolved_total",
        "Contradictions resolved, by resolution"
    );
    metrics::describe_gauge!(
        "gather_contradictions_open",
        "Contradictions currently awaiting review"
    );
    metrics::describe_gauge!(
        "gather_extraction_backlog",
        "Documents/images still waiting for extraction, by modality"
    );
    metrics::describe_histogram!(
        "gather_graph_query_duration_seconds",
        "Knowledge-graph traversal latency"
    );
    metrics::describe_histogram!(
        "gather_http_request_duration_seconds",
        "HTTP request latency by route"
    );
}

/// Background refresher for gauges that mirror database state.
pub async fn gauge_refresher(pool: PgPool) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        if let Ok((open,)) =
            sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM contradictions WHERE status = 'open'")
                .fetch_one(&pool)
                .await
        {
            metrics::gauge!("gather_contradictions_open").set(open as f64);
        }
        if let Ok((docs,)) = sqlx::query_as::<_, (i64,)>(
            "SELECT count(*) FROM documents WHERE extraction_status IN ('pending','processing')",
        )
        .fetch_one(&pool)
        .await
        {
            metrics::gauge!("gather_extraction_backlog", "modality" => "document").set(docs as f64);
        }
        if let Ok((imgs,)) = sqlx::query_as::<_, (i64,)>(
            "SELECT count(*) FROM images WHERE ocr_status IN ('pending','processing')",
        )
        .fetch_one(&pool)
        .await
        {
            metrics::gauge!("gather_extraction_backlog", "modality" => "image").set(imgs as f64);
        }
    }
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
