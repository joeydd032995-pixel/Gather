//! Opt-in update check. Gather is offline by default, so this is the one
//! place the app may reach the internet, and only when the user asks: the
//! setting starts off, and "Check now" is an explicit request. A check is a
//! single HTTPS GET of the release manifest; nothing about the user or their
//! data is sent.
//!
//! Installing needs a build signed with the project's updater key (the
//! installer verifies each download against the public key compiled in).
//! Builds without one report that updates are not available here.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct UpdateSettings {
    /// Check once at start-up. Off unless the user turns it on.
    #[serde(default)]
    pub check_on_start: bool,
}

#[derive(Debug, Serialize)]
pub struct UpdateCheck {
    /// False when this build carries no updater key.
    pub supported: bool,
    pub available: bool,
    pub current_version: String,
    pub version: Option<String>,
    pub notes: Option<String>,
}

/// The update found by the last check, kept for `install_update`.
#[derive(Default)]
pub struct PendingUpdate(pub Mutex<Option<Update>>);

/// Whether this build was configured with an updater public key.
pub fn supported<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|c| c.get("pubkey"))
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.trim().is_empty())
}

fn settings_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|e| format!("locating settings: {e}"))
}

pub fn load<R: Runtime>(app: &AppHandle<R>) -> UpdateSettings {
    settings_path(app)
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save<R: Runtime>(app: &AppHandle<R>, settings: UpdateSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("saving settings: {e}"))?;
    }
    let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| format!("saving settings: {e}"))
}

pub async fn check<R: Runtime>(
    app: &AppHandle<R>,
    pending: &PendingUpdate,
) -> Result<UpdateCheck, String> {
    let current_version = app.package_info().version.to_string();
    if !supported(app) {
        return Ok(UpdateCheck {
            supported: false,
            available: false,
            current_version,
            version: None,
            notes: None,
        });
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?;
    let result = UpdateCheck {
        supported: true,
        available: update.is_some(),
        current_version,
        version: update.as_ref().map(|u| u.version.clone()),
        notes: update.as_ref().and_then(|u| u.body.clone()),
    };
    *pending.0.lock().expect("pending update lock") = update;
    Ok(result)
}

/// Why `install` failed, which decides whether the local stack needs to be
/// brought back up.
pub enum InstallError {
    /// Nothing was changed: the stack is still running.
    NotStarted(String),
    /// The stack was stopped for the installer, which then failed.
    Failed(String),
}

/// Download, verify against the compiled-in key, and install the update
/// found by the last check. `before_install` stops the local stack first so
/// the installer can replace its files.
pub async fn install(
    pending: &PendingUpdate,
    before_install: impl FnOnce(),
) -> Result<(), InstallError> {
    let update = pending
        .0
        .lock()
        .expect("pending update lock")
        .take()
        .ok_or_else(|| InstallError::NotStarted("no update to install; check again".into()))?;
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| InstallError::NotStarted(format!("download failed: {e}")))?;
    before_install();
    update
        .install(bytes)
        .map_err(|e| InstallError::Failed(format!("install failed: {e}")))
}
