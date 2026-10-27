//! Optional local-LLM integration (write-up §5.3): embeddings and
//! LLM-assisted unit extraction via Ollama.
//!
//! Strictly opt-in — disabled unless GATHER_OLLAMA_URL is set — and bound to
//! loopback: a non-loopback Ollama URL is refused unless the same explicit
//! GATHER_ALLOW_NON_LOOPBACK override used for the bind address is set,
//! preserving the "zero unauthorized outbound traffic" guarantee.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::{Semaphore, SemaphorePermit};

use super::rules::ExtractedUnit;
use crate::config::Config;

/// Most tokens the model may write for one section's items: room for a dozen
/// units with their evidence, and no more.
const MAX_EXTRACT_TOKENS: u32 = 768;
/// Most tokens for a document digest (a short summary and a few takeaways).
const MAX_DIGEST_TOKENS: u32 = 450;
/// Most tokens for a contradiction judgement (a verdict and a sentence).
const MAX_JUDGE_TOKENS: u32 = 200;

/// Shared by every client in the process: with `one_at_a_time`, the
/// extraction, scan, photo and query paths take turns, so Ollama never has
/// two models busy (and loaded) for Gather at once.
static ONE_AT_A_TIME: Semaphore = Semaphore::const_new(1);

/// Requests answered per model since it was last unloaded, keyed by Ollama
/// address and model. Shared like `ONE_AT_A_TIME`: each worker builds its own
/// client, but they all talk to the same loaded model.
static ANSWERED: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(Mutex::default);

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
    refresh_after: u32,
    /// Longest stretch of a section sent to the reading model at once; 0 sends
    /// whole sections.
    piece_bytes: usize,
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
Rules: one fact per unit, and split a sentence that states two. \
Each statement must be a whole sentence that stands on its own, with every \
pronoun replaced by the thing it refers to, dated where the text gives a date. \
Never write a heading, a fragment, a question or an instruction as a statement. \
Keep the author\'s hedging: \"might\", \"plans to\", \"is considering\" and \
\"decided against\" must survive into the statement, never be stated as fact. \
Take nothing from code, configuration, logs or tables. \
evidence_span MUST be copied verbatim from the input. \
If the text states no facts, answer {\"units\": []}. No commentary.";

/// Worked examples sent ahead of every section, as earlier turns of the chat.
/// Without them the smaller models copy the answer format back with its
/// placeholder words (or answer nothing); with them, `llama3.2:1b` went from
/// finding none of the statements in ordinary notes to about half. They show
/// a hedged claim kept hedged, one sentence split into two statements, a
/// request left out, and a section with nothing to find.
const EXTRACTION_EXAMPLES: [(&str, &str); 3] = [
    (
        "Priya moved the launch to 14 March because the vendor contract slipped. \
We still need legal to sign off. Please bring your laptop.",
        r#"{"units":[{"kind":"event","statement":"Priya moved the launch to 14 March because the vendor contract slipped.","subject":"Priya","objects":[{"name":"launch","relation":"moved"}],"evidence_span":"Priya moved the launch to 14 March because the vendor contract slipped.","confidence":0.9},{"kind":"claim","statement":"The launch still needs sign-off from legal.","subject":"launch","objects":[{"name":"legal","relation":"needs_signoff_from"}],"evidence_span":"We still need legal to sign off.","confidence":0.8}]}"#,
    ),
    (
        "I think we should switch to Postgres at some point. My rent is 1,150 a month and \
the lease ends in June 2027. Great talking to you!",
        r#"{"units":[{"kind":"preference","statement":"The author thinks they should switch to Postgres at some point.","subject":"the author","objects":[{"name":"Postgres","relation":"may_switch_to"}],"evidence_span":"I think we should switch to Postgres at some point.","confidence":0.6},{"kind":"fact","statement":"The author's rent is 1,150 a month.","subject":"the author","objects":[],"evidence_span":"My rent is 1,150 a month","confidence":0.9},{"kind":"fact","statement":"The lease ends in June 2027.","subject":"the lease","objects":[],"evidence_span":"the lease ends in June 2027","confidence":0.9}]}"#,
    ),
    (
        "Thanks again for the great meeting! Talk soon. Can you send me the slides?",
        r#"{"units":[]}"#,
    ),
];

