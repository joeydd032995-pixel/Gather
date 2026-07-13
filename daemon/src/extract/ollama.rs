//! Optional local-LLM integration (write-up §5.3): embeddings and
//! LLM-assisted unit extraction via Ollama.
//!
//! Strictly opt-in — disabled unless GATHER_OLLAMA_URL is set — and bound to
//! loopback: a non-loopback Ollama URL is refused unless the same explicit
//! GATHER_ALLOW_NON_LOOPBACK override used for the bind address is set,
//! preserving the "zero unauthorized outbound traffic" guarantee.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use super::rules::ExtractedUnit;
use crate::config::Config;

pub struct OllamaClient {
    base: String,
    http: reqwest::Client,
    pub model: String,
    pub embed_model: String,
}

const EXTRACTION_SYSTEM_PROMPT: &str = "You extract atomic factual statements from text. \
Respond with JSON only: {\"units\": [{\"kind\": \"fact|claim|decision|preference|event\", \
\"statement\": \"self-contained statement\", \"subject\": \"entity the statement is about\", \
\"objects\": [{\"name\": \"entity\", \"relation\": \"snake_case_relation\"}], \
\"evidence_span\": \"verbatim quote from the text\", \"confidence\": 0.0}]}. \
Statements must be self-contained and dated where possible. \
evidence_span MUST be copied verbatim from the input. No commentary.";

impl OllamaClient {
    /// Build from config. Returns Ok(None) when Ollama is not configured.
    pub fn from_config(config: &Config) -> Result<Option<Self>, String> {
        let Some(url) = config.ollama_url.as_deref().filter(|u| !u.is_empty()) else {
            return Ok(None);
        };
        let parsed: reqwest::Url = url
            .parse()
            .map_err(|e| format!("GATHER_OLLAMA_URL invalid: {e}"))?;
        let host = parsed.host_str().unwrap_or_default();
        let is_loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        if !is_loopback && !config.allow_non_loopback {
            return Err(format!(
                "refusing non-loopback Ollama URL {url} without GATHER_ALLOW_NON_LOOPBACK=true \
                 (Gather is offline/local-only by default)"
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(120))
            .no_proxy() // localhost traffic must never be routed through a proxy
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Some(Self {
            base: url.trim_end_matches('/').to_string(),
            http,
            model: config.ollama_model.clone(),
            embed_model: config.ollama_embed_model.clone(),
        }))
    }

    /// Embed a batch of texts with the local embedding model (768-dim).
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        #[derive(Deserialize)]
        struct EmbedResponse {
            embeddings: Vec<Vec<f32>>,
        }
        let response = self
            .http
            .post(format!("{}/api/embed", self.base))
            .json(&json!({ "model": self.embed_model, "input": texts }))
            .send()
            .await
            .map_err(|e| format!("ollama embed request: {e}"))?
            .error_for_status()
            .map_err(|e| format!("ollama embed status: {e}"))?;
        let parsed: EmbedResponse = response
            .json()
            .await
            .map_err(|e| format!("ollama embed decode: {e}"))?;
        if parsed.embeddings.len() != texts.len() {
            return Err(format!(
                "ollama returned {} embeddings for {} inputs",
                parsed.embeddings.len(),
                texts.len()
            ));
        }
        Ok(parsed.embeddings)
    }

    /// LLM-assisted extraction over one chunk. Anti-hallucination gate: a
    /// unit is kept only if its evidence_span appears verbatim in the chunk;
    /// its char offsets come from that containment check.
    pub async fn extract(&self, chunk: &str) -> Result<Vec<ExtractedUnit>, String> {
        let response = self
            .http
            .post(format!("{}/api/chat", self.base))
            .json(&json!({
                "model": self.model,
                "stream": false,
                "format": "json",
                "messages": [
                    { "role": "system", "content": EXTRACTION_SYSTEM_PROMPT },
                    { "role": "user", "content": chunk },
                ],
            }))
            .send()
            .await
            .map_err(|e| format!("ollama chat request: {e}"))?
            .error_for_status()
            .map_err(|e| format!("ollama chat status: {e}"))?;

        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("ollama chat decode: {e}"))?;
        let content = body
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or("ollama chat response missing message.content")?;
        let parsed: Value = serde_json::from_str(content)
            .map_err(|e| format!("model returned non-JSON content: {e}"))?;

