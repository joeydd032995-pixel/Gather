use std::sync::Arc;

use gather_daemon::auth_token::{OsKeychain, TokenStore};
use gather_daemon::config::Config;
use gather_daemon::extract::ollama::OllamaClient;
use gather_daemon::{db, routes, AppState};
use metrics_exporter_prometheus::PrometheusBuilder;

/// `gather-daemon print-api-token`: read the OS-keychain bearer token
/// without needing DATABASE_URL or any other daemon config. This is the
/// same keychain entry `auth_token::resolve` and the Tauri app's
/// `get_api_token` command already read (§7.2) — scheduled-backup scripts
/// call this instead of reimplementing native keychain reads per OS.
fn print_api_token() -> anyhow::Result<()> {
    match OsKeychain.get() {
        Ok(Some(token)) => {
            println!("{token}");
            Ok(())
        }
        Ok(None) => {
            eprintln!(
                "no token found in the OS keychain (the daemon creates one on first run \
                 under GATHER_AUTH_MODE=keychain; in env mode there is no keychain token — \
                 use GATHER_API_TOKEN directly)"
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("keychain read failed: {e}");
            std::process::exit(1);
        }
    }
}

/// glibc raises its mmap threshold (up to 32 MB) after large blocks are freed,
/// after which big buffers — an uploaded file, a photo — come from the heap
/// and are kept after being freed. Pinning the threshold keeps every
/// allocation over 1 MB mmap-backed, so it goes back to the OS when freed and
/// the daemon's memory falls back after a large file (Gather targets 4 GB
/// machines). Windows' and macOS' allocators already behave this way.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn bound_heap_retention() {
    const THRESHOLD: libc::c_int = 1 << 20;
    // SAFETY: mallopt only adjusts allocator parameters; called once at
    // startup, before any other threads exist.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, THRESHOLD);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn bound_heap_retention() {}

fn main() -> anyhow::Result<()> {
    bound_heap_retention();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("print-api-token") {
        return print_api_token();
    }

    let mut config = Config::from_env()?;
    gather_daemon::init_tracing(config.log_json);
    gather_daemon::auth_token::resolve(&mut config).map_err(|e| anyhow::anyhow!(e))?;

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

    // Surface the auth posture loudly and as a scrapeable gauge. The API stays
    // open on loopback when no token is configured (docker-compose dev default),
    // but that fact must be impossible to miss and observable in Grafana.
    if config.api_token.is_some() {
        metrics::gauge!("gather_api_auth_enabled").set(1.0);
    } else {
        metrics::gauge!("gather_api_auth_enabled").set(0.0);
        // Whether the open API is genuinely loopback-scoped depends on the
        // actual bind addresses, which GATHER_ALLOW_NON_LOOPBACK can widen
        // (the container image sets it, so a published 0.0.0.0 port is
        // possible). Only claim "loopback-only" when it's actually true.
        let grpc_non_loopback = config.grpc_enabled && !config.grpc_bind_addr.ip().is_loopback();
        if !config.bind_addr.ip().is_loopback() || grpc_non_loopback {
            tracing::warn!(
                http_bind = %config.bind_addr,
                grpc_bind = %config.grpc_bind_addr,
                grpc_enabled = config.grpc_enabled,
                "/api/v1 IS SERVING UNAUTHENTICATED ON A NON-LOOPBACK ADDRESS — anything that can \
                 reach the bind address (limited only by how the port is published) has full API \
                 access. Set GATHER_API_TOKEN (or GATHER_AUTH_MODE=keychain) to enforce bearer \
                 auth, or bind a loopback address."
            );
        } else {
            tracing::warn!(
                "/api/v1 IS SERVING UNAUTHENTICATED — any process that can reach the loopback \
                 port has full API access. This is safe only because the listener is loopback-only. \
                 Set GATHER_API_TOKEN (or GATHER_AUTH_MODE=keychain) to enforce bearer auth."
            );
        }
    }

    tracing::info!(
        memory_profile = config.memory_profile.as_str(),
        db_connections = config.db_max_connections,
        max_upload_mb = config.max_upload_mb,
        "memory profile"
    );
    let pool = db::connect_with_max(&config.database_url, config.db_max_connections).await?;
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
        ollama: ollama_client,
        rate_limiter: gather_daemon::build_rate_limiter(config.rate_limit_rps),
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
    if config.cluster_enabled {
        tokio::spawn(gather_daemon::cluster::worker::worker_loop(
            pool.clone(),
            config.clone(),
        ));
    } else {
        tracing::info!("clustering worker disabled via GATHER_CLUSTER_ENABLED=false");
    }
    // Always runs: it re-ranks the review tray. GATHER_TUNE_ENABLED gates only
    // whether it may move thresholds.
    tokio::spawn(gather_daemon::tune::worker::worker_loop(
        pool.clone(),
        config.clone(),
    ));
    if config.photo_enabled {
        tokio::spawn(gather_daemon::photo::worker::worker_loop(
            pool.clone(),
            config.clone(),
        ));
    } else {
        tracing::info!("photo worker disabled via GATHER_PHOTO_ENABLED=false");
    }
    if !config.tune_enabled {
        tracing::info!("threshold auto-tuning disabled via GATHER_TUNE_ENABLED=false");
    }

    if config.grpc_enabled {
        let grpc_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = gather_daemon::grpc::serve(grpc_state).await {
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
