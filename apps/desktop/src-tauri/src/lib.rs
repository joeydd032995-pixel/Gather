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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![read_upload_file])
        .run(tauri::generate_context!())
        .expect("error while running gather-desktop");
}
