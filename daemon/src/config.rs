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
    /// Explicit opt-out of the loopback-only policy (bind address and
    /// Ollama URL checks). Containers set this; desktops should not.
    pub allow_non_loopback: bool,
    /// Run the background extraction worker (PDF/OCR/atomic units).
    pub extraction_enabled: bool,
    /// Seconds between extraction passes.
    pub extraction_interval_secs: u64,
    /// Max rows claimed per queue per pass.
    pub extraction_batch: i64,
    /// Tesseract CLI binary (name on PATH or absolute path).
    pub tesseract_path: String,
    /// Ollama base URL; None/empty disables all LLM/embedding features.
    pub ollama_url: Option<String>,
    /// Chat model for LLM-assisted extraction.
    pub ollama_model: String,
    /// Embedding model (must produce 768-dim vectors to match the schema).
    pub ollama_embed_model: String,
    /// Run the background contradiction scanner.
    pub scan_enabled: bool,
    /// Seconds between scan passes.
    pub scan_interval_secs: u64,
    /// Max unscanned units claimed per pass.
    pub scan_batch: i64,
    /// Minimum score for a pair to be recorded as a contradiction.
    pub scan_threshold: f32,
    /// Max candidates per blocking strategy per unit.
    pub scan_max_candidates: i64,
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

        let env_bool = |name: &str, default: bool| {
            std::env::var(name)
                .map(|v| v == "true" || v == "1")
                .unwrap_or(default)
        };

        Ok(Self {
            bind_addr,
            database_url,
            api_token,
            max_upload_mb,
            log_json,
            allow_non_loopback,
            extraction_enabled: env_bool("GATHER_EXTRACTION_ENABLED", true),
            extraction_interval_secs: std::env::var("GATHER_EXTRACTION_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.max(1))
                .unwrap_or(30),
            extraction_batch: std::env::var("GATHER_EXTRACTION_BATCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: i64| v.clamp(1, 256))
                .unwrap_or(8),
            tesseract_path: std::env::var("GATHER_TESSERACT_PATH")
                .unwrap_or_else(|_| "tesseract".to_string()),
            ollama_url: std::env::var("GATHER_OLLAMA_URL")
                .ok()
                .filter(|u| !u.is_empty()),
            ollama_model: std::env::var("GATHER_OLLAMA_MODEL")
                .unwrap_or_else(|_| "llama3.2:3b".to_string()),
            ollama_embed_model: std::env::var("GATHER_OLLAMA_EMBED_MODEL")
                .unwrap_or_else(|_| "nomic-embed-text".to_string()),
            scan_enabled: env_bool("GATHER_SCAN_ENABLED", true),
            scan_interval_secs: std::env::var("GATHER_SCAN_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.max(1))
                .unwrap_or(600),
            scan_batch: std::env::var("GATHER_SCAN_BATCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: i64| v.clamp(1, 512))
                .unwrap_or(32),
            scan_threshold: std::env::var("GATHER_SCAN_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: f32| v.clamp(0.0, 1.0))
                .unwrap_or(0.65),
            scan_max_candidates: std::env::var("GATHER_SCAN_MAX_CANDIDATES")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: i64| v.clamp(1, 200))
                .unwrap_or(25),
        })
    }

    /// Baseline config for tests: loopback bind, auth off, extraction knobs
    /// at defaults, Ollama disabled.
    pub fn for_tests(database_url: String) -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().expect("static addr"),
            database_url,
            api_token: None,
            max_upload_mb: 16,
            log_json: false,
            allow_non_loopback: false,
            extraction_enabled: true,
            extraction_interval_secs: 30,
            extraction_batch: 8,
            tesseract_path: "tesseract".to_string(),
            ollama_url: None,
            ollama_model: "llama3.2:3b".to_string(),
            ollama_embed_model: "nomic-embed-text".to_string(),
            scan_enabled: true,
            scan_interval_secs: 600,
            scan_batch: 32,
            scan_threshold: 0.65,
            scan_max_candidates: 25,
        }
    }
}
