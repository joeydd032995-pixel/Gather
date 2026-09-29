//! Which platform an export JSON came from, by its shape alone.
//!
//! Used where the platform isn't chosen by hand, such as a dropped export or an
//! inbox folder. Every check looks for a key only that platform's export
//! has, on the first record, and never guesses: an unrecognised file is
//! reported as such and left alone.

use serde_json::Value;

/// The adapter name (`chatgpt`, `claude`, …) for `data`, or None.
pub fn platform_of(data: &Value) -> Option<&'static str> {
    if let Some(items) = data.as_array() {
        let first = items.iter().find(|v| v.is_object())?;
        if first.get("mapping").is_some_and(Value::is_object) {
            return Some("chatgpt");
        }
        if first.get("chat_messages").is_some_and(Value::is_array) {
            return Some("claude");
        }
        if first.get("responses").is_some_and(Value::is_array) {
            return Some("grok");
        }
        // Google Takeout "My Activity" is shared by every Google product:
        // only prompts to Gemini count.
        if items.iter().any(|v| {
            v.get("title")
                .and_then(Value::as_str)
                .is_some_and(|t| t.replace('\u{a0}', " ").starts_with("Prompted "))
        }) {
            return Some("gemini");
        }
        return None;
    }

    if data.get("schema").and_then(Value::as_str) == Some("gather-generic-v1") {
        return Some("generic");
    }
    if data.get("requests").is_some_and(Value::is_array) {
        return Some("copilot");
    }
    if data.get("threads").is_some_and(Value::is_array)
        || data.get("entries").is_some_and(Value::is_array)
    {
        return Some("perplexity");
    }
    if let Some(conversations) = data.get("conversations").and_then(Value::as_array) {
        let first = conversations.iter().find(|v| v.is_object())?;
        if first.get("responses").is_some_and(Value::is_array) {
            return Some("grok");
        }
        if first.get("messages").is_some_and(Value::is_array) {
            return Some("generic");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_platform_is_told_by_its_own_key() {
        assert_eq!(
            platform_of(&json!([{"title": "t", "mapping": {"a": {}}}])),
            Some("chatgpt")
        );
        assert_eq!(
            platform_of(&json!([{"uuid": "u", "chat_messages": []}])),
            Some("claude")
        );
        assert_eq!(
            platform_of(&json!({"conversations": [{"responses": []}]})),
            Some("grok")
        );
        assert_eq!(platform_of(&json!([{"responses": []}])), Some("grok"));
        assert_eq!(
            platform_of(&json!([
                {"title": "Used Gemini Apps"},
                {"title": "Prompted what is a tsvector"}
            ])),
            Some("gemini")
        );
        // Takeout's title has a non-breaking space after "Prompted".
        assert_eq!(
            platform_of(&json!([{"title": "Prompted\u{a0}what is a tsvector"}])),
            Some("gemini")
        );
        assert_eq!(platform_of(&json!({"requests": []})), Some("copilot"));
        assert_eq!(platform_of(&json!({"threads": []})), Some("perplexity"));
        assert_eq!(
            platform_of(&json!({"entries": [{"query": "q"}]})),
            Some("perplexity")
        );
        assert_eq!(
            platform_of(&json!({"schema": "gather-generic-v1", "conversations": []})),
            Some("generic")
        );
        assert_eq!(
            platform_of(&json!({"conversations": [{"messages": []}]})),
            Some("generic")
        );
    }

    #[test]
    fn anything_else_is_not_guessed() {
        assert_eq!(platform_of(&json!([])), None);
        assert_eq!(platform_of(&json!({"name": "package.json"})), None);
        assert_eq!(platform_of(&json!([1, 2, 3])), None);
        // Other Google activity is not Gemini.
        assert_eq!(platform_of(&json!([{"title": "Searched for cats"}])), None);
        assert_eq!(platform_of(&json!("text")), None);
    }
}
