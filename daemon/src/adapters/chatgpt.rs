//! ChatGPT / OpenAI data-export adapter.
//!
//! Input: the `conversations.json` file from an OpenAI account data export —
//! a JSON array of conversation objects. Each conversation stores messages as
//! a tree in `mapping` (node id -> {message, parent, children}) because the
//! UI supports regenerated branches. We linearize by walking parent links
//! upward from `current_node`, which reproduces the transcript the user
//! actually saw, then keep parent ids so the branch structure survives in
//! `messages.parent_message_id`.

use serde_json::Value;

use super::{
    normalize_role, ts_from_epoch_f64, AdapterError, AdapterOutput, NormalizedConversation,
    NormalizedMessage,
};

const FORMAT: &str = "openai-conversations-json-v1";

pub fn parse(data: &Value) -> Result<AdapterOutput, AdapterError> {
    let conversations = data
        .as_array()
        .ok_or_else(|| malformed("top level must be a JSON array of conversations"))?;

    let mut out = Vec::with_capacity(conversations.len());
    for conv in conversations {
        out.push(parse_conversation(conv)?);
    }
    Ok(AdapterOutput {
        source_format_version: FORMAT,
        conversations: out,
    })
}

fn parse_conversation(conv: &Value) -> Result<NormalizedConversation, AdapterError> {
    let mapping = conv
        .get("mapping")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed("conversation missing 'mapping' object"))?;

    // Walk the active branch: current_node -> parent -> ... -> root.
    let mut chain: Vec<&str> = Vec::new();
    let mut cursor = conv.get("current_node").and_then(Value::as_str);
    while let Some(node_id) = cursor {
        if chain.contains(&node_id) {
            return Err(malformed("cycle in mapping parent chain"));
        }
        chain.push(node_id);
        cursor = mapping
            .get(node_id)
            .and_then(|n| n.get("parent"))
            .and_then(Value::as_str);
    }
    chain.reverse();

    let mut messages = Vec::new();
    for node_id in &chain {
        let Some(node) = mapping.get(*node_id) else {
            continue;
        };
        let Some(msg) = node.get("message").filter(|m| !m.is_null()) else {
            continue; // synthetic root nodes carry no message
        };
        let Some(content) = extract_text(msg) else {
            continue; // hidden/system stubs with empty parts
        };
        let role = msg
            .pointer("/author/role")
            .and_then(Value::as_str)
            .unwrap_or("other");
        messages.push(NormalizedMessage {
            external_id: msg.get("id").and_then(Value::as_str).map(String::from),
            parent_external_id: node.get("parent").and_then(Value::as_str).map(String::from),
            role: normalize_role(role),
            author: msg
                .pointer("/author/name")
                .and_then(Value::as_str)
                .map(String::from),
            model: msg
                .pointer("/metadata/model_slug")
                .and_then(Value::as_str)
                .map(String::from),
            content,
            created_at: msg
                .get("create_time")
                .and_then(Value::as_f64)
                .and_then(ts_from_epoch_f64),
        });
    }

    Ok(NormalizedConversation {
        external_id: conv
            .get("conversation_id")
            .or_else(|| conv.get("id"))
            .and_then(Value::as_str)
            .map(String::from),
        title: conv.get("title").and_then(Value::as_str).map(String::from),
        model: conv
            .get("default_model_slug")
            .and_then(Value::as_str)
            .map(String::from),
        started_at: conv
            .get("create_time")
            .and_then(Value::as_f64)
            .and_then(ts_from_epoch_f64),
        ended_at: conv
            .get("update_time")
            .and_then(Value::as_f64)
            .and_then(ts_from_epoch_f64),
        messages,
    })
}

/// ChatGPT stores message bodies as {content_type, parts: [...]} where parts
/// are strings for text and objects for multimodal attachments.
fn extract_text(msg: &Value) -> Option<String> {
    let parts = msg.pointer("/content/parts")?.as_array()?;
    let text: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
    let joined = text.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn malformed(reason: &str) -> AdapterError {
    AdapterError::Malformed {
        platform: "chatgpt",
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn linearizes_active_branch_of_message_tree() {
        let export = json!([{
            "conversation_id": "conv-1",
            "title": "Budget talk",
            "create_time": 1735689600.0,
            "update_time": 1735693200.5,
            "current_node": "n2",
            "mapping": {
                "root": {"id": "root", "message": null, "parent": null, "children": ["n1"]},
                "n1": {
                    "id": "n1",
                    "parent": "root",
                    "children": ["n2", "n2-regen"],
                    "message": {
                        "id": "m1",
                        "author": {"role": "user"},
                        "create_time": 1735689601.0,
                        "content": {"content_type": "text", "parts": ["My budget is $50/month"]}
                    }
                },
                "n2": {
                    "id": "n2",
                    "parent": "n1",
                    "children": [],
                    "message": {
                        "id": "m2",
                        "author": {"role": "assistant"},
                        "metadata": {"model_slug": "gpt-4o"},
                        "content": {"content_type": "text", "parts": ["Noted: $50/month budget."]}
                    }
                },
                "n2-regen": {
                    "id": "n2-regen",
                    "parent": "n1",
                    "children": [],
                    "message": {
                        "id": "m2b",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Regenerated answer, not on active branch"]}
                    }
                }
            }
        }]);

        let out = parse(&export).unwrap();
        assert_eq!(out.conversations.len(), 1);
        let conv = &out.conversations[0];
        assert_eq!(conv.external_id.as_deref(), Some("conv-1"));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "My budget is $50/month");
        assert_eq!(conv.messages[1].model.as_deref(), Some("gpt-4o"));
        // The regenerated sibling branch is excluded.
        assert!(!conv
            .messages
            .iter()
            .any(|m| m.content.contains("Regenerated")));
    }

    #[test]
    fn rejects_non_array_export() {
        assert!(parse(&json!({"not": "an array"})).is_err());
    }
}