        Ok(parse_llm_units(&parsed, chunk))
    }
}

const VALID_KINDS: &[(&str, &str)] = &[
    ("fact", "fact"),
    ("claim", "claim"),
    ("decision", "decision"),
    ("preference", "preference"),
    ("event", "event"),
];

pub(crate) fn parse_llm_units(parsed: &Value, chunk: &str) -> Vec<ExtractedUnit> {
    let Some(items) = parsed.get("units").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(kind) = item
            .get("kind")
            .and_then(Value::as_str)
            .and_then(|k| VALID_KINDS.iter().find(|(name, _)| *name == k))
            .map(|(_, s)| *s)
        else {
            continue;
        };
        let Some(statement) = item
            .get("statement")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        // Anti-hallucination: evidence must exist verbatim in the source.
        let Some(evidence) = item
            .get("evidence_span")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|e| !e.is_empty())
        else {
            continue;
        };
        let Some(start) = chunk.find(evidence) else {
            continue;
        };
        let confidence = item
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|c| c as f32)
            .unwrap_or(0.5)
            .clamp(0.0, 1.0)
            * 0.9; // LLM units never outrank rule-based hits
        let objects = item
            .get("objects")
            .and_then(Value::as_array)
            .map(|objs| {
                objs.iter()
                    .filter_map(|o| {
                        let name = o.get("name").and_then(Value::as_str)?.trim();
                        let relation = o.get("relation").and_then(Value::as_str)?.trim();
                        if name.is_empty() || relation.is_empty() {
                            None
                        } else {
                            Some((name.to_string(), relation.to_string()))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(ExtractedUnit {
            kind,
            statement: statement.to_string(),
            subject: item
                .get("subject")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            objects,
            char_start: start,
            char_end: start + evidence.len(),
            confidence,
            attrs: json!({ "pattern": "llm" }),
            event_time: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_with_ollama(url: &str, allow: bool) -> Config {
        let mut c = Config::for_tests("postgres://unused".to_string());
        c.ollama_url = Some(url.to_string());
        c.allow_non_loopback = allow;
        c
    }

    #[test]
    fn disabled_when_unset() {
        let c = Config::for_tests("postgres://unused".to_string());
        assert!(OllamaClient::from_config(&c).unwrap().is_none());
    }

    #[test]
    fn loopback_urls_accepted_non_loopback_refused() {
        assert!(
            OllamaClient::from_config(&config_with_ollama("http://127.0.0.1:11434", false))
                .unwrap()
                .is_some()
        );
        assert!(
            OllamaClient::from_config(&config_with_ollama("http://localhost:11434", false))
                .unwrap()
                .is_some()
        );
        assert!(
            OllamaClient::from_config(&config_with_ollama("http://10.0.0.5:11434", false)).is_err()
        );
        assert!(
            OllamaClient::from_config(&config_with_ollama("http://10.0.0.5:11434", true))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn llm_units_require_verbatim_evidence() {
        let chunk = "We decided on Hetzner for backups.";
        let parsed = serde_json::json!({
            "units": [
                { "kind": "decision", "statement": "User decided on Hetzner for backups",
                  "subject": "Me",
                  "objects": [{"name": "Hetzner", "relation": "decided_on"}],
                  "evidence_span": "decided on Hetzner", "confidence": 0.8 },
                { "kind": "fact", "statement": "hallucinated",
                  "evidence_span": "text that is not in the chunk", "confidence": 0.9 },
                { "kind": "not-a-kind", "statement": "bad kind",
                  "evidence_span": "backups", "confidence": 0.9 }
            ]
        });
        let units = parse_llm_units(&parsed, chunk);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "decision");
        assert!((units[0].confidence - 0.72).abs() < 0.001); // 0.8 * 0.9
        assert_eq!(
            &chunk[units[0].char_start..units[0].char_end],
            "decided on Hetzner"
        );
    }
}
