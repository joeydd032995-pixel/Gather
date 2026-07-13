// Thin client for the local Gather daemon. All requests stay on loopback;
// the daemon enforces CORS (tauri://localhost) and optional bearer auth.

export const DAEMON_URL = "http://127.0.0.1:7601";

export interface FileResult {
  filename: string;
  kind: string | null;
  artifact_id: string | null;
  deduplicated: boolean;
  status: "accepted" | "deduplicated" | "rejected";
  detail: string | null;
  segments: number;
}

export interface FilesResponse {
  job_id: string;
  files: FileResult[];
}

export interface HealthState {
  reachable: boolean;
  ready: boolean;
}

// In the packaged app the token is provisioned into the OS keychain by the
// daemon installer and injected here at startup; during development it is
// empty and the loopback daemon runs open.
let apiToken = "";
export function setApiToken(token: string) {
  apiToken = token;
}

function authHeaders(): Record<string, string> {
  return apiToken ? { Authorization: `Bearer ${apiToken}` } : {};
}

export async function checkHealth(): Promise<HealthState> {
  try {
    const [h, r] = await Promise.all([
      fetch(`${DAEMON_URL}/healthz`),
      fetch(`${DAEMON_URL}/readyz`),
    ]);
    return { reachable: h.ok, ready: r.ok };
  } catch {
    return { reachable: false, ready: false };
  }
}

export async function uploadFiles(files: File[]): Promise<FilesResponse> {
  const form = new FormData();
  for (const file of files) {
    form.append("file", file, file.name);
  }
  const res = await fetch(`${DAEMON_URL}/api/v1/ingest/files`, {
    method: "POST",
    headers: authHeaders(),
    body: form,
  });
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    throw new Error(body?.error?.message ?? `upload failed (${res.status})`);
  }
  return res.json();
}