const DIGEST_SYSTEM_PROMPT: &str = "You summarize a document for its owner, using ONLY the \
text provided. Respond with JSON only: {\"summary\": \"two or three plain sentences saying what \
the document is and what it concludes\", \"takeaways\": [\"a decision, conclusion, deadline or \
action, in one sentence\"], \"open_questions\": [\"something the document leaves unresolved\"]}. \
At most 5 takeaways and 3 open questions. Do not add facts that are not in the text. No commentary.";

/// A model's reading of a document: reworded, not yet checked against it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDigest {
    pub summary: String,
    pub takeaways: Vec<String>,
    pub open_questions: Vec<String>,
}

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
            refresh_after: config.ollama_refresh_after,
            piece_bytes: config.ollama_piece_bytes,
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

    /// Count one answered request for `model` and, every `refresh_after`
    /// requests, ask Ollama to unload it.
    ///
    /// Ollama's runner keeps the state of every prompt it has answered, in
    /// memory, in case one comes back. Gather's prompts are all different, so
    /// that only grows: about 15 MB per section for a 1B model, 54 MB for a
    /// 3B one, without a ceiling, which swamps a small computer. Unloading
    /// drops it; the next request loads the model again, which takes a few
    /// seconds. Ollama lets a request in flight finish before it unloads, so
    /// workers that are not serialized by `turn()` can't cut each other off.
    /// A failed unload is only logged: the model's own keep-alive still ends it.
    async fn refresh_if_due(&self, model: &str) {
        if self.refresh_after == 0 {
            return;
        }
        let due = {
            let mut answered = ANSWERED.lock().unwrap_or_else(PoisonError::into_inner);
            let count = answered
                .entry(format!("{}|{model}", self.base))
                .or_insert(0);
            *count += 1;
            let due = *count >= self.refresh_after;
            if due {
                *count = 0;
            }
            due
        };
        if !due {
            return;
        }
        // A generate request with no prompt and keep_alive 0 unloads a model.
        let unloaded = self
            .http
            .post(format!("{}/api/generate", self.base))
            .json(&json!({ "model": model, "keep_alive": 0 }))
            .send()
            .await
            .and_then(|r| r.error_for_status());
        if let Err(e) = unloaded {
            tracing::warn!(error = %e, model, "could not unload the model; it stays in memory until Ollama's keep-alive ends");
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

    /// `body` with the reply capped at `tokens`. A small model asked for JSON
    /// can loop until its context is full; each call then takes minutes of
    /// processor time for nothing. A reply cut short doesn't parse and is
    /// dropped, which the callers already treat as "no items from the model".
    fn capped(mut body: Value, tokens: u32) -> Value {
        body["options"]["num_predict"] = json!(tokens);
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
        if parsed
            .embeddings
            .iter()
            .any(|vector| vector.len() != 768 || vector.iter().any(|value| !value.is_finite()))
        {
            return Err("embedding model must return 768 finite numbers per input".into());
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
        self.refresh_if_due(model).await;
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
    ///
    /// With `piece_bytes` set, the chunk goes to the model in pieces cut at
    /// the ends of paragraphs and sentences; a piece that fails is skipped, so
    /// the rest of the chunk is still read, and the offsets of what is found
    /// still point into the whole chunk.
    pub async fn extract(&self, chunk: &str) -> Result<Vec<ExtractedUnit>, String> {
        self.chat_model()?;
        let mut units: Vec<ExtractedUnit> = Vec::new();
        let (mut answered, mut failed) = (0usize, 0usize);
        let mut last_error = String::new();
        for (offset, piece) in split_pieces(chunk, self.piece_bytes) {
            match self.extract_piece(piece).await {
                Ok(found) => {
                    answered += 1;
                    for mut unit in found {
                        // The same statement from two pieces (a sentence cut
                        // short, a repeated line) is one statement.
                        if units.iter().any(|u| u.statement == unit.statement) {
                            continue;
                        }
                        unit.char_start += offset;
                        unit.char_end += offset;
                        units.push(unit);
                    }
                }
                Err(e) => {
                    failed += 1;
                    last_error = e;
                }
            }
        }
        if answered == 0 && failed > 0 {
            return Err(last_error);
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                answered,
                error = %last_error,
                "llm extraction failed on part of a section; the rest was read"
            );
        }
        Ok(units)
    }

    /// One request: a piece of a section (or all of it).
    async fn extract_piece(&self, chunk: &str) -> Result<Vec<ExtractedUnit>, String> {
        let body = Self::capped(
            self.body(
                json!({
                    "model": self.chat_model()?,
                    "stream": false,
                    "format": "json",
                    "messages": extraction_messages(chunk),
                }),
                true,
            ),
            MAX_EXTRACT_TOKENS,
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
        self.refresh_if_due(self.chat_model()?).await;
        let content = body
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or("ollama chat response missing message.content")?;
        // A reply that ran into the token limit is cut off mid-way, which a
        // small model's long list of statements often does: keep the ones it
        // finished rather than losing them all.
        let parsed: Value = match serde_json::from_str(content) {
            Ok(parsed) => parsed,
            Err(e) => salvage_units(content)
                .ok_or_else(|| format!("model returned non-JSON content: {e}"))?,
        };

        Ok(parse_llm_units(&parsed, chunk))
    }

    /// Reword a document's key sentences into a summary, takeaways and open
    /// questions. The caller checks the answer against the text.
    pub async fn digest(&self, material: &str) -> Result<ModelDigest, String> {
        let body = Self::capped(
            self.body(
                json!({
                    "model": self.chat_model()?,
                    "stream": false,
                    "format": "json",
                    "messages": [
                        { "role": "system", "content": DIGEST_SYSTEM_PROMPT },
                        { "role": "user", "content": material },
                    ],
                }),
                true,
            ),
            MAX_DIGEST_TOKENS,
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
        self.refresh_if_due(self.chat_model()?).await;
        let content = body
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or("ollama chat response missing message.content")?;
        let parsed: Value = serde_json::from_str(content)
            .map_err(|e| format!("model returned non-JSON content: {e}"))?;
        parse_model_digest(&parsed)
    }

    /// Contradiction judge (write-up §6.2): asks the local model whether two
    /// statements conflict. Used by the scanner only on pairs a structural
    /// rule already flagged, so call volume stays small.
    pub async fn judge(&self, statement_a: &str, statement_b: &str) -> Result<Judgement, String> {
        let body = Self::capped(
            self.body(
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
            ),
            MAX_JUDGE_TOKENS,
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
        self.refresh_if_due(self.chat_model()?).await;
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

/// Read a model's digest answer, keeping what is well-formed and a sensible
/// size: a summary of 20-900 characters, up to 5 takeaways and 3 open
/// questions of 10-300 characters each.
pub(crate) fn parse_model_digest(parsed: &Value) -> Result<ModelDigest, String> {
    let text = |v: &Value, min: usize, max: usize| -> Option<String> {
        let t = v.as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
        (min..=max).contains(&t.chars().count()).then_some(t)
    };
    let list = |key: &str, limit: usize| -> Vec<String> {
        parsed
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| text(v, 10, 300))
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default()
    };
    let summary = parsed
        .get("summary")
        .and_then(|v| text(v, 20, 900))
        .unwrap_or_default();
    let takeaways = list("takeaways", 5);
    let open_questions = list("open_questions", 3);
    if summary.is_empty() && takeaways.is_empty() && open_questions.is_empty() {
        return Err("model digest had nothing usable".to_string());
    }
    Ok(ModelDigest {
        summary,
        takeaways,
        open_questions,
    })
}

/// The chat for one extraction request: the instructions, the worked
/// examples as earlier turns, then the text to read.
fn extraction_messages(chunk: &str) -> Value {
    let mut messages = vec![json!({ "role": "system", "content": EXTRACTION_SYSTEM_PROMPT })];
    for (text, answer) in EXTRACTION_EXAMPLES {
        messages.push(json!({ "role": "user", "content": text }));
        messages.push(json!({ "role": "assistant", "content": answer }));
    }
    messages.push(json!({ "role": "user", "content": chunk }));
    Value::Array(messages)
}

/// The finished unit objects of a reply that was cut off before it closed,
/// as the `{"units": [...]}` it would have been. None when there are none.
fn salvage_units(content: &str) -> Option<Value> {
    let list = content.find("\"units\"")?;
    let rest = &content[list..];
    let open = rest.find('[')?;
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    let mut object_start = None;
    let mut units = Vec::new();
    for (i, byte) in rest.bytes().enumerate().skip(open + 1) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    object_start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(start) = object_start.take() {
                        if let Ok(unit) = serde_json::from_str::<Value>(&rest[start..=i]) {
                            units.push(unit);
                        }
                    }
                }
            }
            b']' if depth == 0 => break,
            _ => {}
        }
    }
    (!units.is_empty()).then(|| json!({ "units": units }))
}

/// Where the last sentence in `window` ends (after the space or line break
/// that follows its full stop, question mark or exclamation mark).
fn last_sentence_end(window: &str) -> Option<usize> {
    let bytes = window.as_bytes();
    (0..bytes.len().saturating_sub(1))
        .rev()
        .find(|&i| matches!(bytes[i], b'.' | b'?' | b'!') && bytes[i + 1].is_ascii_whitespace())
        .map(|i| i + 2)
}

/// `text` in pieces of at most `max` bytes, each with its byte offset into
/// `text`, cut at the last paragraph end, sentence end, line end or space in
/// reach (else at a character boundary). Pieces with nothing in them, such as
/// the blanks left where code or tables were taken out, are left out. `max` 0,
/// or text that already fits, gives the whole text as one piece.
fn split_pieces(text: &str, max: usize) -> Vec<(usize, &str)> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    if max == 0 || text.len() <= max {
        return vec![(0, text)];
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            let window = &text[start..end];
            let cut = window
                .rfind("\n\n")
                .map(|i| i + 2)
                .or_else(|| last_sentence_end(window))
                .or_else(|| window.rfind('\n').map(|i| i + 1))
                .or_else(|| window.rfind(' ').map(|i| i + 1))
                .filter(|&cut| cut > 0);
            if let Some(cut) = cut {
                end = start + cut;
            }
        }
        // A character wider than `max` still has to go somewhere.
        if end == start {
            end = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
        let piece = &text[start..end];
        if !piece.trim().is_empty() {
            pieces.push((start, piece));
        }
        start = end;
    }
    pieces
}

const VALID_KINDS: &[(&str, &str)] = &[
    ("fact", "fact"),
    ("claim", "claim"),
    ("decision", "decision"),
    ("preference", "preference"),
    ("event", "event"),
];

/// Whether the sentence that runs on from `end` in `chunk` is a question.
fn ends_in_question(chunk: &str, end: usize) -> bool {
    chunk[end..]
        .chars()
        .find(|c| matches!(c, '.' | '!' | '?' | '\n'))
        == Some('?')
}

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
        // A model writes its own prose, so unlike a copied sentence it can
        // come back as a heading, a fragment or half a sentence. The rules
        // never see this check.
        let Some(statement) = item
            .get("statement")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| crate::extract::worth::structural_ok(s))
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
        // A question states nothing, even when a model rewords it as a
        // statement ("Is the budget $75?" -> "The budget is $75").
        if evidence.ends_with('?') || ends_in_question(chunk, start + evidence.len()) {
            continue;
        }
        let confidence = item
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|c| c as f32)
            // A model that gives no figure isn't saying it is unsure.
            .unwrap_or(0.7)
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
    fn replies_are_capped_without_losing_the_memory_settings() {
        let mut c = config_with_ollama("http://127.0.0.1:11434", false);
        c.ollama_num_ctx = Some(2048);
        let client = OllamaClient::from_config(&c).unwrap().unwrap();
        let capped = OllamaClient::capped(client.body(json!({ "model": "m" }), true), 768);
        assert_eq!(capped["options"]["num_predict"], 768);
        assert_eq!(capped["options"]["num_ctx"], 2048, "kept alongside the cap");
        // Without a context setting the cap still stands on its own.
        let plain = OllamaClient::capped(json!({ "model": "m" }), 200);
        assert_eq!(plain["options"]["num_predict"], 200);
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

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, Value)>>>;

    /// A stand-in Ollama that answers chat, embed and generate requests and
    /// records each one in order. `/api/generate` answers `unload_status`.
    async fn fake_ollama(unload_status: axum::http::StatusCode) -> (String, Seen) {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Json, Router};

        let seen = Seen::default();
        let app = Router::new()
            .route(
                "/api/chat",
                post(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    s.lock().unwrap().push(("chat", b));
                    Json(json!({ "message": { "content":
                        r#"{"contradicts": false, "confidence": 0.5, "why": "x"}"# } }))
                }),
            )
            .route(
                "/api/generate",
                post(
                    move |State(s): State<Seen>, Json(b): Json<Value>| async move {
                        s.lock().unwrap().push(("generate", b));
                        (unload_status, Json(json!({ "done": true })))
                    },
                ),
            )
            .route(
                "/api/embed",
                post(|State(s): State<Seen>, Json(b): Json<Value>| async move {
                    let n = b["input"].as_array().map_or(1, Vec::len);
                    s.lock().unwrap().push(("embed", b));
                    Json(json!({ "embeddings": vec![vec![0.0f32; 768]; n] }))
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    fn client_for(url: &str, refresh_after: u32) -> OllamaClient {
        let mut c = config_with_ollama(url, false);
        c.ollama_model = Some("chat-model".to_string());
        c.ollama_refresh_after = refresh_after;
        OllamaClient::from_config(&c).unwrap().unwrap()
    }

    #[tokio::test]
    async fn the_chat_model_is_unloaded_after_the_configured_number_of_requests() {
        let (url, seen) = fake_ollama(axum::http::StatusCode::OK).await;
        let client = client_for(&url, 3);
        for _ in 0..7 {
            client.judge("a", "b").await.unwrap();
        }
        let seen = seen.lock().unwrap();
        let kinds: Vec<_> = seen.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            ["chat", "chat", "chat", "generate", "chat", "chat", "chat", "generate", "chat"],
            "an unload follows every third answer, before the next request"
        );
        // An empty generate request with keep_alive 0 is how Ollama is asked
        // to drop a model.
        assert_eq!(seen[3].1, json!({ "model": "chat-model", "keep_alive": 0 }));
    }

    #[tokio::test]
    async fn zero_never_unloads() {
        let (url, seen) = fake_ollama(axum::http::StatusCode::OK).await;
        let client = client_for(&url, 0);
        for _ in 0..5 {
            client.judge("a", "b").await.unwrap();
        }
        assert!(seen.lock().unwrap().iter().all(|(k, _)| *k == "chat"));
    }

    #[tokio::test]
    async fn embedding_requests_do_not_count_towards_an_unload() {
        // The embedding model keeps no prompt memory, and unloading it would
        // only make the next search wait for a reload.
        let (url, seen) = fake_ollama(axum::http::StatusCode::OK).await;
        let client = client_for(&url, 2);
        for _ in 0..5 {
            client.embed(&["text".to_string()]).await.unwrap();
        }
        assert!(seen.lock().unwrap().iter().all(|(k, _)| *k == "embed"));
    }

    #[tokio::test]
    async fn a_failed_unload_does_not_fail_the_request() {
        let (url, seen) = fake_ollama(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let client = client_for(&url, 1);
        client.judge("a", "b").await.unwrap();
        client.judge("a", "b").await.unwrap();
        let kinds: Vec<_> = seen.lock().unwrap().iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, ["chat", "generate", "chat", "generate"]);
    }

    #[test]
    fn pieces_tile_the_text_and_stay_within_the_limit() {
        let text = "First paragraph, two sentences. Here is the second one.\n\n\
                    Another paragraph follows, with a longer sentence that runs on for a while. \
                    And a short one! Is this a question? Yes it is.\nA line break, then more words \
                    without any full stop at all until the very end of the text";
        for max in [20, 40, 60, 100, 1000] {
            let pieces = split_pieces(text, max);
            let mut rebuilt = String::new();
            for (offset, piece) in &pieces {
                assert_eq!(&text[*offset..*offset + piece.len()], *piece, "max {max}");
                assert!(
                    piece.len() <= max.max(text.len().min(max)),
                    "max {max}: {piece:?}"
                );
                rebuilt.push_str(piece);
            }
            assert_eq!(rebuilt, text, "max {max}: nothing lost, nothing repeated");
        }
    }

    #[test]
    fn pieces_end_where_a_sentence_ends() {
        let text = "Alpha beta gamma. Delta epsilon zeta. Eta theta iota. Kappa lambda mu.";
        let pieces = split_pieces(text, 40);
        assert_eq!(
            pieces,
            vec![
                (0, "Alpha beta gamma. Delta epsilon zeta. "),
                (38, "Eta theta iota. Kappa lambda mu.")
            ]
        );
        let paragraphs = "One sentence here. Another one.\n\nSecond paragraph starts. It goes on.";
        assert_eq!(
            split_pieces(paragraphs, 40)[0].1,
            "One sentence here. Another one.\n\n"
        );
    }

    #[test]
    fn pieces_handle_unbroken_runs_and_wide_characters() {
        // No break to cut at: cut at a character boundary, never inside one.
        let run = "é".repeat(50);
        for (offset, piece) in split_pieces(&run, 7) {
            assert!(piece.len() <= 7 && piece.chars().all(|c| c == 'é'));
            assert!(run.is_char_boundary(offset));
        }
        // A limit smaller than one character still moves forward.
        let pieces = split_pieces("日本語", 1);
        assert_eq!(pieces.iter().map(|(_, p)| *p).collect::<String>(), "日本語");
    }

    #[test]
    fn pieces_skip_blanks_and_keep_short_or_unlimited_text_whole() {
        assert!(split_pieces("  \n\n \t", 50).is_empty());
        assert!(split_pieces("", 50).is_empty());
        assert_eq!(split_pieces("short text", 50), vec![(0, "short text")]);
        let long = "word ".repeat(100);
        assert_eq!(
            split_pieces(&long, 0),
            vec![(0, long.as_str())],
            "0 sends it whole"
        );
        // A stretch blanked out (code or a table taken from the section) sends
        // nothing: no piece is only spaces.
        let blanked = format!("Real prose here. {}Real prose again.", " ".repeat(300));
        let pieces = split_pieces(&blanked, 60);
        assert!(pieces.iter().all(|(_, p)| !p.trim().is_empty()));
        assert!(pieces.iter().any(|(_, p)| p.contains("again")));
    }

    #[test]
    fn a_reply_cut_off_mid_way_keeps_the_units_it_finished() {
        let cut = r#"{"units":[{"kind":"fact","statement":"A { brace } in text, and a \"quote\".","evidence_span":"x"},{"kind":"fact","statement":"Second.","evidence_span":"y"},{"kind":"fact","statement":"Third is cut of"#;
        let salvaged = salvage_units(cut).unwrap();
        let units = salvaged["units"].as_array().unwrap();
        assert_eq!(units.len(), 2, "the unfinished third is dropped");
        assert_eq!(
            units[0]["statement"],
            "A { brace } in text, and a \"quote\"."
        );
        assert_eq!(units[1]["statement"], "Second.");
        // Nothing finished, or not a reply at all: nothing to keep.
        assert!(salvage_units(r#"{"units":[{"kind":"fact","statement":"cut"#).is_none());
        assert!(salvage_units("the model rambled").is_none());
        assert!(salvage_units(r#"{"other": [1, 2"#).is_none());
        // A closed list is whatever it holds.
        assert_eq!(
            salvage_units(r#"{"units":[{"a":1}],"trailing": tru"#).unwrap()["units"],
            json!([{ "a": 1 }])
        );
    }

    #[test]
    fn the_worked_examples_are_themselves_valid_answers() {
        // Each answer must pass the checks every reply does (verbatim
        // evidence, whole-sentence statements), or the model is taught to
        // write what would be thrown away.
        for (text, answer) in EXTRACTION_EXAMPLES {
            let parsed: Value = serde_json::from_str(answer).unwrap();
            let written = parsed["units"].as_array().unwrap().len();
            assert_eq!(parse_llm_units(&parsed, text).len(), written, "{text}");
        }
        let messages = extraction_messages("the section");
        let messages = messages.as_array().unwrap();
        assert_eq!(messages.len(), 2 + 2 * EXTRACTION_EXAMPLES.len());
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages.last().unwrap()["content"], "the section");
        assert_eq!(messages.last().unwrap()["role"], "user");
    }

    type Requests = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

    /// A stand-in chat endpoint: records each request, and answers the nth
    /// (counting from 0) with whatever `reply(n, request)` says.
    async fn fake_chat<F>(reply: F) -> (String, Requests)
    where
        F: Fn(usize, &Value) -> (axum::http::StatusCode, String) + Send + Sync + 'static,
    {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Json, Router};

        let requests = Requests::default();
        let reply = std::sync::Arc::new(reply);
        let app = Router::new()
            .route(
                "/api/chat",
                post(
                    move |State(log): State<Requests>, Json(body): Json<Value>| {
                        let reply = reply.clone();
                        async move {
                            let n = {
                                let mut log = log.lock().unwrap();
                                log.push(body.clone());
                                log.len() - 1
                            };
                            let (status, content) = reply(n, &body);
                            (status, Json(json!({ "message": { "content": content } })))
                        }
                    },
                ),
            )
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, requests)
    }

    fn reading_client(url: &str, piece_bytes: usize) -> OllamaClient {
        let mut c = config_with_ollama(url, false);
        c.ollama_model = Some("chat-model".to_string());
        c.ollama_piece_bytes = piece_bytes;
        OllamaClient::from_config(&c).unwrap().unwrap()
    }

    /// The last line of the text the model was asked to read, as a unit.
    fn quote_the_first_sentence(request: &Value) -> String {
        let text = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        let sentence = text.split_inclusive(". ").next().unwrap().trim();
        json!({ "units": [{
            "kind": "fact", "statement": sentence, "evidence_span": sentence,
        }] })
        .to_string()
    }

    const SECTION: &str = "The first sentence is here. The second sentence follows it. \
        The third sentence is a little longer than the others. A fourth sentence closes it.";

    #[tokio::test]
    async fn a_long_section_is_read_in_pieces_and_offsets_point_into_the_whole() {
        let (url, requests) =
            fake_chat(|_, req| (axum::http::StatusCode::OK, quote_the_first_sentence(req))).await;
        let client = reading_client(&url, 70);
        let units = client.extract(SECTION).await.unwrap();

        let requests = requests.lock().unwrap();
        assert!(requests.len() >= 2, "{} requests", requests.len());
        for request in requests.iter() {
            let messages = request["messages"].as_array().unwrap();
            let piece = messages.last().unwrap()["content"].as_str().unwrap();
            assert!(piece.len() <= 70, "{piece:?}");
            assert_eq!(
                messages.len(),
                2 + 2 * EXTRACTION_EXAMPLES.len(),
                "every request carries the worked examples"
            );
        }
        assert_eq!(units.len(), requests.len(), "one first sentence per piece");
        for unit in &units {
            assert_eq!(
                &SECTION[unit.char_start..unit.char_end],
                unit.statement,
                "the span is the quoted text, located in the whole section"
            );
        }
    }

    #[tokio::test]
    async fn without_a_piece_size_the_whole_section_is_one_request() {
        let (url, requests) =
            fake_chat(|_, req| (axum::http::StatusCode::OK, quote_the_first_sentence(req))).await;
        let units = reading_client(&url, 0).extract(SECTION).await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(units.len(), 1);
    }

    #[tokio::test]
    async fn a_piece_that_fails_does_not_cost_the_others() {
        let (url, requests) = fake_chat(|n, req| {
            if n == 1 {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, String::new())
            } else {
                (axum::http::StatusCode::OK, quote_the_first_sentence(req))
            }
        })
        .await;
        let units = reading_client(&url, 70).extract(SECTION).await.unwrap();
        let asked = requests.lock().unwrap().len();
        assert_eq!(units.len(), asked - 1, "all but the failed piece");

        // When every piece fails, the failure is reported as before.
        let (url, _) =
            fake_chat(|_, _| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, String::new())).await;
        assert!(reading_client(&url, 70).extract(SECTION).await.is_err());
    }

    #[tokio::test]
    async fn a_cut_off_reply_still_yields_its_finished_units() {
        let cut = r#"{"units":[{"kind":"fact","statement":"The first sentence is here.","evidence_span":"The first sentence is here."},{"kind":"fact","statement":"The second sentence follows it.","evidence_span":"The second sentence follows it."},{"kind":"fact","statement":"The third sent"#;
        let (url, _) = fake_chat(move |_, _| (axum::http::StatusCode::OK, cut.to_string())).await;
        let units = reading_client(&url, 0).extract(SECTION).await.unwrap();
        let statements: Vec<_> = units.iter().map(|u| u.statement.as_str()).collect();
        assert_eq!(
            statements,
            [
                "The first sentence is here.",
                "The second sentence follows it."
            ]
        );
    }

    #[tokio::test]
    async fn the_same_statement_from_two_pieces_is_kept_once() {
        // A line repeated through a section (a page footer, a refrain) comes
        // back from every piece that holds it.
        let section = "The totals are final. Filler words here to pad out. \
                       The totals are final. Filler words once more to pad.";
        let (url, requests) =
            fake_chat(|_, req| (axum::http::StatusCode::OK, quote_the_first_sentence(req))).await;
        let units = reading_client(&url, 50).extract(section).await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 4, "four pieces");
        let statements: Vec<_> = units.iter().map(|u| u.statement.as_str()).collect();
        assert_eq!(
            statements,
            [
                "The totals are final.",
                "Filler words here to pad out.",
                "Filler words once more to pad."
            ]
        );
        assert_eq!(units[0].char_start, 0, "the first one is the one kept");
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

    #[test]
    fn a_question_is_not_kept_even_when_the_model_rewords_it() {
        let chunk = "Is the budget $75 per month? The budget is $90 per month.";
        let parsed = serde_json::json!({
            "units": [
                { "kind": "fact", "statement": "The budget is $75 per month",
                  "subject": "budget", "objects": [],
                  "evidence_span": "Is the budget $75 per month", "confidence": 0.9 },
                { "kind": "fact", "statement": "The budget is $90 per month",
                  "subject": "budget", "objects": [],
                  "evidence_span": "The budget is $90 per month", "confidence": 0.9 }
            ]
        });
        let units = parse_llm_units(&parsed, chunk);
        assert_eq!(units.len(), 1, "{units:?}");
        assert!(units[0].statement.contains("$90"));
    }

    #[test]
    fn a_model_fragment_is_not_a_statement() {
        let chunk = "We moved the backup target to Hetzner because the old box died.";
        let parsed = serde_json::json!({
            "units": [
                { "kind": "fact", "statement": "because the old box died",
                  "subject": "backup target", "objects": [],
                  "evidence_span": "because the old box died", "confidence": 0.9 },
                { "kind": "fact", "statement": "The backup target moved to Hetzner",
                  "subject": "backup target", "objects": [],
                  "evidence_span": "moved the backup target to Hetzner", "confidence": 0.9 }
            ]
        });
        let units = parse_llm_units(&parsed, chunk);
        assert_eq!(units.len(), 1, "{units:?}");
        assert!(units[0].statement.starts_with("The backup target"));
    }

    #[test]
    fn a_model_digest_keeps_only_what_is_well_formed() {
        let parsed = serde_json::json!({
            "summary": "A plan to move the nightly backups to Hetzner before the March deadline.",
            "takeaways": [
                "The team chose Hetzner CX22 because the monthly cost is lower.",
                "too short",
                42,
                "The migration must finish before 2026-03-01 or the old contract renews."
            ],
            "open_questions": ["Will the first full backup fit inside the nightly window?"]
        });
        let digest = parse_model_digest(&parsed).unwrap();
        assert!(digest.summary.starts_with("A plan to move"));
        assert_eq!(
            digest.takeaways.len(),
            2,
            "short and non-text items are dropped"
        );
        assert_eq!(digest.open_questions.len(), 1);
        // Too many items are cut to the limits.
        let many = serde_json::json!({
            "takeaways": (0..9).map(|i| format!("Takeaway number {i} is long enough")).collect::<Vec<_>>()
        });
        assert_eq!(parse_model_digest(&many).unwrap().takeaways.len(), 5);
        // Nothing usable is an error, so the plain digest stands.
        assert!(parse_model_digest(&serde_json::json!({})).is_err());
        assert!(parse_model_digest(&serde_json::json!({"summary": "hi"})).is_err());
    }
}
