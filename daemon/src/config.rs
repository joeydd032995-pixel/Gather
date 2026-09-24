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
    /// Max Postgres connections in the shared pool. Sized for concurrent
    /// REST/gRPC handlers plus the extraction and scan workers.
    pub db_max_connections: u32,
    /// Optional bearer token. When set, every /api/v1 request must carry
    /// `Authorization: Bearer <token>`. Health and metrics stay open (loopback only).
    pub api_token: Option<String>,
    /// Token source: "env" (GATHER_API_TOKEN or open loopback) or "keychain"
    /// (OS-keychain get-or-create; the packaged desktop default).
    pub auth_mode: String,
    /// Upload cap per request body, in megabytes.
    pub max_upload_mb: usize,
    /// Requests/sec allowed across /api/v1 and the gRPC services combined
    /// (a shared global bucket). 0 disables rate limiting. Bounds a runaway
    /// local client; the listener is loopback-only regardless.
    pub rate_limit_rps: u32,
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
    /// Local vision model for photo captions (e.g. `moondream`, `llava`).
    /// None disables captions and photo topics; requires `ollama_url`.
    pub ollama_vision_model: Option<String>,
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
    /// Confidence at/above which a freshly extracted unit is auto-admitted
    /// silently. In the band below it (but at/above `admit_drop_below`) the unit
    /// is still admitted, then parked in `review_queue` for optional review.
    /// Default 0.5.
    pub admit_hold_below: f32,
    /// Confidence below which a unit is retracted on ingest. Default 0.0 — off,
    /// so nothing is dropped unless explicitly configured (conservative).
    pub admit_drop_below: f32,
    /// Run the background clustering worker (entity auto-resolution + topic
    /// grouping).
    pub cluster_enabled: bool,
    /// Seconds between clustering passes.
    pub cluster_interval_secs: u64,
    /// Max unclustered units claimed per topic-clustering pass.
    pub cluster_batch: i64,
    /// k for the mutual-kNN graph.
    pub cluster_k: usize,
    /// Minimum similarity for a mutual-kNN edge (topic grouping / entity graph).
    pub cluster_threshold: f32,
    /// Components larger than this are treated as too diffuse to auto-label and
    /// are skipped (chaining guard).
    pub cluster_max_component: usize,
    /// Let the tuner move decision thresholds from user feedback. The tune
    /// worker always runs (it also re-ranks the review tray); this gates only
    /// the threshold changes.
    pub tune_enabled: bool,
    /// Seconds between tray re-ranking / tuning passes.
    pub tune_interval_secs: u64,
    /// Labels needed at/above a threshold before the tuner may move it.
    pub tune_min_samples: usize,
    /// Precision the auto-accepted band must hold; the tuner raises a
    /// threshold below it and lowers one only when a 95% bound clears it.
    pub tune_target_precision: f32,
    /// Run the photo worker (hashing, duplicate groups, albums, captions).
    pub photo_enabled: bool,
    /// Seconds between photo passes.
    pub photo_interval_secs: u64,
    /// Images hashed (and, with a vision model, captioned) per pass.
    pub photo_batch: i64,
    /// Max differing pHash bits for two photos to count as near-duplicates.
    pub photo_dup_max_distance: u32,
    /// A gap longer than this between shots starts a new album.
    pub photo_album_gap_hours: i64,
    /// Consecutive located shots further apart than this start a new album.
    pub photo_album_split_km: f64,
    /// Smallest group of photos that becomes an album.
    pub photo_album_min_size: usize,
    /// Minimum caption-embedding cosine for two photos to share a topic.
    pub photo_topic_threshold: f32,
    /// Enable the gRPC server (default true).
    pub grpc_enabled: bool,
    /// Address to bind the gRPC listener. Same loopback policy as bind_addr.
    pub grpc_bind_addr: SocketAddr,
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
    #[error("{var} has an invalid value {value:?}: {reason}")]
    BadEnvValue {
        var: &'static str,
        value: String,
        reason: &'static str,
    },
}

