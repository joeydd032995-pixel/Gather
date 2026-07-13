//! Gather desktop shell. The UI talks to the local daemon over loopback
//! HTTP; the only native surface this app adds is the file dialog and a
//! command to read a picked file's bytes for upload.

use std::path::PathBuf;

/// Read a file the user explicitly selected via the native dialog so the
/// webview can upload it to the local daemon. Scope: only invoked with paths
/// returned by the dialog plugin; rejects directories.
#[tauri::command]
fn read_upload_file(path: PathBuf) -> Result<Vec<u8>, String> {
    if path.is_dir() {
        return Err("directories cannot be uploaded".to_string());
    }
    std::fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

/// Read the daemon's API token from the OS keychain (same entry the daemon
/// writes in GATHER_AUTH_MODE=keychain). None when absent — dev daemons run
/// open on loopback, so the UI simply sends no Authorization header.
#[tauri::command]
fn get_api_token() -> Result<Option<String>, String> {
    let entry = keyring::Entry::new("gather-daemon", "api-token")
        .map_err(|e| format!("keychain entry: {e}"))?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keychain read: {e}")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![read_upload_file, get_api_token])
        .run(tauri::generate_context!())
        .expect("error while running gather-desktop");
}
