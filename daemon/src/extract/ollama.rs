//! Optional local-LLM integration (write-up §5.3): embeddings and
//! LLM-assisted unit extraction via Ollama.
//!
//! Strictly opt-in — disabled unless GATHER_OLLAMA_URL is set — and bound to
//! loopback: a non-loopback Ollama URL is refused unless the same explicit
//! GATHER_ALLOW_NON_LOOPBACK override used for the bind address is set,
//! preserving the "zero unauthorized outbound traffic" guarantee.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::{Semaphore, SemaphorePermit};

use super::rules::ExtractedUnit;
use crate::config::Config;

/// Shared by every client in the process: with `one_at_a_time`, the
/// extraction, scan, photo and query paths take turns, so Ollama never has
/// two models busy (and loaded) for Gather at once.
static ONE_AT_A_TIME: Semaphore = Semaphore::const_new(1);

pub struct OllamaClient {
    base: String,
    http: reqwest::Client,
    /// Chat model for extraction and the contradiction judge; None keeps
    /// this client to embeddings (and captions, with a vision model).
    pub model: Option<String>,
    pub embed_model: String,
    /// Vision model for photo captions; None when not configured.
    pub vision_model: Option<String>,
    keep_alive: Option<String>,
    num_ctx: Option<u32>,
    one_at_a_time: bool,
}

/// Why a caption could not be produced.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptionError {
    /// The model is unreachable, overloaded or erroring: retry later.
    Unavailable(String),
    /// The model answered but can't caption this image: don't retry it.
    Rejected(String),
}

impl std::fmt::Display for CaptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptionError::Unavailable(m) | CaptionError::Rejected(m) => f.write_str(m),
        }
    }
}

