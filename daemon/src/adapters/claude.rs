//! Claude.ai data-export adapter.
//!
//! Input: the `conversations.json` file from a claude.ai account data export —
//! a JSON array of conversations, each with `uuid`, `name`, `created_at`,
//! `updated_at`, and `chat_messages`: [{uuid, sender: "human"|"assistant",
//! text, content: [{type: "text", text}], created_at}]. Timestamps are
//! RFC 3339 strings.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    normalize_role, AdapterError, AdapterOutput, NormalizedConversation, NormalizedMessage,
};

const FORMAT: &str = "claude-ai-export-json-v1";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let conversations = data
        .as_array()
        .ok_or_else(|| malformed("top level must be a JSON array of conversations"))?;

    let mut out = Vec::with_capacity(conversations.len());
    for conv in conversations {
        let messages_raw = conv
            .get("chat_messages")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("conversation missing 'chat_messages' array"))?;

        let mut messages = Vec::with_capacity(messages_raw.len());
        for msg in messages_raw {
            let Some(content) = extract_text(msg) else {
                continue;
            };
            let role = msg.get("sender").and_then(Value::as_str).unwrap_or("other");
            messages.push(NormalizedMessage {
                external_id: msg.get("uuid").and_then(Value::as_str).map(String::from),
                parent_external_id: None,
                role: normalize_role(role),
                author: None,
                model: None,
                content,
                created_at: rfc3339(msg.get("created_at")),
            });
        }

        out.push(NormalizedConversation {
            external_id: conv.get("uuid").and_then(Value::as_str).map(String::from),
            title: conv.get("name").and_then(Value::as_str).map(String::from),
            model: conv.get("model").and_then(Value::as_str).map(String::from),
            started_at: rfc3339(conv.get("created_at")),
            ended_at: rfc3339(conv.get("updated_at")),
            messages,
        });
    }

    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: out,
    })
}

/// Newer exports carry structured `content` blocks; older ones only `text`.
fn extract_text(msg: &Value) -> Option<String> {
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        let text: Vec<&str> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        let joined = text.join("\n").trim().to_string();
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    msg.get("text")
        .and_then(Value::as_str)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn rfc3339(v: Option<&Value>) -> Option<DateTime<Utc>> {
    v.and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "claude",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_claude_export_with_content_blocks() {
        let export = json!([{
            "uuid": "c0ffee00-0000-0000-0000-000000000001",
            "name": "VPS planning",
            "created_at": "2026-01-05T10:00:00Z",
            "updated_at": "2026-01-05T10:30:00Z",
            "chat_messages": [
                {
                    "uuid": "m-1",
                    "sender": "human",
                    "created_at": "2026-01-05T10:00:01Z",
                    "content": [{"type": "text", "text": "I decided to use Hetzner for backups"}],
                    "text": "I decided to use Hetzner for backups"
                },
                {
                    "uuid": "m-2",
                    "sender": "assistant",
                    "created_at": "2026-01-05T10:00:05Z",
                    "content": [{"type": "text", "text": "Hetzner CX22 is a good fit."}],
                    "text": "Hetzner CX22 is a good fit."
                }
            ]
        }]);

        let out = parse(&export).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.title.as_deref(), Some("VPS planning"));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user"); // "human" normalized
        assert_eq!(conv.messages[1].role, "assistant");
        assert!(conv.started_at.is_some());
    }
}
