//! Perplexity adapter: thread export JSON.
//!
//! Input: `{threads: [{title, entries: [{query, answer, timestamp}]}]}`;
//! a top-level `entries` array (single-thread export) is also accepted.
//! Each entry becomes a user(query) + assistant(answer) message pair.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{AdapterError, AdapterOutput, NormalizedConversation, NormalizedMessage};

const FORMAT: &str = "perplexity-thread-export-v1";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let threads: Vec<&Value> = if let Some(threads) = data.get("threads").and_then(Value::as_array)
    {
        threads.iter().collect()
    } else if data.get("entries").and_then(Value::as_array).is_some() {
        vec![data] // single-thread export
    } else {
        return Err(malformed("expected 'threads' array or top-level 'entries'"));
    };

    let mut out = Vec::with_capacity(threads.len());
    for (index, thread) in threads.into_iter().enumerate() {
        let entries = thread
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("thread missing 'entries' array"))?;

        let mut messages = Vec::with_capacity(entries.len() * 2);
        for entry in entries {
            let time = rfc3339(entry.get("timestamp"));
            if let Some(query) = text_field(entry, "query") {
                messages.push(message("user", query, time));
            }
            if let Some(answer) = text_field(entry, "answer") {
                messages.push(message("assistant", answer, time));
            }
        }

        out.push(NormalizedConversation {
            external_id: thread
                .get("id")
                .or_else(|| thread.get("uuid"))
                .and_then(Value::as_str)
                .map(String::from)
                .or(Some(format!("thread-{index}"))),
            title: thread
                .get("title")
                .and_then(Value::as_str)
                .map(String::from),
            model: None,
            started_at: messages.first().and_then(|m| m.created_at),
            ended_at: messages.last().and_then(|m| m.created_at),
            messages,
        });
    }

    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: out,
    })
}

fn message(role: &str, content: String, created_at: Option<DateTime<Utc>>) -> NormalizedMessage {
    NormalizedMessage {
        external_id: None,
        parent_external_id: None,
        role: role.to_string(),
        author: None,
        model: None,
        content,
        created_at,
    }
}

fn text_field(entry: &Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn rfc3339(v: Option<&Value>) -> Option<DateTime<Utc>> {
    v?.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "perplexity",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_threads_with_query_answer_pairs() {
        let export = json!({
            "threads": [{
                "id": "px-1",
                "title": "hosting research",
                "entries": [
                    {"query": "cheapest EU VPS?", "answer": "Hetzner CX22 at EUR 3.79.",
                     "timestamp": "2026-03-01T09:00:00Z"},
                    {"query": "and with a volume?", "answer": "About EUR 4.75 total.",
                     "timestamp": "2026-03-01T09:01:00Z"}
                ]
            }]
        });
        let out = parse(&export).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.external_id.as_deref(), Some("px-1"));
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[1].role, "assistant");
        assert!(conv.messages[1].content.contains("Hetzner"));
    }

    #[test]
    fn accepts_single_thread_entries_form() {
        let export = json!({
            "entries": [{"query": "q", "answer": "a", "timestamp": "2026-03-01T09:00:00Z"}]
        });
        let out = parse(&export).unwrap();
        assert_eq!(out.conversations.len(), 1);
        assert_eq!(out.conversations[0].messages.len(), 2);
    }

    #[test]
    fn rejects_unknown_shape() {
        assert!(parse(&json!({"conversations": []})).is_err());
    }
}
