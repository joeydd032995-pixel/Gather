use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

/// Connect to Postgres with bounded retries so `docker compose up` ordering
/// races (daemon ready before Postgres finishes initdb) resolve themselves.
pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let options: PgConnectOptions = database_url
        .parse::<PgConnectOptions>()?
        .log_statements(tracing::log::LevelFilter::Debug);

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match PgPoolOptions::new()
            .max_connections(8)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options.clone())
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) if attempt < 10 => {
                let backoff = Duration::from_millis(500 * u64::from(attempt));
                tracing::warn!(
                    attempt,
                    error = %e,
                    backoff_ms = backoff.as_millis(),
                    "database not ready, retrying"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Apply embedded migrations (daemon/migrations/*.sql). Idempotent.
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// Readiness probe: round-trip the database and confirm pgvector is loaded.
pub async fn readiness_check(pool: &PgPool) -> anyhow::Result<()> {
    let (vector_loaded,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(pool)
            .await?;
    anyhow::ensure!(vector_loaded, "pgvector extension is not installed");
    Ok(())
}
