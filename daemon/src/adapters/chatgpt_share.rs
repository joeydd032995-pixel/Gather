//! A ChatGPT shared-conversation page, saved from the browser.
//!
//! ChatGPT has no export for a conversation made in some of its products
//! other than a share link. Opening the link and saving the page (Ctrl+S,
//! "Webpage, HTML only") keeps the conversation: the page carries it inside
//! React Router's streamed data, in `window.__reactRouterContext
//! .streamController.enqueue("…")` scripts, as the same `mapping` /
//! `current_node` object a ChatGPT data export holds. Reading that needs no
//! network and no sign-in: this decodes the stream and hands the conversation
//! to the ChatGPT adapter.
//!
//! The stream is `turbo-stream`'s flat encoding: one JSON array where an
//! object is `{"_<i>": <j>}` (the key is the string at index `i`, the value the
//! entry at index `j`), an array lists indices, strings, numbers and booleans
//! sit inline, and small negative numbers stand for `null`, `undefined` and
//! the like. Only what is needed is decoded: the object that has both a
//! `mapping` and a `current_node`.

use serde_json::{Map, Value};

const ENQUEUE: &str = "streamController.enqueue(";
/// Guards against a hostile or corrupt page: nesting and total entries read.
const MAX_DEPTH: usize = 200;
const MAX_STEPS: usize = 20_000_000;

/// Whether `html` looks like a saved ChatGPT share page.
pub fn looks_like(html: &str) -> bool {
    html.contains(ENQUEUE) && html.contains("mapping") && html.contains("current_node")
}

/// The shared conversation, shaped like one entry of a ChatGPT export's
/// `conversations.json` (so `chatgpt::parse(&json!([conversation]))` reads it).
pub fn extract(html: &str) -> Option<Value> {
    let mut from = 0;
    while let Some(at) = html[from..].find(ENQUEUE) {
        let start = from + at + ENQUEUE.len();
        from = start;
        let Some(literal) = js_string_literal(&html[start..]) else {
            continue;
        };
        let Ok(chunk) = serde_json::from_str::<String>(literal) else {
            continue;
        };
        let Ok(Value::Array(table)) = serde_json::from_str::<Value>(&chunk) else {
            continue; // later chunks resolve promises; they aren't the table
        };
        if let Some(conversation) = find_conversation(&table) {
            return Some(conversation);
        }
    }
    None
}

/// The string literal at the start of `s` (with its quotes), or None.
fn js_string_literal(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(&s[..=i]),
            _ => i += 1,
        }
    }
    None
}

fn key_names<'a>(table: &'a [Value], object: &'a Map<String, Value>) -> Vec<&'a str> {
    object
        .keys()
        .filter_map(|k| {
            let index: usize = k.strip_prefix('_')?.parse().ok()?;
            table.get(index)?.as_str()
        })
        .collect()
}

fn find_conversation(table: &[Value]) -> Option<Value> {
    for (index, entry) in table.iter().enumerate() {
        let Value::Object(object) = entry else {
            continue;
        };
        let names = key_names(table, object);
        if names.contains(&"mapping") && names.contains(&"current_node") {
            let mut steps = 0;
            let decoded = decode(table, index as i64, 0, &mut steps)?;
            if decoded.get("mapping").is_some_and(Value::is_object) {
                return Some(decoded);
            }
        }
    }
    None
}

