//! OS-user-bound API-token provisioning (write-up §7.2).
//!
//! In `keychain` mode the daemon owns its bearer token: read it from the OS
//! keychain (macOS Keychain / Windows Credential Manager / Secret Service),
//! generating and storing a fresh 256-bit token on first run. The Tauri app
//! reads the same entry, so only processes running as this OS user can call
//! the API. Keychain storage is behind a trait so tests (and any headless
//! fallback logic) run against an in-memory store.

use crate::config::Config;

pub const KEYCHAIN_SERVICE: &str = "gather-daemon";
pub const KEYCHAIN_USER: &str = "api-token";

/// Minimal keychain surface the resolver needs.
pub trait TokenStore {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, token: &str) -> Result<(), String>;
}

/// Real OS keychain via the `keyring` crate.
pub struct OsKeychain;

impl TokenStore for OsKeychain {
    fn get(&self) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
            .map_err(|e| format!("keychain entry: {e}"))?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain read: {e}")),
        }
    }

    fn set(&self, token: &str) -> Result<(), String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
            .map_err(|e| format!("keychain entry: {e}"))?
            .set_password(token)
            .map_err(|e| format!("keychain write: {e}"))
    }
}

/// 256 bits of OS randomness, hex-encoded (64 chars).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS RNG unavailable");
    hex::encode(bytes)
}

/// Get-or-create the token in the given store.
pub fn get_or_create_token(store: &dyn TokenStore) -> Result<String, String> {
    if let Some(existing) = store.get()? {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let token = generate_token();
    store.set(&token)?;
    Ok(token)
}

/// Resolve the effective API token per GATHER_AUTH_MODE, mutating the config
/// in place. Keychain unavailability (headless hosts, containers) degrades
/// to the mode's fallback with a prominent warning — auth hardening must
/// never brick the daemon.
pub fn resolve(config: &mut Config) {
    if config.auth_mode != "keychain" {
        return; // env mode: config.api_token already holds GATHER_API_TOKEN
    }
    match get_or_create_token(&OsKeychain) {
        Ok(token) => {
            tracing::info!(
                fingerprint = &token[..8],
                "api token loaded from OS keychain"
            );
            config.api_token = Some(token);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "GATHER_AUTH_MODE=keychain but the OS keychain is unavailable; \
                 continuing with loopback-open API (set GATHER_API_TOKEN to enforce auth)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MemoryStore(Mutex<Option<String>>);

    impl TokenStore for MemoryStore {
        fn get(&self) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set(&self, token: &str) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(token.to_string());
            Ok(())
        }
    }

    struct BrokenStore;
    impl TokenStore for BrokenStore {
        fn get(&self) -> Result<Option<String>, String> {
            Err("no secret service".to_string())
        }
        fn set(&self, _: &str) -> Result<(), String> {
            Err("no secret service".to_string())
        }
    }

    #[test]
    fn generates_256_bit_hex_tokens() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn first_run_creates_and_persists() {
        let store = MemoryStore(Mutex::new(None));
        let created = get_or_create_token(&store).unwrap();
        assert_eq!(created.len(), 64);
        // Second call returns the same token, not a new one.
        let again = get_or_create_token(&store).unwrap();
        assert_eq!(created, again);
    }

    #[test]
    fn existing_token_is_reused() {
        let store = MemoryStore(Mutex::new(Some("preexisting-token".to_string())));
        assert_eq!(get_or_create_token(&store).unwrap(), "preexisting-token");
    }

    #[test]
    fn broken_store_surfaces_error() {
        assert!(get_or_create_token(&BrokenStore).is_err());
    }
}
