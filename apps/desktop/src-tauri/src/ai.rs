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

const FILE: &str = "ai-settings.json";
pub const DEFAULT_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";
/// Enough for Ollama's model list; anything bigger isn't Ollama.
const MAX_RESPONSE: u64 = 1024 * 1024;

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
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            enabled: false,
            url: DEFAULT_URL.to_string(),
            chat_model: String::new(),
            embed_model: DEFAULT_EMBED_MODEL.to_string(),
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

/// What Settings shows: the saved choice, else what the environment set up.
pub fn load(data: &Path) -> AiSettingsView {
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
                chat_model: env("GATHER_OLLAMA_MODEL")
                    .filter(|m| !m.eq_ignore_ascii_case("none"))
                    .unwrap_or_default(),
                embed_model: env("GATHER_OLLAMA_EMBED_MODEL")
                    .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string()),
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
    vec![
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
    ]
}

fn validate(s: &AiSettings) -> Result<AiSettings, String> {
    let url = s.url.trim().trim_end_matches('/').to_string();
    let chat_model = s.chat_model.trim().to_string();
    let embed_model = s.embed_model.trim().to_string();
    if s.enabled {
        parse_url(&url)?;
        if embed_model.is_empty() {
            return Err("Choose a model for search, e.g. nomic-embed-text.".to_string());
        }
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
        url: if url.is_empty() {
            DEFAULT_URL.to_string()
        } else {
            url
        },
        chat_model,
        embed_model: if embed_model.is_empty() {
            DEFAULT_EMBED_MODEL.to_string()
        } else {
            embed_model
        },
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

/// Ask Ollama at `url` which models it has.
pub fn check(url: &str) -> Result<OllamaCheck, String> {
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
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    // HTTP/1.0: the reply comes whole and the connection then closes.
    stream
        .write_all(
            format!(
                "GET /api/tags HTTP/1.0\r\nHost: {host}:{port}\r\nAccept: application/json\r\n\r\n"
            )
            .as_bytes(),
        )
        .map_err(|_| unreachable())?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE)
        .read_to_end(&mut response)
        .map_err(|e| format!("Ollama didn't finish answering: {e}"))?;
    parse_tags(&response)
}

fn parse_tags(response: &[u8]) -> Result<OllamaCheck, String> {
    let not_ollama = || "Something answered, but it doesn't look like Ollama.".to_string();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(not_ollama)?;
    let head = String::from_utf8_lossy(&response[..split]);
    if !(head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")) {
        return Err(not_ollama());
    }
    let tags: Tags = serde_json::from_slice(&response[split + 4..]).map_err(|_| not_ollama())?;
    let mut models: Vec<String> = tags.models.into_iter().map(|t| t.name).collect();
    models.sort();
    Ok(OllamaCheck { models })
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
        assert_eq!(
            parse_tags(reply).unwrap().models,
            vec!["llama3.2:1b", "nomic-embed-text:latest"]
        );
        assert!(parse_tags(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(parse_tags(b"HTTP/1.1 200 OK\r\n\r\n<html>").is_err());
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
            },
        )
        .unwrap();
        assert_eq!(saved.url, "http://localhost:11434");
        assert_eq!(load(&dir).source, "saved");
        let env = daemon_env(&dir);
        assert!(env.contains(&("GATHER_OLLAMA_URL", "http://localhost:11434".to_string())));
        assert!(env.contains(&("GATHER_OLLAMA_MODEL", "none".to_string())));

        save(
            &dir,
            &AiSettings {
                enabled: false,
                ..saved
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