const CAPTION_PROMPT: &str = "Describe this photo in one factual sentence: the main subject, \
the setting, and any visible text. No speculation.";

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
            vision_model: config.ollama_vision_model.clone(),
            keep_alive: config.ollama_keep_alive.clone(),
            num_ctx: config.ollama_num_ctx,
            one_at_a_time: config.ollama_one_at_a_time,
        }))
    }

    /// Wait for this client's turn when requests are serialized; hold the
    /// returned permit until the response has been read.
    async fn turn(&self) -> Option<SemaphorePermit<'static>> {
        if self.one_at_a_time {
            ONE_AT_A_TIME.acquire().await.ok()
        } else {
            None
        }
    }

    /// A request body with the configured memory settings added.
    /// `with_context` is false for embeddings, whose context is the model's.
    fn body(&self, request: Value, with_context: bool) -> Value {
        let mut body = request;
        if let Some(keep_alive) = &self.keep_alive {
            body["keep_alive"] = json!(keep_alive);
        }
        if let (true, Some(num_ctx)) = (with_context, self.num_ctx) {
            let mut options = Map::new();
            options.insert("num_ctx".to_string(), json!(num_ctx));
            body["options"] = Value::Object(options);
        }
        body
    }

    fn chat_model(&self) -> Result<&str, String> {
        self.model
            .as_deref()
            .ok_or_else(|| "no chat model configured (GATHER_OLLAMA_MODEL)".to_string())
    }

    /// Embed a batch of texts with the local embedding model (768-dim).
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        #[derive(Deserialize)]
        struct EmbedResponse {
            embeddings: Vec<Vec<f32>>,
        }
        let _turn = self.turn().await;
        let response = self
            .http
            .post(format!("{}/api/embed", self.base))
            .json(&self.body(json!({ "model": self.embed_model, "input": texts }), false))
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

    /// One-sentence caption of a photo from the local vision model.
    ///
    /// Errors are split so callers can tell a model that is down (retry the
    /// whole batch later) from one that rejected this particular image (skip
    /// it, don't block the queue behind it).
    pub async fn caption(&self, image_bytes: &[u8]) -> Result<String, CaptionError> {
        use base64::Engine;
        let model = self
            .vision_model
            .as_deref()
            .ok_or_else(|| CaptionError::Unavailable("no vision model configured".to_string()))?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(image_bytes);
        let body = self.body(
            json!({
                "model": model,
                "prompt": CAPTION_PROMPT,
                "images": [encoded],
                "stream": false,
            }),
            true,
        );
        drop(encoded);
        let _turn = self.turn().await;
        let response = self
            .http
            .post(format!("{}/api/generate", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|e| CaptionError::Unavailable(format!("ollama caption request: {e}")))?;
        let status = response.status();
        if status.is_client_error() {
            // Bad input for this model (e.g. an image format it can't read).
            return Err(CaptionError::Rejected(format!(
                "ollama caption status: {status}"
            )));
        }
        if !status.is_success() {
            return Err(CaptionError::Unavailable(format!(
                "ollama caption status: {status}"
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| CaptionError::Rejected(format!("ollama caption decode: {e}")))?;
        let caption = body
            .get("response")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                CaptionError::Rejected("ollama caption response missing text".to_string())
            })?;
        Ok(caption.to_string())
    }

    /// LLM-assisted extraction over one chunk. Anti-hallucination gate: a
    /// unit is kept only if its evidence_span appears verbatim in the chunk;
    /// its char offsets come from that containment check.
    pub async fn extract(&self, chunk: &str) -> Result<Vec<ExtractedUnit>, String> {
        let body = self.body(
            json!({
                "model": self.chat_model()?,
                "stream": false,
                "format": "json",
                "messages": [
                    { "role": "system", "content": EXTRACTION_SYSTEM_PROMPT },
                    { "role": "user", "content": chunk },
                ],
            }),
            true,
        );
        let _turn = self.turn().await;
        let response = self
            .http
            .post(format!("{}/api/chat", self.base))
            .json(&body)
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

    /// Contradiction judge (write-up §6.2): asks the local model whether two
    /// statements conflict. Used by the scanner only on pairs a structural
    /// rule already flagged, so call volume stays small.
    pub async fn judge(&self, statement_a: &str, statement_b: &str) -> Result<Judgement, String> {
        let body = self.body(
            json!({
                "model": self.chat_model()?,
                "stream": false,
                "format": "json",
                "messages": [
                    { "role": "system", "content": JUDGE_SYSTEM_PROMPT },
                    { "role": "user",
                      "content": format!("A: {statement_a}\nB: {statement_b}") },
                ],
            }),
            true,
        );
        let _turn = self.turn().await;
        let response = self
            .http
            .post(format!("{}/api/chat", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ollama judge request: {e}"))?
            .error_for_status()
            .map_err(|e| format!("ollama judge status: {e}"))?;
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("ollama judge decode: {e}"))?;
        let content = body
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or("ollama judge response missing message.content")?;
        let parsed: Value = serde_json::from_str(content)
            .map_err(|e| format!("judge returned non-JSON content: {e}"))?;
        parse_judgement(&parsed)
    }
}

const JUDGE_SYSTEM_PROMPT: &str = "You judge whether two statements contradict each other. \
Two statements contradict when they cannot both be true at the same time. \
Respond with JSON only: {\"contradicts\": true|false, \"confidence\": 0.0-1.0, \
\"why\": \"one short sentence\"}. No commentary.";

pub struct Judgement {
    pub contradicts: bool,
    pub confidence: f32,
    pub why: String,
}

pub(crate) fn parse_judgement(parsed: &Value) -> Result<Judgement, String> {
    let contradicts = parsed
        .get("contradicts")
        .and_then(Value::as_bool)
        .ok_or("judge output missing boolean 'contradicts'")?;
    let confidence = parsed
        .get("confidence")
        .and_then(Value::as_f64)
        .map(|c| (c as f32).clamp(0.0, 1.0))
        .unwrap_or(0.5);
    let why = parsed
        .get("why")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(Judgement {
        contradicts,
        confidence,
        why,
    })
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
    fn memory_settings_are_added_to_requests() {
        let mut c = config_with_ollama("http://127.0.0.1:11434", false);
        c.ollama_keep_alive = Some("1m".to_string());
        c.ollama_num_ctx = Some(2048);
        let client = OllamaClient::from_config(&c).unwrap().unwrap();
        let chat = client.body(json!({ "model": "m" }), true);
        assert_eq!(chat["keep_alive"], "1m");
        assert_eq!(chat["options"]["num_ctx"], 2048);
        let embed = client.body(json!({ "model": "e" }), false);
        assert_eq!(embed["keep_alive"], "1m");
        assert!(embed.get("options").is_none());

        let plain = OllamaClient::from_config(&config_with_ollama("http://127.0.0.1:11434", false))
            .unwrap()
            .unwrap();
        assert_eq!(
            plain.body(json!({ "model": "m" }), true),
            json!({ "model": "m" })
        );
    }

    #[tokio::test]
    async fn chat_features_need_a_chat_model() {
        let mut c = config_with_ollama("http://127.0.0.1:11434", false);
        c.ollama_model = None;
        let client = OllamaClient::from_config(&c).unwrap().unwrap();
        // Refused before any request is made.
        assert!(client.extract("text").await.is_err());
        assert!(client.judge("a", "b").await.is_err());
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
