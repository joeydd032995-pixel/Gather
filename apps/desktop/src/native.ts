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
