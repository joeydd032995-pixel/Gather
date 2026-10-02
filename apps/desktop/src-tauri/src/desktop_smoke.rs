//! Opt-in driver for CI's real packaged desktop smoke test.
//! Without GATHER_DESKTOP_SMOKE_REPORT this module never changes window state.
//! Reports query the actual native window; no service or focus result is mocked.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tauri::Manager;

pub struct Probe {
    report: PathBuf,
    data: PathBuf,
    started: Instant,
    ready: bool,
    focused_reported: bool,
}

impl Probe {
    pub fn from_env(data: &Path) -> Option<Self> {
        if !std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true") {
            return None;
        }
        let report = std::env::var_os("GATHER_DESKTOP_SMOKE_REPORT")?;
        Some(Self {
            report: PathBuf::from(report),
            data: data.to_path_buf(),
            started: Instant::now(),
            ready: false,
            focused_reported: false,
        })
    }

    pub fn tick(&mut self, app: &tauri::AppHandle) {
        if std::fs::remove_file(self.report.with_extension("close")).is_ok() {
            app.exit(0);
            return;
        }
        if std::fs::remove_file(self.report.with_extension("minimize")).is_ok() {
            self.ready = false;
            self.focused_reported = false;
        }
        if self.ready {
            if !self.focused_reported {
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_focused().unwrap_or(false) && !window.is_minimized().unwrap_or(true) {
                        self.write("focused", &window);
                        self.focused_reported = true;
                    }
                }
            }
            return;
        }
        if self.started.elapsed() < Duration::from_secs(2) {
            return;
        }
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.minimize();
            if window.is_minimized().unwrap_or(false) {
                self.ready = true;
                self.write("ready", &window);
            }
        }
    }

    pub fn focused(&self, window: &tauri::WebviewWindow) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if window.is_focused().unwrap_or(false) && !window.is_minimized().unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.write("focused", window);
    }

    fn write(&self, phase: &str, window: &tauri::WebviewWindow) {
        let state = serde_json::json!({
            "phase": phase,
            "pid": std::process::id(),
            "data_dir": self.data,
            "focused": window.is_focused().unwrap_or(false),
            "minimized": window.is_minimized().unwrap_or(true),
        });
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            if let Some(parent) = self.report.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temp = self.report.with_extension("tmp");
            std::fs::write(&temp, serde_json::to_vec(&state)?)?;
            std::fs::rename(temp, &self.report)?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("desktop smoke report failed: {error}");
        }
    }
}
