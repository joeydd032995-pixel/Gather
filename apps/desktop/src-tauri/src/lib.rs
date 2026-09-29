//! Gather desktop shell. The UI talks to the local daemon over loopback
//! HTTP. Natively, this app adds the file dialog, a command to read a picked
//! file's bytes for upload, the supervisor that runs the bundled database and
//! daemon (`runtime`), and the opt-in update check (`updates`).

mod ai;
mod folders;
mod memory;
mod runtime;
mod updates;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager, RunEvent, State};

use runtime::{Paths, Runtime, Status};
use updates::{InstallError, PendingUpdate, UpdateCheck, UpdateSettings};

/// Read a file the user explicitly selected via the native dialog so the
/// webview can upload it to the local daemon. Scope: only invoked with paths
/// returned by the dialog plugin; rejects directories. Returned as raw bytes
/// (an ArrayBuffer in the webview): serialized as JSON it would be an array
/// of numbers many times the file's size.
///
/// A file over the daemon's per-file limit is refused from its size, before
/// any of it is read: the daemon would refuse it anyway, and holding it in
/// memory first could exhaust a small machine.
#[tauri::command]
fn read_upload_file(path: PathBuf) -> Result<tauri::ipc::Response, String> {
    use std::io::Read;

    if path.is_dir() {
        return Err("directories cannot be uploaded".to_string());
    }
    let failed = |e: std::io::Error| format!("failed to read {}: {e}", path.display());
    let cap_mb = memory::max_upload_mb(memory::current().profile);
    let cap = cap_mb * 1024 * 1024;
    let too_large = || {
        format!("This file is larger than {cap_mb} MB, the most Gather accepts per file on this computer.")
    };
    let file = std::fs::File::open(&path).map_err(failed)?;
    let len = file.metadata().map_err(failed)?.len();
    if len > cap {
        return Err(too_large());
    }
    // Bounded, in case the file grew since its size was read.
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(cap + 1).read_to_end(&mut bytes).map_err(failed)?;
    if bytes.len() as u64 > cap {
        return Err(too_large());
    }
    Ok(tauri::ipc::Response::new(bytes))
}

/// The files of a project folder the user picked with the native dialog,
/// with their paths inside it (no bytes: those are read per file).
#[tauri::command]
fn list_project_folder(path: PathBuf) -> Result<folders::FolderListing, String> {
    folders::list(&path)
}

/// The daemon's API token. In GATHER_AUTH_MODE=env (chosen by the user,
/// e.g. on a desktop without a keyring) the daemon inherits this app's
/// GATHER_API_TOKEN, so return that; otherwise read the OS keychain entry the
/// daemon writes in keychain mode. None when absent: dev daemons run open on
/// loopback, so the UI simply sends no Authorization header.
#[tauri::command]
fn get_api_token() -> Result<Option<String>, String> {
    let env_mode = std::env::var("GATHER_AUTH_MODE").is_ok_and(|m| m.eq_ignore_ascii_case("env"));
    if env_mode {
        return Ok(std::env::var("GATHER_API_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()));
    }
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

/// The memory profile the local stack runs with, for the Settings page.
#[tauri::command]
fn memory_profile() -> memory::MemoryInfo {
    memory::current()
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
    let stack = Arc::clone(&runtime);
    match updates::install(&pending, move || stack.stop()).await {
        Ok(()) => app.restart(),
        Err(InstallError::NotStarted(e)) => Err(e),
        Err(InstallError::Failed(e)) => {
            // The stack was stopped for the installer: bring it back.
            runtime.resume();
            Err(format!("{e}. Gather is still on the current version."))
        }
    }
}

/// The app data folder: the database, logs and saved AI settings.
fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|p| dunce::simplified(&p).to_path_buf())
        .map_err(|e| format!("data dir: {e}"))
}

#[tauri::command]
fn get_ai_settings(app: AppHandle) -> Result<ai::AiSettingsView, String> {
    Ok(ai::load(&data_dir(&app)?))
}

/// Save the AI model choice and restart Gather's background service with it.
#[tauri::command]
fn save_ai_settings(
    app: AppHandle,
    runtime: State<'_, Arc<Runtime>>,
    settings: ai::AiSettings,
) -> Result<ai::AiSettings, String> {
    let saved = ai::save(&data_dir(&app)?, &settings)?;
    runtime.restart("Applying your AI model settings")?;
    Ok(saved)
}

/// Which models Ollama at `url` has, or why it couldn't be reached.
#[tauri::command]
async fn test_ollama(url: String) -> Result<ai::OllamaCheck, String> {
    tauri::async_runtime::spawn_blocking(move || ai::check(&url))
        .await
        .map_err(|e| e.to_string())?
}

/// Where daemon.log and postgres.log are.
#[tauri::command]
fn logs_dir(app: AppHandle, runtime: State<'_, Arc<Runtime>>) -> Result<String, String> {
    let dir = match runtime.logs_dir() {
        Some(dir) => dir,
        None => data_dir(&app)?.join("logs"),
    };
    Ok(dir.display().to_string())
}

/// Show the logs folder in the system's file manager.
#[tauri::command]
fn open_logs_folder(app: AppHandle, runtime: State<'_, Arc<Runtime>>) -> Result<(), String> {
    let dir = match runtime.logs_dir() {
        Some(dir) => dir,
        None => data_dir(&app)?.join("logs"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    #[cfg(windows)]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let opener = "xdg-open";
    std::process::Command::new(opener)
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("opening {}: {e}", dir.display()))
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
    // On Windows these can come back as verbatim `\\?\C:\...` paths.
    // PostgreSQL's tools locate each other from their own path and pass it
    // through cmd.exe, which can't run a verbatim path, so initdb fails with
    // "program postgres is needed by initdb but was not found". Plain paths
    // work everywhere; `simplified` leaves them unchanged elsewhere.
    let plain = |p: PathBuf| dunce::simplified(&p).to_path_buf();
    Ok(Paths {
        postgres: plain(resources.join("postgres")),
        daemon: plain(exe_dir.join(format!("gather-daemon{}", std::env::consts::EXE_SUFFIX))),
        data: plain(data),
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
            list_project_folder,
            get_api_token,
            runtime_status,
            memory_profile,
            get_update_settings,
            set_update_settings,
            check_for_update,
            install_update,
            get_ai_settings,
            save_ai_settings,
            test_ollama,
            logs_dir,
            open_logs_folder,
        ])
        .build(tauri::generate_context!())
        .expect("error while building gather-desktop");

    app.run(move |_, event| {
        if let RunEvent::Exit = event {
            runtime.stop();
        }
    });
}
