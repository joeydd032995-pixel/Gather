//! Generic fallback adapter: the documented `gather-generic-v1` schema.
//!
//! Any platform without a dedicated adapter can be ingested by converting its
//! export to this shape (documented in docs/TECHNICAL-WRITEUP.md §4.2):
//!
//! ```json
//! {
//!   "schema": "gather-generic-v1",
//!   "conversations": [{
//!     "id": "optional-external-id",
//!     "title": "optional",
//!     "model": "optional",
//!     "started_at": "RFC3339 optional",
//!     "messages": [{
//!       "role": "user|assistant|system|tool",
//!       "author": "optional",
//!       "content": "required",
//!       "created_at": "RFC3339 optional"
//!     }]
//!   }]
//! }
//! ```
//!
//! This same adapter backs agent-log ingestion (Claude Code / Goose / Aider
//! JSONL is converted line-by-line into this shape by the ingest route).

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    normalize_role, AdapterError, AdapterOutput, NormalizedConversation, NormalizedMessage,
};

const FORMAT: &str = "gather-generic-v1";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let conversations = data
        .get("conversations")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("missing 'conversations' array"))?;

    let mut out = Vec::with_capacity(conversations.len());
    for conv in conversations {
        let messages_raw = conv
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("conversation missing 'messages' array"))?;

        let mut messages = Vec::with_capacity(messages_raw.len());
        for msg in messages_raw {
            let content = msg
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty());
            let Some(content) = content else {
                continue;
            };
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("other");
            messages.push(NormalizedMessage {
                external_id: msg.get("id").and_then(Value::as_str).map(String::from),
                parent_external_id: None,
                role: normalize_role(role),
                author: msg.get("author").and_then(Value::as_str).map(String::from),
                model: msg.get("model").and_then(Value::as_str).map(String::from),
                content: content.to_string(),
                created_at: rfc3339(msg.get("created_at")),
            });
        }

        out.push(NormalizedConversation {
            external_id: conv.get("id").and_then(Value::as_str).map(String::from),
            title: conv.get("title").and_then(Value::as_str).map(String::from),
            model: conv.get("model").and_then(Value::as_str).map(String::from),
            started_at: rfc3339(conv.get("started_at")),
            ended_at: rfc3339(conv.get("ended_at")),
            messages,
        });
    }

    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: out,
    })
}

fn rfc3339(v: Option<&Value>) -> Option<DateTime<Utc>> {
    v.and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "generic",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_generic_schema_and_skips_empty_messages() {
        let export = json!({
            "schema": "gather-generic-v1",
            "conversations": [{
                "id": "sess-42",
                "title": "Agent session",
                "messages": [
                    {"role": "user", "content": "Deploy the daemon"},
                    {"role": "assistant", "content": "  "},
                    {"role": "assistant", "content": "Done", "created_at": "2026-02-01T12:00:00Z"}
                ]
            }]
        });

        let out = parse(&export).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].content, "Done");
        assert!(conv.messages[1].created_at.is_some());
    }

    #[test]
    fn rejects_missing_conversations_key() {
        assert!(parse(&json!({"schema": "gather-generic-v1"})).is_err());
    }
}
