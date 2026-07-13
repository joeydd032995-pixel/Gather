//! GitHub Copilot Chat adapter: VS Code chat session JSON.
//!
//! Input: a `chat.json` session file from VS Code workspace storage —
//! `{requesterUsername?, requests: [{message: {text}, response: [{value} |
//! {kind, …}], timestamp?}]}`. One conversation per file; each request
//! yields a user message and, when present, an assistant message built from
//! the concatenated string `value` parts of the response.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    ts_from_epoch_ms, AdapterError, AdapterOutput, NormalizedConversation, NormalizedMessage,
};

const FORMAT: &str = "vscode-chat-session-v1";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let requests = data
        .get("requests")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("expected a 'requests' array (VS Code chat session)"))?;

    let author = data
        .get("requesterUsername")
        .and_then(Value::as_str)
        .map(String::from);

    let mut messages = Vec::with_capacity(requests.len() * 2);
    for request in requests {
        let time = request
            .get("timestamp")
            .and_then(Value::as_f64)
            .and_then(ts_from_epoch_ms);
        if let Some(text) = request
            .pointer("/message/text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            messages.push(NormalizedMessage {
                external_id: request
                    .get("requestId")
                    .and_then(Value::as_str)
                    .map(String::from),
                parent_external_id: None,
                role: "user".to_string(),
                author: author.clone(),
                model: None,
                content: text.to_string(),
                created_at: time,
            });
        }
        let response_text = response_text(request.get("response"));
        if !response_text.is_empty() {
            messages.push(NormalizedMessage {
                external_id: None,
                parent_external_id: None,
                role: "assistant".to_string(),
                author: Some("copilot".to_string()),
                model: request
                    .pointer("/modelId")
                    .and_then(Value::as_str)
                    .map(String::from),
                content: response_text,
                created_at: time,
            });
        }
    }

    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: vec![NormalizedConversation {
            external_id: data
                .get("sessionId")
                .and_then(Value::as_str)
                .map(String::from),
            title: data
                .get("customTitle")
                .and_then(Value::as_str)
                .map(String::from),
            model: None,
            started_at: first_time(&messages),
            ended_at: last_time(&messages),
            messages,
        }],
    })
}

/// Response parts are `{value: "markdown"}` blocks interleaved with
/// tool-call/`kind` markers; keep the string values.
fn response_text(response: Option<&Value>) -> String {
    let Some(parts) = response.and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|p| p.get("value").and_then(Value::as_str))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_time(messages: &[NormalizedMessage]) -> Option<DateTime<Utc>> {
    messages.iter().find_map(|m| m.created_at)
}

fn last_time(messages: &[NormalizedMessage]) -> Option<DateTime<Utc>> {
    messages.iter().rev().find_map(|m| m.created_at)
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "copilot",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_vscode_chat_session() {
        let export = json!({
            "sessionId": "sess-42",
            "requesterUsername": "joey",
            "requests": [{
                "requestId": "r1",
                "message": {"text": "add a health endpoint"},
                "timestamp": 1767225600000i64,
                "response": [
                    {"value": "Here is the handler:"},
                    {"kind": "codeblockUri"},
                    {"value": "fn healthz() {}"}
                ]
            }]
        });
        let out = parse(&export).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.external_id.as_deref(), Some("sess-42"));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].author.as_deref(), Some("joey"));
        assert_eq!(conv.messages[1].role, "assistant");
        assert!(conv.messages[1].content.contains("fn healthz"));
    }

    #[test]
    fn rejects_non_session_payloads() {
        assert!(parse(&json!({"conversations": []})).is_err());
    }
}