/// Parse a `[0,1]` float env var, rejecting non-finite values (a NaN would
/// survive `clamp` and poison every threshold comparison). Absent → default.
fn parse_unit_float(var: &'static str, default: f32) -> Result<f32, ConfigError> {
    match std::env::var(var) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let v: f32 = raw.parse().map_err(|_| ConfigError::BadEnvValue {
                var,
                value: raw.clone(),
                reason: "expected a number in [0, 1]",
            })?;
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err(ConfigError::BadEnvValue {
                    var,
                    value: raw,
                    reason: "expected a finite number in [0, 1]",
                });
            }
            Ok(v)
        }
    }
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

        // A set-but-invalid value is a misconfiguration, not a reason to
        // silently fall back to the default; only an unset var uses the default.
        let db_max_connections = match std::env::var("GATHER_DB_MAX_CONNECTIONS") {
            Err(_) => 8,
            Ok(raw) => {
                let n: u32 = raw.parse().map_err(|_| ConfigError::BadEnvValue {
                    var: "GATHER_DB_MAX_CONNECTIONS",
                    value: raw.clone(),
                    reason: "expected a positive integer",
                })?;
                if n == 0 {
                    return Err(ConfigError::BadEnvValue {
                        var: "GATHER_DB_MAX_CONNECTIONS",
                        value: raw,
                        reason: "must be at least 1",
                    });
                }
                n
            }
        };

        let api_token = std::env::var("GATHER_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());

        let max_upload_mb = std::env::var("GATHER_MAX_UPLOAD_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        // 0 is a valid value (rate limiting disabled); only a malformed value
        // is an error rather than a silent fall-back to the default.
        let rate_limit_rps = match std::env::var("GATHER_RATE_LIMIT_RPS") {
            Err(_) => 50,
            Ok(raw) => raw.parse().map_err(|_| ConfigError::BadEnvValue {
                var: "GATHER_RATE_LIMIT_RPS",
                value: raw,
                reason: "expected a non-negative integer",
            })?,
        };

        let log_json = std::env::var("GATHER_LOG_JSON")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let env_bool = |name: &str, default: bool| {
            std::env::var(name)
                .map(|v| v == "true" || v == "1")
                .unwrap_or(default)
        };

        let auth_mode = std::env::var("GATHER_AUTH_MODE")
            .map(|v| v.to_lowercase())
            .unwrap_or_else(|_| "env".to_string());

        // Admission thresholds are validated rather than clamped: a non-finite
        // value (NaN survives clamp) would make every comparison false and
        // silently auto-admit everything, and drop_below > hold_below would
        // erase the hold band and drop units the docs say are auto-admitted.
        let admit_hold_below = parse_unit_float("GATHER_ADMIT_HOLD_BELOW", 0.5)?;
        let admit_drop_below = parse_unit_float("GATHER_ADMIT_DROP_BELOW", 0.0)?;
        if admit_drop_below > admit_hold_below {
            return Err(ConfigError::BadEnvValue {
                var: "GATHER_ADMIT_DROP_BELOW",
                value: admit_drop_below.to_string(),
                reason: "must be <= GATHER_ADMIT_HOLD_BELOW",
            });
        }

        let grpc_enabled = env_bool("GATHER_GRPC_ENABLED", true);
        let grpc_raw =
            std::env::var("GATHER_GRPC_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:7602".to_string());
        let grpc_bind_addr: SocketAddr = grpc_raw
            .parse()
            .map_err(|_| ConfigError::BadBindAddr(grpc_raw.clone()))?;
        if !grpc_bind_addr.ip().is_loopback() && !allow_non_loopback {
            return Err(ConfigError::NonLoopbackBind(grpc_bind_addr));
        }

        Ok(Self {
            bind_addr,
            database_url,
            db_max_connections,
            api_token,
            auth_mode,
            max_upload_mb,
            rate_limit_rps,
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
            ollama_vision_model: std::env::var("GATHER_OLLAMA_VISION_MODEL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
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
            admit_hold_below,
            admit_drop_below,
            cluster_enabled: env_bool("GATHER_CLUSTER_ENABLED", true),
            cluster_interval_secs: std::env::var("GATHER_CLUSTER_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.max(1))
                .unwrap_or(900),
            cluster_batch: std::env::var("GATHER_CLUSTER_BATCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: i64| v.clamp(2, 2_000))
                .unwrap_or(500),
            cluster_k: std::env::var("GATHER_CLUSTER_K")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.clamp(1, 50))
                .unwrap_or(6),
            // Validated (not clamped) so GATHER_CLUSTER_THRESHOLD=NaN is a
            // startup error rather than a threshold every comparison fails.
            cluster_threshold: parse_unit_float("GATHER_CLUSTER_THRESHOLD", 0.5)?,
            cluster_max_component: std::env::var("GATHER_CLUSTER_MAX_COMPONENT")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.clamp(2, 10_000))
                .unwrap_or(50),
            tune_enabled: env_bool("GATHER_TUNE_ENABLED", true),
            tune_interval_secs: std::env::var("GATHER_TUNE_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.max(1))
                .unwrap_or(600),
            tune_min_samples: std::env::var("GATHER_TUNE_MIN_SAMPLES")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.clamp(5, 100_000))
                .unwrap_or(20),
            // Validated: a NaN target would make every comparison false and
            // silently freeze (or, worse, loosen) the tuner.
            tune_target_precision: parse_unit_float("GATHER_TUNE_TARGET_PRECISION", 0.90)?,
            photo_enabled: env_bool("GATHER_PHOTO_ENABLED", true),
            photo_interval_secs: std::env::var("GATHER_PHOTO_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.max(1))
                .unwrap_or(600),
            photo_batch: std::env::var("GATHER_PHOTO_BATCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: i64| v.clamp(1, 1_000))
                .unwrap_or(32),
            photo_dup_max_distance: std::env::var("GATHER_PHOTO_DUP_MAX_DISTANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: u32| v.min(16))
                .unwrap_or(6),
            photo_album_gap_hours: std::env::var("GATHER_PHOTO_ALBUM_GAP_HOURS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: i64| v.clamp(1, 24 * 30))
                .unwrap_or(3),
            photo_album_split_km: std::env::var("GATHER_PHOTO_ALBUM_SPLIT_KM")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v: &f64| v.is_finite())
                .map(|v: f64| v.clamp(0.1, 20_000.0))
                .unwrap_or(10.0),
            photo_album_min_size: std::env::var("GATHER_PHOTO_ALBUM_MIN_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.clamp(1, 1_000))
                .unwrap_or(2),
            photo_topic_threshold: parse_unit_float("GATHER_PHOTO_TOPIC_THRESHOLD", 0.8)?,
            grpc_enabled,
            grpc_bind_addr,
        })
    }

    /// Baseline config for tests: loopback bind, auth off, extraction knobs
    /// at defaults, Ollama disabled, gRPC disabled (tests spawn grpc::serve
    /// on ephemeral ports directly).
    pub fn for_tests(database_url: String) -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().expect("static addr"),
            database_url,
            db_max_connections: 8,
            api_token: None,
            auth_mode: "env".to_string(),
            max_upload_mb: 16,
            rate_limit_rps: 0,
            log_json: false,
            allow_non_loopback: false,
            extraction_enabled: true,
            extraction_interval_secs: 30,
            extraction_batch: 8,
            tesseract_path: "tesseract".to_string(),
            ollama_url: None,
            ollama_model: "llama3.2:3b".to_string(),
            ollama_embed_model: "nomic-embed-text".to_string(),
            ollama_vision_model: None,
            scan_enabled: true,
            scan_interval_secs: 600,
            scan_batch: 32,
            scan_threshold: 0.65,
            scan_max_candidates: 25,
            admit_hold_below: 0.5,
            admit_drop_below: 0.0,
            cluster_enabled: true,
            cluster_interval_secs: 900,
            cluster_batch: 500,
            cluster_k: 6,
            cluster_threshold: 0.5,
            cluster_max_component: 50,
            tune_enabled: true,
            tune_interval_secs: 600,
            tune_min_samples: 20,
            tune_target_precision: 0.90,
            photo_enabled: true,
            photo_interval_secs: 600,
            photo_batch: 32,
            photo_dup_max_distance: 6,
            photo_album_gap_hours: 3,
            photo_album_split_km: 10.0,
            photo_album_min_size: 2,
            photo_topic_threshold: 0.8,
            grpc_enabled: false,
            grpc_bind_addr: "127.0.0.1:0".parse().expect("static addr"),
        }
    }
}
