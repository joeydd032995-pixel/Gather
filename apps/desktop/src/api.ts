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

// --- Autonomous pipeline: review tray, feedback, tuning --------------------

async function postJson<T>(path: string, body: unknown = {}): Promise<T> {
  const res = await fetch(`${DAEMON_URL}/api/v1${path}`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return jsonOrThrow<T>(res);
}

async function getJson<T>(path: string): Promise<T> {
  const res = await fetch(`${DAEMON_URL}/api/v1${path}`, { headers: authHeaders() });
  return jsonOrThrow<T>(res);
}

export type ReviewReason = "low-confidence" | "merge-band" | "oversized-component" | string;

/** One item parked in the optional review tray, most informative first. */
export interface ReviewItem {
  id: string;
  target_kind: "unit" | "entity" | "merge" | "cluster";
  target_id: string;
  reason: ReviewReason;
  info_gain: number;
  signals: Record<string, unknown>;
  /** Unit statement, for unit items. */
  statement: string | null;
  /** Entity names, for held merge pairs. */
  a_name: string | null;
  b_name: string | null;
  created_at: string;
}

export async function listReview(limit = 100): Promise<ReviewItem[]> {
  const body = await getJson<{ items: ReviewItem[] }>(`/review?limit=${limit}`);
  return body.items;
}

/** Agree: keep a held unit, or perform a held merge. Records a positive label. */
export function acceptReview(id: string, note?: string): Promise<{ action: string }> {
  return postJson(`/review/${id}/accept`, { note: note || null });
}

/** Disagree: retract a held unit, or dismiss a held merge pair. Negative label. */
export function rejectReview(id: string, note?: string): Promise<{ action: string }> {
  return postJson(`/review/${id}/reject`, { note: note || null });
}

/** Close a tray item without acting on it (no label). */
export function dismissReview(id: string): Promise<unknown> {
  return postJson(`/review/${id}/resolve`);
}

/** Undo a reject: reactivates a retracted unit. */
export function restoreUnit(id: string): Promise<unknown> {
  return postJson(`/units/${id}/restore`);
}

export async function editUnit(id: string, statement: string, note?: string): Promise<void> {
  const res = await fetch(`${DAEMON_URL}/api/v1/units/${id}`, {
    method: "PATCH",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ statement, note: note || null }),
  });
  await jsonOrThrow(res);
}

export type TuningKey = "admit.hold_below" | "merge.auto_single" | "merge.agree";

export interface TunedThreshold {
  key: TuningKey;
  value: number;
  default: number;
  tuned: boolean;
  bounds: { min: number; max: number };
}

export interface TuningChange {
  key: TuningKey;
  old_value: number | null;
  new_value: number | null;
  actor: string;
  reason: Record<string, unknown>;
  created_at: string;
}

export interface TuningState {
  enabled: boolean;
  target_precision: number;
  min_samples: number;
  thresholds: TunedThreshold[];
  history: TuningChange[];
}

export function getTuning(): Promise<TuningState> {
  return getJson("/tuning");
}

export function resetTuning(key?: TuningKey): Promise<{ reset: string[] }> {
  return postJson("/tuning/reset", key ? { key } : {});
}

// --- Clusters and photos ------------------------------------------------------

export type ClusterKind = "topic" | "entity" | "photo_dup" | "album" | "photo_topic";

export interface ClusterSummary {
  id: string;
  kind: ClusterKind;
  label: string;
  cohesion: number;
  size: number;
  representative_id: string | null;
  updated_at: string;
}

export interface ClusterMember {
  member_kind: "unit" | "entity" | "image";
  member_id: string;
  sim: number;
  statement: string | null;
  /** Entity members: the entity's name. */
  name: string | null;
  /** Entity members: the entity it was merged into (null for the survivor). */
  merged_into: string | null;
  filename: string | null;
  taken_at: string | null;
  caption: string | null;
}

export interface ClusterDetail extends ClusterSummary {
  created_at: string;
  members: ClusterMember[];
}

/** One page of clusters of a kind, newest first. */
export async function listClusters(
  kind: ClusterKind,
  limit: number,
  offset: number,
): Promise<ClusterSummary[]> {
  const body = await getJson<{ items: ClusterSummary[] }>(
    `/clusters?kind=${kind}&limit=${limit}&offset=${offset}`,
  );
  return body.items;
}

export function getCluster(id: string): Promise<ClusterDetail> {
  return getJson(`/clusters/${id}`);
}

/**
 * Fetch a thumbnail with the bearer token (an <img src> can't send it) and
 * return an object URL. The caller must URL.revokeObjectURL it when done.
 */
export async function fetchThumbnailUrl(imageId: string): Promise<string> {
  const res = await fetch(`${DAEMON_URL}/api/v1/images/${imageId}/thumbnail`, {
    headers: authHeaders(),
  });
  if (!res.ok) {
    throw new Error(`thumbnail unavailable (${res.status})`);
  }
  return URL.createObjectURL(await res.blob());
}

export interface UnmergeOutcome {
  winner_id: string;
  loser_id: string;
  units_restored: number;
  relationships_restored: number;
  aliases_restored: number;
  descendants_restored: number;
}

/** Split a merged-away entity back out; the pair is never merged again. */
export function unmergeEntity(id: string, note?: string): Promise<UnmergeOutcome> {
  return postJson(`/entities/${id}/unmerge`, { note: note || null });
}
