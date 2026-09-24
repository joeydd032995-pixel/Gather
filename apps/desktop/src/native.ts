// Bindings to the desktop shell's native commands (src-tauri/src/lib.rs).
// Outside Tauri (vite dev in a browser) none of these exist.

export const isTauri = "__TAURI_INTERNALS__" in window;

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(command, args);
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
