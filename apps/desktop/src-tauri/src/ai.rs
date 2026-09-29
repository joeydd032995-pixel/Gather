//! The local AI model (Ollama) Gather uses, chosen in Settings.
//!
//! The choice is saved in the app data folder and handed to the daemon as
//! environment variables when the app starts it (see `runtime`). Until the
//! user saves a choice, whatever the app's own environment says applies, so
//! a setup made with environment variables keeps working.
//!
//! Ollama runs on this machine: like the daemon, only a loopback address is
//! accepted, and the connection test is one plain HTTP request to it.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::memory::Profile;

const FILE: &str = "ai-settings.json";
pub const DEFAULT_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";
/// The daemon's reading model when GATHER_OLLAMA_MODEL isn't set, on the
/// standard memory profile (the low profile has none).
const DAEMON_DEFAULT_CHAT_MODEL: &str = "llama3.2:3b";
/// Vector size of the database's embedding columns.
pub const EMBED_DIMENSIONS: usize = 768;
/// Enough for Ollama's model list; anything bigger isn't Ollama.
const MAX_RESPONSE: u64 = 1024 * 1024;

/// How hard Gather lets the AI model work while it reads files. The model
/// uses every core it can get, so on a small computer a big import would
/// otherwise keep the processor near 100 % for hours; the daemon rests in
/// between to hold it to this share of the time.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Speed {
    /// About 30 % of the time: leaves the computer usable.
    Gentle,
    /// About 60 %.
    Balanced,
    /// No rests between files; the fastest, and the hottest.
    Full,
}

impl Speed {
    /// The value the daemon reads from GATHER_EXTRACTION_AI_DUTY_PERCENT.
    pub fn duty_percent(self) -> u8 {
        match self {
            Speed::Gentle => 30,
            Speed::Balanced => 60,
            Speed::Full => 100,
        }
    }

