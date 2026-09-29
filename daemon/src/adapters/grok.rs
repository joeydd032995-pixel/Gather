//! Grok / xAI adapter: account data export JSON.
//!
//! Input: the conversations file from an xAI data export —
//! `{conversations: [{conversation_id, title, create_time (epoch ms),
//! responses: [{sender: "human"|"assistant", message, create_time}]}]}`.
//! Also accepts a bare top-level array of the same conversation objects.

use serde_json::Value;

use super::{
    normalize_role, ts_from_epoch_f64, ts_from_epoch_ms, AdapterError, AdapterOutput,
    NormalizedConversation, NormalizedMessage,
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
        // The account export wraps a conversation's own fields in a
        // `conversation` object and each reply in a `response` object; the
        // flat shape (fields directly on the item) is accepted too.
        let meta = conv
            .get("conversation")
            .filter(|c| c.is_object())
            .unwrap_or(conv);

        let mut messages = Vec::with_capacity(responses.len());
        for item in responses {
            let response = item
                .get("response")
                .filter(|r| r.is_object())
                .unwrap_or(item);
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
                external_id: first_str(response, &["_id", "response_id", "id"]),
                parent_external_id: first_str(response, &["parent_response_id"]),
                role: normalize_role(role),
                author: None,
                model: first_str(response, &["model"]),
                content: content.to_string(),
                created_at: response.get("create_time").and_then(time_field),
            });
        }
        // Replies are kept in the order they were made, when every one says
        // when (the sort is stable, so equal times keep their file order).
        if messages.iter().all(|m| m.created_at.is_some()) {
            messages.sort_by_key(|m| m.created_at);
        }

        out.push(NormalizedConversation {
            external_id: first_str(meta, &["id", "conversation_id"])
                .or_else(|| first_str(conv, &["conversation_id", "id"])),
            title: first_str(meta, &["title"]),
            model: None,
            started_at: meta.get("create_time").and_then(time_field),
            ended_at: meta
                .get("modify_time")
                .or_else(|| meta.get("update_time"))
                .and_then(time_field),
            messages,
        });
    }

    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: out,
    })
}

fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// xAI timestamps appear as epoch numbers (milliseconds, or seconds when
/// small) or numeric strings, RFC 3339 strings, and MongoDB-style
/// `{"$date": …}` wrappers holding any of those (`{"$numberLong": "…"}`).
fn time_field(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    match v {
        Value::Number(n) => epoch(n.as_f64()?),
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<f64>() {
                epoch(n)
            } else {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            }
        }
        Value::Object(o) => {
            if let Some(inner) = o.get("$date") {
                return time_field(inner);
            }
            o.get("$numberLong").and_then(time_field)
        }
        _ => None,
    }
}

/// Below 1e11 a number is seconds (that reaches year 5138 as milliseconds'
/// worth of 1970), otherwise milliseconds.
fn epoch(n: f64) -> Option<chrono::DateTime<chrono::Utc>> {
    if n.abs() < 1e11 {
        ts_from_epoch_f64(n)
    } else {
        ts_from_epoch_ms(n)
    }
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
    fn parses_the_wrapped_account_export() {
        // conversation and response objects wrapped, Mongo-style dates,
        // upper-case sender, replies out of order.
        let export = json!({
            "conversations": [{
                "conversation": {
                    "id": "c-1", "title": "Laptop advice",
                    "create_time": "2026-03-01T09:00:00.123Z",
                    "modify_time": "2026-03-01T09:05:00Z"
                },
                "responses": [
                    {"response": {"_id": "r2", "sender": "ASSISTANT", "model": "grok-3",
                        "message": "Depends on your budget.", "parent_response_id": "r1",
                        "create_time": {"$date": {"$numberLong": "1772355605000"}}},
                     "share_link": null},
                    {"response": {"_id": "r1", "sender": "human",
                        "message": "What laptop should I buy?",
                        "create_time": {"$date": {"$numberLong": "1772355601000"}}}},
                    {"response": {"_id": "r3", "sender": "human", "message": "  ",
                        "create_time": {"$date": {"$numberLong": "1772355700000"}}}}
                ]
            }],
            "projects": [], "tasks": []
        });
        let out = parse(&export).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.external_id.as_deref(), Some("c-1"));
        assert_eq!(conv.title.as_deref(), Some("Laptop advice"));
        assert_eq!(conv.started_at.unwrap().timestamp(), 1772355600);
        assert_eq!(conv.messages.len(), 2, "the blank reply is left out");
        // Put in the order they were made, whatever order the file had.
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What laptop should I buy?");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].model.as_deref(), Some("grok-3"));
        assert_eq!(conv.messages[1].parent_external_id.as_deref(), Some("r1"));
        assert_eq!(conv.messages[1].external_id.as_deref(), Some("r2"));
    }

    #[test]
    fn times_are_read_from_every_form_they_come_in() {
        let ms = 1_772_355_601_000i64;
        for v in [
            json!(ms),
            json!(ms.to_string()),
            json!(1_772_355_601i64),
            json!("2026-03-01T09:00:01Z"),
            json!({"$date": ms}),
            json!({"$date": {"$numberLong": ms.to_string()}}),
            json!({"$date": "2026-03-01T09:00:01Z"}),
        ] {
            let t = time_field(&v).unwrap_or_else(|| panic!("{v}"));
            assert_eq!(t.timestamp(), 1_772_355_601, "{v}");
        }
        assert!(time_field(&json!(null)).is_none());
        assert!(time_field(&json!("not a time")).is_none());
    }

    #[test]
    fn accepts_bare_array_and_rejects_garbage() {
        let bare = json!([{"conversation_id": "c", "responses": []}]);
        assert_eq!(parse(&bare).unwrap().conversations.len(), 1);
        assert!(parse(&json!({"foo": "bar"})).is_err());
    }
}
