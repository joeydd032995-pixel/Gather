use std::sync::Arc;

use gather_daemon::config::Config;
use gather_daemon::{db, routes, AppState};
use metrics_exporter_prometheus::PrometheusBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    gather_daemon::init_tracing(config.log_json);

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
        auth = config.api_token.is_some(),
        "starting gather-daemon (offline-by-default: no outbound connections)"
    );

    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;
    tracing::info!("database connected, migrations applied");

    let state = AppState {
        pool: pool.clone(),
        config: Arc::new(config.clone()),
        metrics: metrics_handle,
    };

    tokio::spawn(gather_daemon::gauge_refresher(pool.clone()));

    let app = routes::build_router(state);
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(gather_daemon::shutdown_signal())
        .await?;

    // Drain the pool so Postgres sees clean disconnects.
    pool.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}