    /// What the daemon uses when nothing is chosen.
    pub fn for_profile(profile: Profile) -> Speed {
        match profile {
            Profile::Low => Speed::Gentle,
            Profile::Standard => Speed::Balanced,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AiSettings {
    /// Use Ollama at all.
    pub enabled: bool,
    /// Where Ollama listens, e.g. http://127.0.0.1:11434.
    pub url: String,
    /// Model that reads files into items (e.g. llama3.2:1b); empty to use
    /// Ollama for search only.
    pub chat_model: String,
    /// Model for search by meaning (768-dimension vectors).
    pub embed_model: String,
    /// How hard the reading model may work; None until chosen, when the
    /// daemon's default for this computer applies (and Settings shows it).
    #[serde(default)]
    pub speed: Option<Speed>,
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            enabled: false,
            url: DEFAULT_URL.to_string(),
            chat_model: String::new(),
            embed_model: DEFAULT_EMBED_MODEL.to_string(),
            speed: None,
        }
    }
}

/// The settings, and where they came from.
#[derive(Debug, Serialize)]
pub struct AiSettingsView {
    #[serde(flatten)]
    pub settings: AiSettings,
    /// "saved" (chosen in Settings), "environment" (GATHER_OLLAMA_* set when
    /// the app started) or "default".
    pub source: &'static str,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn saved(data: &Path) -> Option<AiSettings> {
    let bytes = fs::read(data.join(FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// What Settings shows: the saved choice, else what the environment set up
/// (with the daemon's own default model for `profile` where it sets none).
pub fn load(data: &Path, profile: Profile) -> AiSettingsView {
    let mut view = load_raw(data, profile);
    // Always show a speed: the default for this computer when none is chosen.
    view.settings.speed = view.settings.speed.or(Some(Speed::for_profile(profile)));
    view
}

fn load_raw(data: &Path, profile: Profile) -> AiSettingsView {
    if let Some(settings) = saved(data) {
        return AiSettingsView {
            settings,
            source: "saved",
        };
    }
    match env("GATHER_OLLAMA_URL") {
        Some(url) => AiSettingsView {
            settings: AiSettings {
                enabled: true,
                url,
                chat_model: match env("GATHER_OLLAMA_MODEL") {
                    Some(m) if m.eq_ignore_ascii_case("none") => String::new(),
                    Some(m) => m,
                    None if profile == Profile::Low => String::new(),
                    None => DAEMON_DEFAULT_CHAT_MODEL.to_string(),
                },
                embed_model: env("GATHER_OLLAMA_EMBED_MODEL")
                    .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string()),
                speed: None,
            },
            source: "environment",
        },
        None => AiSettingsView {
            settings: AiSettings::default(),
            source: "default",
        },
    }
}

/// Check and save. The daemon picks them up when it next starts.
pub fn save(data: &Path, settings: &AiSettings) -> Result<AiSettings, String> {
    let settings = validate(settings)?;
    fs::create_dir_all(data).map_err(|e| format!("saving AI settings: {e}"))?;
    let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(data.join(FILE), json).map_err(|e| format!("saving AI settings: {e}"))?;
    Ok(settings)
}

/// Environment for the daemon. Nothing when no choice was saved, so the
/// app's own environment passes through unchanged.
pub fn daemon_env(data: &Path) -> Vec<(&'static str, String)> {
    let Some(s) = saved(data) else {
        return Vec::new();
    };
    if !s.enabled {
        // Empty turns Ollama off, whatever the app's environment says.
        return vec![("GATHER_OLLAMA_URL", String::new())];
    }
    let mut env = vec![
        ("GATHER_OLLAMA_URL", s.url),
        ("GATHER_OLLAMA_EMBED_MODEL", s.embed_model),
        (
            "GATHER_OLLAMA_MODEL",
            if s.chat_model.is_empty() {
                "none".to_string()
            } else {
                s.chat_model
            },
        ),
    ];
    if let Some(speed) = s.speed {
        env.push((
            "GATHER_EXTRACTION_AI_DUTY_PERCENT",
            speed.duty_percent().to_string(),
        ));
    }
    env
}

/// Tidy and check settings; the address comes back as `http://host:port`,
/// with Ollama's port filled in when it was left out.
pub fn validate(s: &AiSettings) -> Result<AiSettings, String> {
    let url = s.url.trim().trim_end_matches('/').to_string();
    let chat_model = s.chat_model.trim().to_string();
    let embed_model = s.embed_model.trim().to_string();
    let url = if url.is_empty() {
        DEFAULT_URL.to_string()
    } else if s.enabled {
        let (host, port) = parse_url(&url)?;
        // The daemon's HTTP client would take a missing port as 80.
        if host.contains(':') {
            format!("http://[{host}]:{port}")
        } else {
            format!("http://{host}:{port}")
        }
    } else {
        url
    };
    if s.enabled && embed_model.is_empty() {
        return Err("Choose a model for search, e.g. nomic-embed-text.".to_string());
    }
    for model in [&chat_model, &embed_model] {
        if !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'))
        {
            return Err(format!("“{model}” isn't a model name Ollama uses."));
        }
    }
    Ok(AiSettings {
        enabled: s.enabled,
        url,
        chat_model,
        embed_model: if embed_model.is_empty() {
            DEFAULT_EMBED_MODEL.to_string()
        } else {
            embed_model
        },
        speed: s.speed,
    })
}

/// Host and port of an `http://host:port` address on this machine.
fn parse_url(url: &str) -> Result<(String, u16), String> {
    let rest = url.trim().strip_prefix("http://").ok_or_else(|| {
        "The address should start with http://, e.g. http://127.0.0.1:11434".to_string()
    })?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6
            .split_once(']')
            .ok_or("The address isn't valid.".to_string())?;
        (host.to_string(), after.strip_prefix(':').unwrap_or(""))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p),
            None => (authority.to_string(), ""),
        }
    };
    let port = if port.is_empty() {
        11434
    } else {
        port.parse()
            .map_err(|_| format!("“{port}” isn't a port number."))?
    };
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !local {
        return Err(
            "Gather only connects to Ollama on this computer: use 127.0.0.1 or localhost."
                .to_string(),
        );
    }
    Ok((host, port))
}

/// What a connection test found.
#[derive(Debug, Serialize)]
pub struct OllamaCheck {
    /// Models Ollama has downloaded, e.g. "llama3.2:1b".
    pub models: Vec<String>,
}

#[derive(Deserialize)]
struct Tags {
    #[serde(default)]
    models: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

/// One HTTP/1.0 request to Ollama at `url`: its status and body. HTTP/1.0
/// so the reply comes whole and the connection then closes.
fn request(
    url: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    wait: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let (host, port) = parse_url(url)?;
    let unreachable = || {
        format!(
            "Nothing answered at {url}. Is Ollama installed and running? \
             (Start the Ollama app, or run “ollama serve”.)"
        )
    };
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| unreachable())?
        .collect();
    let mut stream = addrs
        .iter()
        .find_map(|a| TcpStream::connect_timeout(a, Duration::from_millis(1500)).ok())
        .ok_or_else(unreachable)?;
    let _ = stream.set_read_timeout(Some(wait));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let host_header = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut head =
        format!("{method} {path} HTTP/1.0\r\nHost: {host_header}\r\nAccept: application/json\r\n");
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    head.push_str(body.unwrap_or(""));
    stream
        .write_all(head.as_bytes())
        .map_err(|_| unreachable())?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE)
        .read_to_end(&mut response)
        .map_err(|e| format!("Ollama didn't finish answering: {e}"))?;
    split_response(&response)
}