/// Decode the entry `reference` points at (or the special value it stands for).
fn decode(table: &[Value], reference: i64, depth: usize, steps: &mut usize) -> Option<Value> {
    *steps += 1;
    if depth > MAX_DEPTH || *steps > MAX_STEPS {
        return None;
    }
    if reference < 0 {
        // -1 is `undefined`; the others (null, NaN, ±Infinity, -0) are read
        // as null: JSON has no better word for them.
        return Some(Value::Null);
    }
    match table.get(reference as usize)? {
        Value::Object(object) => {
            let mut out = Map::with_capacity(object.len());
            for (key, value) in object {
                let name = match key.strip_prefix('_').and_then(|n| n.parse::<usize>().ok()) {
                    Some(i) => table.get(i)?.as_str()?.to_string(),
                    None => key.clone(),
                };
                if value.as_i64() == Some(-1) {
                    continue; // undefined: the key isn't there
                }
                out.insert(name, decode(table, value.as_i64()?, depth + 1, steps)?);
            }
            Some(Value::Object(out))
        }
        Value::Array(items) => {
            // A tagged array (`["D", ms]` a date, `["P", id]` a promise, …)
            // isn't a list of references.
            if items.first().is_some_and(Value::is_string) {
                return Some(match items.as_slice() {
                    [Value::String(tag), value, ..] if tag == "D" => value.clone(),
                    _ => Value::Null,
                });
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(decode(table, item.as_i64()?, depth + 1, steps)?);
            }
            Some(Value::Array(out))
        }
        primitive => Some(primitive.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Flatten `value` into the stream's table; returns its index (or -5 for null).
    fn encode(value: &Value, table: &mut Vec<Value>) -> i64 {
        match value {
            Value::Null => -5,
            Value::Object(map) => {
                let index = table.len();
                table.push(Value::Null);
                let mut out = Map::new();
                for (key, item) in map {
                    let key_index = table.len();
                    table.push(json!(key));
                    let item_index = encode(item, table);
                    out.insert(format!("_{key_index}"), json!(item_index));
                }
                table[index] = Value::Object(out);
                index as i64
            }
            Value::Array(items) => {
                let index = table.len();
                table.push(Value::Null);
                let refs: Vec<Value> = items.iter().map(|i| json!(encode(i, table))).collect();
                table[index] = Value::Array(refs);
                index as i64
            }
            other => {
                table.push(other.clone());
                (table.len() - 1) as i64
            }
        }
    }

    /// A page the way ChatGPT serves it: the conversation deep inside route
    /// data, in one enqueue script, and a second script resolving a promise.
    fn page(conversation: &Value) -> String {
        let root = json!({
            "loaderData": {
                "root": {"dd": {"traceId": "t"}, "flags": [true, false, null]},
                "routes/share.$shareId.($action)": {
                    "sharedConversationId": "abc",
                    "serverResponse": {"type": "data", "data": conversation},
                    "meta": {"pageTitle": "ChatGPT - Example"}
                }
            },
            "actionData": null,
            "errors": null
        });
        let mut table = Vec::new();
        encode(&root, &mut table);
        let chunk = serde_json::to_string(&Value::Array(table)).unwrap();
        let literal = serde_json::to_string(&chunk).unwrap();
        format!(
            "<html><head><title>ChatGPT - Example</title></head><body><script>window.__reactRouterContext = {{}}; \
             window.__reactRouterContext.streamController = {{}};</script>\
             <script>window.__reactRouterContext.streamController.enqueue({literal});</script>\
             <script>window.__reactRouterContext.streamController.enqueue(\"P21:[{{}}]\\n\");</script>\
             </body></html>"
        )
    }

    fn conversation() -> Value {
        json!({
            "title": "Adapt project devices",
            "conversation_id": "conv-share-1",
            "create_time": 1790708402.5,
            "update_time": 1790708415.25,
            "default_model_slug": "some-model",
            "current_node": "n3",
            "safe_urls": ["https://example.com/a"],
            "mapping": {
                "root": {"id": "root", "message": null, "parent": null, "children": ["n1"]},
                "n1": {"id": "n1", "parent": "root", "children": ["n2"],
                    "message": {"id": "m1", "author": {"role": "system"},
                        "metadata": {"is_visually_hidden_from_conversation": true},
                        "content": {"content_type": "text", "parts": ["hidden instructions"]}}},
                "n2": {"id": "n2", "parent": "n1", "children": ["n3"],
                    "message": {"id": "m2", "author": {"role": "user"},
                        "create_time": 1790708403.0,
                        "content": {"content_type": "text",
                                    "parts": ["Please set up the project on my new laptop."]}}},
                "n3": {"id": "n3", "parent": "n2", "children": [],
                    "message": {"id": "m3", "author": {"role": "assistant"},
                        "metadata": {"model_slug": "some-model"},
                        "content": {"content_type": "text",
                                    "parts": ["Sure: install Rust first, then clone the repo."]}}}
            }
        })
    }

    #[test]
    fn a_saved_share_page_gives_back_the_conversation() {
        let html = page(&conversation());
        assert!(looks_like(&html));
        let found = extract(&html).unwrap();
        assert_eq!(found, conversation());
    }

    #[test]
    fn the_conversation_reads_through_the_chatgpt_adapter() {
        let found = extract(&page(&conversation())).unwrap();
        let out = super::super::chatgpt::parse(&json!([found])).unwrap();
        let conv = &out.conversations[0];
        assert_eq!(conv.external_id.as_deref(), Some("conv-share-1"));
        assert_eq!(conv.title.as_deref(), Some("Adapt project devices"));
        let said: Vec<(&str, &str)> = conv
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            said,
            [
                ("user", "Please set up the project on my new laptop."),
                (
                    "assistant",
                    "Sure: install Rust first, then clone the repo."
                ),
            ],
            "the hidden system entry is left out"
        );
    }

    #[test]
    fn other_pages_are_not_share_pages() {
        assert!(!looks_like("<html><body>Just a page</body></html>"));
        assert!(extract("<html><body>Just a page</body></html>").is_none());
        // Enqueue scripts without a conversation in them.
        let html = "<script>window.__reactRouterContext.streamController.enqueue(\"[{\\\"_1\\\":2},\\\"a\\\",\\\"b\\\"]\");</script>";
        assert!(extract(html).is_none());
        // Damaged data is not a panic.
        assert!(extract("streamController.enqueue(\"[{").is_none());
        assert!(extract("streamController.enqueue(\"\\u12").is_none());
    }

    #[test]
    fn special_values_and_typed_entries_decode_without_trouble() {
        // -1 (undefined) leaves the key out; -5 is null; a date entry gives
        // its value; a promise entry is null.
        let table = json!([
            {"_1": 2, "_3": -1, "_4": -5, "_5": 6, "_7": 8},
            "keep", "yes", "gone", "nothing", "when", ["D", 1700000000000i64], "later", ["P", 21]
        ]);
        let Value::Array(table) = table else { panic!() };
        let mut steps = 0;
        let v = decode(&table, 0, 0, &mut steps).unwrap();
        assert_eq!(
            v,
            json!({"keep": "yes", "nothing": null, "when": 1700000000000i64, "later": null})
        );
    }
}
