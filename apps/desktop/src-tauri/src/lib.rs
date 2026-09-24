//! Gather desktop shell. The UI talks to the local daemon over loopback
//! HTTP. Natively, this app adds the file dialog, a command to read a picked
//! file's bytes for upload, the supervisor that runs the bundled database and
//! daemon (`runtime`), and the opt-in update check (`updates`).

mod runtime;
mod updates;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager, RunEvent, State};

use runtime::{Paths, Runtime, Status};
use updates::{PendingUpdate, UpdateCheck, UpdateSettings};

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

/// Where the bundled database and daemon are in start-up.
#[tauri::command]
fn runtime_status(runtime: State<'_, Arc<Runtime>>) -> Status {
    runtime.status()
}

#[tauri::command]
fn get_update_settings(app: AppHandle) -> UpdateSettings {
    updates::load(&app)
}

#[tauri::command]
fn set_update_settings(app: AppHandle, settings: UpdateSettings) -> Result<(), String> {
    updates::save(&app, settings)
}

#[tauri::command]
async fn check_for_update(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> Result<UpdateCheck, String> {
    updates::check(&app, &pending).await
}

#[tauri::command]
async fn install_update(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
    runtime: State<'_, Arc<Runtime>>,
) -> Result<(), String> {
    let runtime = Arc::clone(&runtime);
    updates::install(&pending, move || runtime.stop()).await?;
    app.restart()
}

fn runtime_paths(app: &AppHandle) -> Result<Paths, String> {
    let resources = app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource dir: {e}"))?;
    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("data dir: {e}"))?;
    // Tauri installs sidecars (bundle.externalBin) next to the app binary.
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("locating the app: {e}"))?
        .parent()
        .map(PathBuf::from)
        .ok_or("locating the app directory")?;
    Ok(Paths {
        postgres: resources.join("postgres"),
        daemon: exe_dir.join(format!("gather-daemon{}", std::env::consts::EXE_SUFFIX)),
        data,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let runtime = Arc::new(Runtime::default());
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::clone(&runtime))
        .manage(PendingUpdate::default())
        .setup(|app| {
            if updates::supported(app.handle()) {
                app.handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())?;
            }
            let runtime = Arc::clone(&app.state::<Arc<Runtime>>());
            match runtime_paths(app.handle()) {
                Ok(paths) => {
                    std::thread::spawn(move || runtime.start(&paths));
                }
                Err(e) => eprintln!("gather: not managing the local stack: {e}"),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            read_upload_file,
            get_api_token,
            runtime_status,
            get_update_settings,
            set_update_settings,
            check_for_update,
            install_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while building gather-desktop");

    app.run(move |_, event| {
        if let RunEvent::Exit = event {
            runtime.stop();
        }
    });
}
