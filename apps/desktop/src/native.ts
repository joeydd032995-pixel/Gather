// Bindings to the desktop shell's native commands (src-tauri/src/lib.rs).
// Outside Tauri (vite dev in a browser) none of these exist.

export const isTauri = "__TAURI_INTERNALS__" in window;

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(command, args);
}

/**
 * Puts the app window into (or out of) full screen: the native window in the
 * desktop app, the page in a browser. Resolves to whether anything changed, so
 * a caller that turned full screen on only undoes its own change.
 */
export async function setFullscreen(on: boolean): Promise<boolean> {
  if (isTauri) {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const win = getCurrentWindow();
    if ((await win.isFullscreen()) === on) return false;
    await win.setFullscreen(on);
    return true;
  }
  if (on) {
    if (document.fullscreenElement || !document.fullscreenEnabled) return false;
    await document.documentElement.requestFullscreen();
    return true;
  }
  if (!document.fullscreenElement) return false;
  await document.exitFullscreen();
  return true;
}

/** Start-up state of the bundled database and daemon. */
export type RuntimeStatus =
  | { state: "starting"; step: string }
  | { state: "ready" }
  | { state: "unmanaged" }
  | { state: "failed"; message: string; log_dir: string };

export function runtimeStatus(): Promise<RuntimeStatus> {
  return call<RuntimeStatus>("runtime_status");
}

/** How much memory the bundled database and daemon are set up to use. */
export interface MemoryInfo {
  profile: "standard" | "low";
  /** Total RAM in MiB, when the app could read it. */
  total_mb: number | null;
  /** True when GATHER_MEMORY_PROFILE chose the profile rather than detection. */
  overridden: boolean;
}

export function memoryProfile(): Promise<MemoryInfo> {
  return call<MemoryInfo>("memory_profile");
}

export function getApiToken(): Promise<string | null> {
  return call<string | null>("get_api_token");
}

export interface UpdateSettings {
  check_on_start: boolean;
}

export interface UpdateCheck {
  /** False when this build has no updater key: updates come from the releases page. */
  supported: boolean;
  available: boolean;
  current_version: string;
  version: string | null;
  notes: string | null;
}

export function getUpdateSettings(): Promise<UpdateSettings> {
  return call<UpdateSettings>("get_update_settings");
}

export function setUpdateSettings(settings: UpdateSettings): Promise<void> {
  return call<void>("set_update_settings", { settings });
}

export function checkForUpdate(): Promise<UpdateCheck> {
  return call<UpdateCheck>("check_for_update");
}

/** Downloads, verifies and installs the update found by the last check, then restarts. */
export function installUpdate(): Promise<void> {
  return call<void>("install_update");
}

/** The local AI model (Ollama) Gather uses; see src-tauri/src/ai.rs. */
export interface AiSettings {
  enabled: boolean;
  /** Where Ollama listens, e.g. http://127.0.0.1:11434. */
  url: string;
  /** Model that reads files into items; empty for search only. */
  chat_model: string;
  /** Model for search by meaning. */
  embed_model: string;
  /** How hard the reading model may work (about 30 %, 60 % or all of the time). */
  speed: ReadingSpeed;
}

export type ReadingSpeed = "gentle" | "balanced" | "full";

export interface AiSettingsView extends AiSettings {
  /** Saved in Settings, taken from GATHER_OLLAMA_* variables, or the defaults. */
  source: "saved" | "environment" | "default";
}

/** Automatic import: an inbox folder Gather watches, and Claude Code's sessions. */
export interface ImportSettings {
  /** The inbox folder; null = off. */
  inbox: string | null;
  claude_code: boolean;
}

export interface ImportView extends ImportSettings {
  /** Where Claude Code's sessions would be read from. */
  claude_code_dir: string | null;
  /** A folder to suggest for the inbox. */
  suggested_inbox: string | null;
}

export function getImportSettings(): Promise<ImportView> {
  return call<ImportView>("get_import_settings");
}

/** Saves and restarts Gather's background service to apply it. */
export function saveImportSettings(settings: ImportSettings): Promise<ImportSettings> {
  return call<ImportSettings>("save_import_settings", { settings });
}

/** Ask for a folder with the system's folder dialog; null when cancelled. */
export async function chooseFolder(title: string): Promise<string | null> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const selection = await open({ directory: true, title });
  return typeof selection === "string" ? selection : null;
}

export function getAiSettings(): Promise<AiSettingsView> {
  return call<AiSettingsView>("get_ai_settings");
}

/** Saves the choice and restarts Gather's background service with it. */
export function saveAiSettings(settings: AiSettings): Promise<AiSettings> {
  return call<AiSettings>("save_ai_settings", { settings });
}

/** The models Ollama at `url` has downloaded; rejects with why it couldn't be reached. */
export function testOllama(url: string): Promise<{ models: string[] }> {
  return call<{ models: string[] }>("test_ollama", { url });
}

/** Where daemon.log and postgres.log are written. */
export function logsDir(): Promise<string> {
  return call<string>("logs_dir");
}

/** Shows the logs folder in the system's file manager. */
export function openLogsFolder(): Promise<void> {
  return call<void>("open_logs_folder");
}
