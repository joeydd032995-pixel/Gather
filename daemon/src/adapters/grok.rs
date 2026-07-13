//! Grok / xAI adapter: account data export JSON.
//!
//! Input: the conversations file from an xAI data export —
//! `{conversations: [{conversation_id, title, create_time (epoch ms),
//! responses: [{sender: "human"|"assistant", message, create_time}]}]}`.
//! Also accepts a bare top-level array of the same conversation objects.

use serde_json::Value;

use super::{
    normalize_role, ts_from_epoch_ms, AdapterError, AdapterOutput, NormalizedConversation,
    NormalizedMessage,
};

const FORMAT: &str = "xai-export-v1";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let conversations = data
        .get("conversations")
        .and_then(Value::as_array)
        .or_else(|| data.as_array())
        .ok_or_else(|| malformed("expected 'conversations' array (or a top-level array)"))?;

    let mut out = Vec::with_capacity(conversations.len());
    for conv in conversations {
        let responses = conv
            .get("responses")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("conversation missing 'responses' array"))?;

        let mut messages = Vec::with_capacity(responses.len());
        for response in responses {
            let content = response
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty());
            let Some(content) = content else { continue };
            let role = response
                .get("sender")
                .and_then(Value::as_str)
                .unwrap_or("other");
            messages.push(NormalizedMessage {
                external_id: response
                    .get("response_id")
                    .and_then(Value::as_str)
                    .map(String::from),
                parent_external_id: None,
                role: normalize_role(role),
                author: None,
                model: response
                    .get("model")
                    .and_then(Value::as_str)
                    .map(String::from),
                content: content.to_string(),
                created_at: response.get("create_time").and_then(epoch_field),
            });
        }

        out.push(NormalizedConversation {
            external_id: conv
                .get("conversation_id")
                .or_else(|| conv.get("id"))
                .and_then(Value::as_str)
                .map(String::from),
            title: conv.get("title").and_then(Value::as_str).map(String::from),
            model: None,
            started_at: conv.get("create_time").and_then(epoch_field),
            ended_at: conv.get("update_time").and_then(epoch_field),
            messages,
        });
    }

    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: out,
    })
}

/// xAI timestamps appear as epoch-ms numbers or numeric strings.
fn epoch_field(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let ms = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    ts_from_epoch_ms(ms)
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "grok",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_xai_export() {
        let export = json!({
            "conversations": [{
                "conversation_id": "grok-1",
                "title": "budget chat",
                "create_time": 1767225600000i64,
                "responses": [
                    {"sender": "human", "message": "What laptop should I buy?",
                     "create_time": 1767225601000i64},
                    {"sender": "assistant", "message": "Depends on your budget.",
                     "create_time": "1767225605000"}
                ]
            }]
        });
        let out = parse(&export).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.external_id.as_deref(), Some("grok-1"));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user"); // human normalized
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[0].created_at.unwrap().timestamp(), 1767225601);
    }

    #[test]
    fn accepts_bare_array_and_rejects_garbage() {
        let bare = json!([{"conversation_id": "c", "responses": []}]);
        assert_eq!(parse(&bare).unwrap().conversations.len(), 1);
        assert!(parse(&json!({"foo": "bar"})).is_err());
    }
}
