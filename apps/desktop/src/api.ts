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

// --- Contradiction review -------------------------------------------------

export interface ContradictionSummary {
  id: string;
  score: number;
  detection_method: string;
  explanation: string | null;
  status: string;
  detected_at: string;
  unit_a: { id: string; statement: string };
  unit_b: { id: string; statement: string };
}

export interface Provenance {
  artifact_kind: string;
  source_platform: string;
  original_filename: string | null;
  ingested_at: string;
  quote: string | null;
}

export interface ContradictionDetail extends ContradictionSummary {
  unit_a: ContradictionSummary["unit_a"] & {
    valid_from: string | null;
    provenance: Provenance[];
  };
  unit_b: ContradictionSummary["unit_b"] & {
    valid_from: string | null;
    provenance: Provenance[];
  };
  audit: { action: string; actor: string; note: string | null; created_at: string }[];
}

export type Resolution = "resolved_a" | "resolved_b" | "both_valid" | "dismissed";

async function jsonOrThrow<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    throw new Error(body?.error?.message ?? `request failed (${res.status})`);
  }
  return res.json();
}

export async function listContradictions(status = "open"): Promise<ContradictionSummary[]> {
  const res = await fetch(`${DAEMON_URL}/api/v1/contradictions?status=${status}&limit=100`, {
    headers: authHeaders(),
  });
  const body = await jsonOrThrow<{ items: ContradictionSummary[] }>(res);
  return body.items;
}

export async function getContradiction(id: string): Promise<ContradictionDetail> {
  const res = await fetch(`${DAEMON_URL}/api/v1/contradictions/${id}`, {
    headers: authHeaders(),
  });
  return jsonOrThrow(res);
}

export async function resolveContradiction(
  id: string,
  resolution: Resolution,
  note?: string,
): Promise<void> {
  const res = await fetch(`${DAEMON_URL}/api/v1/contradictions/${id}/resolve`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ resolution, note: note || null }),
  });
  await jsonOrThrow(res);
}

export async function annotateContradiction(id: string, note: string): Promise<void> {
  const res = await fetch(`${DAEMON_URL}/api/v1/contradictions/${id}/annotations`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ note }),
  });
  await jsonOrThrow(res);
}

// --- Entity resolution ----------------------------------------------------

export interface EntityRef {
  id: string;
  name: string;
  kind: string;
}

export interface MergeSuggestion {
  a: EntityRef;
  b: EntityRef;
  score: number;
  /** "rule:name-similarity" (offline) or "embedding:cosine" (Ollama opt-in). */
  method: string;
}

export interface EntityDetail extends EntityRef {
  description: string | null;
  merged_into_entity_id: string | null;
  created_at: string;
  aliases: string[];
  audit: {
    action: string;
    actor: string;
    note: string | null;
    winner_entity_id: string;
    loser_entity_id: string;
    created_at: string;
  }[];
}

export async function listMergeSuggestions(): Promise<MergeSuggestion[]> {
  const res = await fetch(`${DAEMON_URL}/api/v1/entities/merge-suggestions?limit=100`, {
    headers: authHeaders(),
  });
  const body = await jsonOrThrow<{ items: MergeSuggestion[] }>(res);
  return body.items;
}

export async function getEntity(id: string): Promise<EntityDetail> {
  const res = await fetch(`${DAEMON_URL}/api/v1/entities/${id}`, {
    headers: authHeaders(),
  });
  return jsonOrThrow(res);
}

/** `winnerId` survives; `loserId` is folded into it. */
export async function mergeEntities(
  winnerId: string,
  loserId: string,
  note?: string,
): Promise<void> {
  const res = await fetch(`${DAEMON_URL}/api/v1/entities/${winnerId}/merge`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ loser_id: loserId, note: note || null }),
  });
  await jsonOrThrow(res);
}

export async function dismissMergeSuggestion(
  id: string,
  otherId: string,
  note?: string,
): Promise<void> {
  const res = await fetch(
    `${DAEMON_URL}/api/v1/entities/${id}/merge-suggestions/dismiss`,
    {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({ other_id: otherId, note: note || null }),
    },
  );
  await jsonOrThrow(res);
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
