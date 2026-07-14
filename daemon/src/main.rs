use std::sync::Arc;

use gather_daemon::config::Config;
use gather_daemon::extract::ollama::OllamaClient;
use gather_daemon::{db, routes, AppState};
use metrics_exporter_prometheus::PrometheusBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = Config::from_env()?;
    gather_daemon::init_tracing(config.log_json);
    gather_daemon::auth_token::resolve(&mut config);

    let metrics_handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Suffix("duration_seconds".to_string()),
            &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.25, 0.5, 1.0, 2.5, 5.0,
            ],
        )?
        .install_recorder()?;
    gather_daemon::describe_metrics();

    tracing::info!(
        bind = %config.bind_addr,
        grpc_bind = %config.grpc_bind_addr,
        grpc_enabled = config.grpc_enabled,
        auth = config.api_token.is_some(),
        "starting gather-daemon (offline-by-default: no outbound connections)"
    );

    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;
    tracing::info!("database connected, migrations applied");

    // Build shared Ollama client once at startup for server-side query embedding.
    // Extraction and scan workers keep constructing their own (no behavior change).
    let ollama_client: Option<Arc<OllamaClient>> = match OllamaClient::from_config(&config) {
        Ok(Some(c)) => {
            tracing::info!("Ollama client initialised for server-side query embedding");
            Some(Arc::new(c))
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "Ollama client disabled");
            None
        }
    };

    let state = AppState {
        pool: pool.clone(),
        config: Arc::new(config.clone()),
        metrics: metrics_handle,
        ollama: ollama_client.clone(),
    };

    tokio::spawn(gather_daemon::gauge_refresher(pool.clone()));
    if config.extraction_enabled {
        tokio::spawn(gather_daemon::extract::worker_loop(
            pool.clone(),
            config.clone(),
        ));
    } else {
        tracing::info!("extraction worker disabled via GATHER_EXTRACTION_ENABLED=false");
    }
    if config.scan_enabled {
        tokio::spawn(gather_daemon::scan::worker_loop(
            pool.clone(),
            config.clone(),
        ));
    } else {
        tracing::info!("contradiction scanner disabled via GATHER_SCAN_ENABLED=false");
    }

    if config.grpc_enabled {
        let grpc_pool = pool.clone();
        let grpc_cfg = Arc::new(config.clone());
        let grpc_ollama = ollama_client.clone();
        tokio::spawn(async move {
            if let Err(e) =
                gather_daemon::grpc::serve(grpc_pool, grpc_cfg, grpc_ollama).await
            {
                tracing::error!(error = %e, "gRPC server failed");
            }
        });
    } else {
        tracing::info!("gRPC server disabled via GATHER_GRPC_ENABLED=false");
    }

    let app = routes::build_router(state);
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "HTTP listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(gather_daemon::shutdown_signal())
        .await?;

    // Drain the pool so Postgres sees clean disconnects.
    pool.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}
