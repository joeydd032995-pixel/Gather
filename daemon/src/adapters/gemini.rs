//! Google Gemini adapter: Takeout "My Activity" JSON.
//!
//! Input: `MyActivity.json` from a Google Takeout export (Gemini Apps /
//! Bard product) — a JSON array of activity records:
//! `{title: "Prompted <text>", time: RFC3339, safeHtmlItem: {htmlValue}?}`.
//! Each record becomes one conversation: the prompt as the user message and,
//! when the export includes it, the tag-stripped `htmlValue` as the
//! assistant message. Takeout frequently omits responses — those records
//! still ingest as single-message conversations (documented in §2).

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{AdapterError, AdapterOutput, NormalizedConversation, NormalizedMessage};

const FORMAT: &str = "google-takeout-myactivity-v1";
const PROMPT_PREFIX: &str = "Prompted ";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let records = data
        .as_array()
        .ok_or_else(|| malformed("top level must be a JSON array of activity records"))?;

    let mut conversations = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Some(title) = record.get("title").and_then(Value::as_str) else {
            continue; // non-prompt activity rows (e.g. "Used Gemini Apps")
        };
        let Some(prompt) = title.strip_prefix(PROMPT_PREFIX) else {
            continue;
        };
        let prompt = prompt.trim();
        if prompt.is_empty() {
            continue;
        }
        let time = record
            .get("time")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let mut messages = vec![NormalizedMessage {
            external_id: None,
            parent_external_id: None,
            role: "user".to_string(),
            author: None,
            model: None,
            content: prompt.to_string(),
            created_at: time,
        }];
        if let Some(html) = record
            .pointer("/safeHtmlItem/htmlValue")
            .and_then(Value::as_str)
        {
            let text = strip_html(html);
            if !text.is_empty() {
                messages.push(NormalizedMessage {
                    external_id: None,
                    parent_external_id: None,
                    role: "assistant".to_string(),
                    author: None,
                    model: None,
                    content: text,
                    created_at: time,
                });
            }
        }

        conversations.push(NormalizedConversation {
            external_id: Some(format!("takeout-{index}")),
            title: Some(truncate(prompt, 120)),
            model: None,
            started_at: time,
            ended_at: time,
            messages,
        });
    }

    if conversations.is_empty() {
        return Err(malformed(
            "no 'Prompted …' activity records found (is this a Gemini MyActivity.json?)",
        ));
    }
    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations,
    })
}

/// Minimal tag stripper for Takeout's sanitized HTML: drops tags, decodes
/// the handful of entities Takeout emits, collapses whitespace.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "gemini",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_prompt_and_html_response() {
        let export = json!([
            {"header": "Gemini Apps", "title": "Used Gemini Apps", "time": "2026-02-01T08:00:00Z"},
            {
                "header": "Gemini Apps",
                "title": "Prompted what is my VPS budget",
                "time": "2026-02-01T08:01:00Z",
                "safeHtmlItem": {"htmlValue": "<p>Your budget is <b>$75</b> per month.</p>"}
            }
        ]);
        let out = parse(&export).unwrap();
        assert_eq!(out.conversations.len(), 1); // non-prompt record skipped
        let conv = &out.conversations[0];
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "what is my VPS budget");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "Your budget is $75 per month.");
        assert!(conv.started_at.is_some());
    }

    #[test]
    fn prompt_without_response_still_ingests() {
        let export = json!([
            {"title": "Prompted summarize my notes", "time": "2026-02-01T08:00:00Z"}
        ]);
        let out = parse(&export).unwrap();
        assert_eq!(out.conversations[0].messages.len(), 1);
    }

    #[test]
    fn rejects_non_gemini_payloads() {
        assert!(parse(&json!({"not": "an array"})).is_err());
        assert!(parse(&json!([{"title": "Watched a video"}])).is_err());
    }
}