fn not_ollama() -> String {
    "Something answered, but it doesn't look like Ollama.".to_string()
}

/// Status code and body of a raw HTTP response.
fn split_response(response: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(not_ollama)?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status = head
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.get(2..5))
        .and_then(|code| code.parse().ok())
        .ok_or_else(not_ollama)?;
    Ok((status, response[split + 4..].to_vec()))
}

/// Ask Ollama at `url` which models it has.
pub fn check(url: &str) -> Result<OllamaCheck, String> {
    let (status, body) = request(url, "GET", "/api/tags", None, Duration::from_secs(5))?;
    parse_tags(status, &body)
}

fn parse_tags(status: u16, body: &[u8]) -> Result<OllamaCheck, String> {
    if status != 200 {
        return Err(not_ollama());
    }
    let tags: Tags = serde_json::from_slice(body).map_err(|_| not_ollama())?;
    let mut models: Vec<String> = tags.models.into_iter().map(|t| t.name).collect();
    models.sort();
    Ok(OllamaCheck { models })
}

/// Check that `model` at `url` gives the vectors search is built for.
/// Loading a model the first time can take a while, hence the long wait.
pub fn check_embed_model(url: &str, model: &str) -> Result<(), String> {
    let body = serde_json::json!({ "model": model, "input": "Gather dimension check" });
    let (status, reply) = request(
        url,
        "POST",
        "/api/embed",
        Some(&body.to_string()),
        Duration::from_secs(120),
    )?;
    embed_dimensions(model, status, &reply)
}

