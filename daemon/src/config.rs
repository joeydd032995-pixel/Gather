use std::net::SocketAddr;

/// Runtime configuration, sourced exclusively from environment variables.
///
/// Offline-by-default: the daemon itself never initiates outbound network
/// connections. The only sockets it opens are the loopback listener and the
/// Postgres connection. Optional integrations (Ollama, VPS sync) are separate
/// processes and are opt-in via their own tooling.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address to bind. Defaults to loopback; binding a non-loopback address
    /// requires GATHER_ALLOW_NON_LOOPBACK=true as an explicit override.
    pub bind_addr: SocketAddr,
    pub database_url: String,
    /// Optional bearer token. When set, every /api/v1 request must carry
    /// `Authorization: Bearer <token>`. Health and metrics stay open (loopback only).
    pub api_token: Option<String>,
    /// Upload cap per request body, in megabytes.
    pub max_upload_mb: usize,
    /// Emit JSON logs instead of human-readable ones.
    pub log_json: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("DATABASE_URL must be set")]
    MissingDatabaseUrl,
    #[error("GATHER_BIND_ADDR is not a valid socket address: {0}")]
    BadBindAddr(String),
    #[error(
        "refusing to bind non-loopback address {0} without GATHER_ALLOW_NON_LOOPBACK=true \
         (Gather is offline/local-only by default)"
    )]
    NonLoopbackBind(SocketAddr),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw =
            std::env::var("GATHER_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:7601".to_string());
        let bind_addr: SocketAddr = bind_raw
            .parse()
            .map_err(|_| ConfigError::BadBindAddr(bind_raw.clone()))?;

        let allow_non_loopback = std::env::var("GATHER_ALLOW_NON_LOOPBACK")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        if !bind_addr.ip().is_loopback() && !allow_non_loopback {
            return Err(ConfigError::NonLoopbackBind(bind_addr));
        }

        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| ConfigError::MissingDatabaseUrl)?;

        let api_token = std::env::var("GATHER_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        let max_upload_mb = std::env::var("GATHER_MAX_UPLOAD_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        let log_json = std::env::var("GATHER_LOG_JSON")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        Ok(Self {
            bind_addr,
            database_url,
            api_token,
            max_upload_mb,
            log_json,
        })
    }
}
