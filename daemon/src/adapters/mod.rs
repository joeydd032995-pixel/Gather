//! Source adapters: normalize any AI platform's export format into the
//! `artifacts -> conversations -> messages` schema.
//!
//! Every adapter is a pure function from raw export JSON to a list of
//! [`NormalizedConversation`]s. Adapters never touch the database; the ingest
//! route owns persistence, dedup, and provenance. This keeps platform quirks
//! testable in isolation and makes adding a platform a one-file change.

pub mod chatgpt;
pub mod claude;
pub mod copilot;
pub mod gemini;
pub mod generic;
pub mod grok;
pub mod perplexity;

use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMessage {
    pub external_id: Option<String>,
    pub parent_external_id: Option<String>,
    pub role: String, // system | user | assistant | tool | function | other
    pub author: Option<String>,
    pub model: Option<String>,
    pub content: String,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedConversation {
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub messages: Vec<NormalizedMessage>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error(
        "unsupported platform '{0}'; supported: chatgpt, claude, gemini, grok, perplexity, \
         copilot, generic"
    )]
    UnsupportedPlatform(String),
    #[error("malformed {platform} export: {reason}")]
    Malformed {
        platform: &'static str,
        reason: String,
    },
}

/// Identifies which adapter parsed an export, recorded on the artifact so
/// provenance queries can always answer "which platform said this".
pub struct AdapterOutput {
    pub source_format_version: &'static str,
    pub conversations: Vec<NormalizedConversation>,
}

/// Dispatch an export payload to the adapter registered for `platform`.
pub fn normalize(platform: &str, data: &Value) -> Result<AdapterOutput, AdapterError> {
    match platform {
        "chatgpt" => chatgpt::parse(data),
        "claude" => claude::parse(data),
        "gemini" => gemini::parse(data),
        "grok" => grok::parse(data),
        "perplexity" => perplexity::parse(data),
        "copilot" => copilot::parse(data),
        "generic" => generic::parse(data),
        other => Err(AdapterError::UnsupportedPlatform(other.to_string())),
    }
}

pub(crate) fn normalize_role(raw: &str) -> String {
    match raw {
        "system" | "user" | "assistant" | "tool" | "function" => raw.to_string(),
        "human" => "user".to_string(),
        "ai" | "model" | "bot" => "assistant".to_string(),
        _ => "other".to_string(),
    }
}

pub(crate) fn ts_from_epoch_f64(secs: f64) -> Option<DateTime<Utc>> {
    let whole = secs.trunc() as i64;
    let nanos = ((secs - secs.trunc()) * 1e9) as u32;
    DateTime::from_timestamp(whole, nanos)
}

pub(crate) fn ts_from_epoch_ms(millis: f64) -> Option<DateTime<Utc>> {
    ts_from_epoch_f64(millis / 1000.0)
}