fn embed_dimensions(model: &str, status: u16, body: &[u8]) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        embeddings: Vec<Vec<f32>>,
        #[serde(default)]
        error: Option<String>,
    }
    let reply: Reply = serde_json::from_slice(body).map_err(|_| not_ollama())?;
    if status == 404 {
        return Err(format!(
            "Ollama doesn't have “{model}” yet: run “ollama pull {model}”, then save again."
        ));
    }
    if status != 200 {
        return Err(format!(
            "“{model}” can't be used for search: {}",
            reply
                .error
                .unwrap_or_else(|| format!("Ollama answered {status}"))
        ));
    }
    match reply.embeddings.first().map(Vec::len) {
        Some(EMBED_DIMENSIONS) => Ok(()),
        Some(n) => Err(format!(
            "“{model}” gives {n}-number vectors, but Gather's search needs {EMBED_DIMENSIONS} \
             (as {DEFAULT_EMBED_MODEL} gives). Choose a model like that one for search."
        )),
        None => Err(format!(
            "“{model}” didn't return a vector: it isn't a search model."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_addresses_are_accepted() {
        assert_eq!(
            parse_url("http://127.0.0.1:11434").unwrap(),
            ("127.0.0.1".to_string(), 11434)
        );
        assert_eq!(
            parse_url("http://localhost").unwrap(),
            ("localhost".to_string(), 11434)
        );
        assert_eq!(
            parse_url("http://[::1]:9000/").unwrap(),
            ("::1".to_string(), 9000)
        );
        assert!(parse_url("http://192.168.1.5:11434").is_err());
        assert!(parse_url("https://127.0.0.1:11434").is_err());
        assert!(parse_url("127.0.0.1:11434").is_err());
        assert!(parse_url("http://127.0.0.1:port").is_err());
    }

    #[test]
    fn the_model_list_is_read_from_ollamas_reply() {
        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
            {\"models\":[{\"name\":\"nomic-embed-text:latest\"},{\"name\":\"llama3.2:1b\"}]}";
        let (status, body) = split_response(reply).unwrap();
        assert_eq!(
            parse_tags(status, &body).unwrap().models,
            vec!["llama3.2:1b", "nomic-embed-text:latest"]
        );
        let (status, body) = split_response(b"HTTP/1.1 404 Not Found\r\n\r\n").unwrap();
        assert!(parse_tags(status, &body).is_err());
        let (status, body) = split_response(b"HTTP/1.1 200 OK\r\n\r\n<html>").unwrap();
        assert!(parse_tags(status, &body).is_err());
        assert!(split_response(b"garbage").is_err());
    }

    #[test]
    fn only_768_number_vectors_are_accepted_for_search() {
        let vector = |n: usize| format!("{{\"embeddings\":[[{}]]}}", vec!["0.1"; n].join(","));
        assert!(embed_dimensions("nomic-embed-text", 200, vector(768).as_bytes()).is_ok());
        let wrong = embed_dimensions("mxbai-embed-large", 200, vector(1024).as_bytes());
        assert!(wrong.unwrap_err().contains("1024"));
        let missing = embed_dimensions(
            "nomic-embed-text",
            404,
            br#"{"error":"model \"nomic-embed-text\" not found, try pulling it first"}"#,
        );
        assert!(missing
            .unwrap_err()
            .contains("ollama pull nomic-embed-text"));
        let chat = embed_dimensions(
            "llama3.2:1b",
            400,
            br#"{"error":"\"llama3.2:1b\" does not support embeddings"}"#,
        );
        assert!(chat.unwrap_err().contains("does not support embeddings"));
    }

    #[test]
    fn settings_always_show_a_speed_and_old_files_still_load() {
        let dir = std::env::temp_dir().join(format!("gather-ai-speed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // A file saved before speeds existed.
        fs::write(
            dir.join(FILE),
            r#"{"enabled":true,"url":"http://127.0.0.1:11434","chat_model":"smollm2:360m","embed_model":"nomic-embed-text"}"#,
        )
        .unwrap();
        assert_eq!(load(&dir, Profile::Low).settings.speed, Some(Speed::Gentle));
        assert_eq!(
            load(&dir, Profile::Standard).settings.speed,
            Some(Speed::Balanced)
        );
        assert!(daemon_env(&dir).iter().all(|(n, _)| !n.contains("DUTY")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_settings_become_the_daemons_environment() {
        let dir = std::env::temp_dir().join(format!("gather-ai-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(
            daemon_env(&dir).is_empty(),
            "nothing saved: environment passes through"
        );

        let saved = save(
            &dir,
            &AiSettings {
                enabled: true,
                url: " http://localhost:11434/ ".to_string(),
                chat_model: String::new(),
                embed_model: "nomic-embed-text".to_string(),
                speed: None,
            },
        )
        .unwrap();
        assert_eq!(saved.url, "http://localhost:11434");
        assert_eq!(load(&dir, Profile::Standard).source, "saved");
        // A missing port is Ollama's, written out for the daemon's client.
        let portless = validate(&AiSettings {
            url: "http://localhost".to_string(),
            ..saved.clone()
        })
        .unwrap();
        assert_eq!(portless.url, "http://localhost:11434");
        let env = daemon_env(&dir);
        assert!(env.contains(&("GATHER_OLLAMA_URL", "http://localhost:11434".to_string())));
        assert!(env.contains(&("GATHER_OLLAMA_MODEL", "none".to_string())));
        assert!(
            !env.iter()
                .any(|(name, _)| *name == "GATHER_EXTRACTION_AI_DUTY_PERCENT"),
            "no speed chosen: the daemon's default for this computer applies"
        );

        // A chosen speed is passed on as the share of time the model may work.
        let fast = save(
            &dir,
            &AiSettings {
                speed: Some(Speed::Full),
                ..saved.clone()
            },
        )
        .unwrap();
        assert!(
            daemon_env(&dir).contains(&("GATHER_EXTRACTION_AI_DUTY_PERCENT", "100".to_string()))
        );
        assert_eq!(Speed::Gentle.duty_percent(), 30);
        assert_eq!(Speed::Balanced.duty_percent(), 60);
        assert_eq!(fast.speed, Some(Speed::Full));

        save(
            &dir,
            &AiSettings {
                enabled: false,
                ..saved.clone()
            },
        )
        .unwrap();
        assert_eq!(
            daemon_env(&dir),
            vec![("GATHER_OLLAMA_URL", String::new())],
            "turned off: Ollama is off even if the environment set it up"
        );

        assert!(save(
            &dir,
            &AiSettings {
                enabled: true,
                url: "http://10.0.0.2:11434".to_string(),
                chat_model: String::new(),
                embed_model: "nomic-embed-text".to_string(),
                speed: None,
            }
        )
        .is_err());
        assert!(save(
            &dir,
            &AiSettings {
                chat_model: "llama3.2:1b; rm -rf".to_string(),
                ..AiSettings::default()
            }
        )
        .is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
