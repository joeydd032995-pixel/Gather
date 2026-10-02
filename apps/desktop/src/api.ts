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

/** How many times an upload rate-limited by the daemon (429) is retried. */
const UPLOAD_RETRIES = 8;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function uploadFiles(files: File[]): Promise<FilesResponse> {
  const form = new FormData();
  for (const file of files) {
    form.append("file", file, file.name);
  }
  return postForm("/ingest/files", form);
}

/** POST a multipart form, retrying while the daemon is rate limiting. */
async function postForm<T>(path: string, form: FormData): Promise<T> {
  // Uploading a batch one file at a time can outpace the daemon's rate limit;
  // a 429 means "slow down", not "this file is bad", so wait and retry.
  let res: Response;
  for (let attempt = 0; ; attempt++) {
    res = await fetch(`${DAEMON_URL}/api/v1${path}`, {
      method: "POST",
      headers: authHeaders(),
      body: form,
    });
    if (res.status !== 429 || attempt >= UPLOAD_RETRIES) break;
    const retryAfter = Number(res.headers.get("Retry-After"));
    await sleep(retryAfter > 0 ? retryAfter * 1000 : 250 * 2 ** attempt);
  }
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

// --- Library: stored files, their contents, and search ---------------------

export type ArtifactStatus = "processing" | "done" | "failed";

export interface ArtifactSummary {
  id: string;
  kind: string;
  source_platform: string;
  original_filename: string | null;
  media_type: string | null;
  byte_size: number;
  ingested_at: string;
  /** Items Gather extracted from this file. */
  unit_count: number;
  status: ArtifactStatus;
}

export interface ArtifactDetail extends ArtifactSummary {
  document: { page_count: number | null; segment_count: number; extraction_status: string } | null;
  image: {
    id: string;
    width: number | null;
    height: number | null;
    taken_at: string | null;
    caption: string | null;
  } | null;
  conversations: { id: string; title: string | null }[];
}

/** What the daemon runs with, and how much is still waiting to be read. */
export interface DaemonStatus {
  version: string;
  ai: {
    enabled: boolean;
    url: string | null;
    model: string | null;
    embed_model: string | null;
  };
  reading: {
    /** Sections, messages and image text not yet read. */
    chunks: number;
    /** Files with something still to read. */
    files: number;
    /** Sections set aside after an error (details in daemon.log). */
    failed: number;
  };
  /** The latest job to read earlier files with the AI model; null if none yet. */
  reread: RereadJob | null;
  /** Automatic import: what is watched and what has been read. */
  import: {
    inbox_dir: string | null;
    claude_code_dir: string | null;
    /** Claude Code sessions imported. */
    sessions: number;
    /** Inbox files read. */
    inbox_done: number;
    /** Files that could not be read (in the inbox's failed folder, with why). */
    needs_attention: number;
    recent: {
      name: string | null;
      kind: "claude_code" | "inbox";
      status: "imported" | "unrecognized" | "failed";
      detail: string | null;
      at: string;
    }[];
  };
}

export interface RereadJob {
  id: string;
  model: string;
  status: "running" | "done" | "cancelled";
  /** Sections to read when the job started. */
  total: number;
  /** Sections the model has read. */
  done: number;
  /** Sections the model couldn't read. */
  failed: number;
}

/** Read earlier files again with the AI model; `job` is null when there is nothing left to read. */
export async function startReread(): Promise<{ job: RereadJob | null }> {
  return postJson("/reread");
}

export async function cancelReread(): Promise<{ job: RereadJob | null }> {
  return postJson("/reread/cancel");
}

export async function getStatus(): Promise<DaemonStatus> {
  const res = await fetch(`${DAEMON_URL}/api/v1/status`, { headers: authHeaders() });
  return jsonOrThrow<DaemonStatus>(res);
}

export async function listArtifacts(limit = 50, offset = 0): Promise<ArtifactSummary[]> {
  const res = await fetch(`${DAEMON_URL}/api/v1/artifacts?limit=${limit}&offset=${offset}`, {
    headers: authHeaders(),
  });
  const body = await jsonOrThrow<{ items: ArtifactSummary[] }>(res);
  return body.items;
}

export async function getArtifact(id: string): Promise<ArtifactDetail> {
  const res = await fetch(`${DAEMON_URL}/api/v1/artifacts/${id}`, { headers: authHeaders() });
  return jsonOrThrow<ArtifactDetail>(res);
}

export interface UnitSummary {
  id: string;
  kind: string;
  statement: string;
  confidence: number;
  status: string;
  subject_entity_id: string | null;
  valid_from: string | null;
}

/**
 * Live units (not retracted or superseded) extracted from one file, or about
 * one entity, newest first.
 */
export async function listUnits(filter: {
  artifactId?: string;
  subjectEntityId?: string;
  limit?: number;
  offset?: number;
}): Promise<UnitSummary[]> {
  const params = new URLSearchParams({
    live: "true",
    limit: String(filter.limit ?? 100),
    offset: String(filter.offset ?? 0),
  });
  if (filter.artifactId) params.set("artifact_id", filter.artifactId);
  if (filter.subjectEntityId) params.set("subject_entity_id", filter.subjectEntityId);
  const res = await fetch(`${DAEMON_URL}/api/v1/atomic-units?${params}`, {
    headers: authHeaders(),
  });
  const body = await jsonOrThrow<{ items: UnitSummary[] }>(res);
  return body.items;
}

export interface Passage {
  seq: number;
  heading: string | null;
  page: number | null;
  role: string | null;
  text: string;
}

export interface ArtifactContent {
  source: "document" | "conversation" | "image" | "none";
  items: Passage[];
  total: number;
}

export async function getArtifactContent(
  id: string,
  limit = 20,
  offset = 0,
): Promise<ArtifactContent> {
  const res = await fetch(
    `${DAEMON_URL}/api/v1/artifacts/${id}/content?limit=${limit}&offset=${offset}`,
    { headers: authHeaders() },
  );
  return jsonOrThrow<ArtifactContent>(res);
}

/** What a document is about and the sentences that say the most. */
export interface ArtifactDigest {
  artifact_id: string;
  /** "extractive" (sentences from the file) or "llm:<model>" (reworded, checked against the text). */
  method: string;
  summary: string;
  key_points: { text: string; score: number; segment_seq: number }[];
  topics: string[];
  outline: { level: number; text: string }[];
  takeaways: string[];
  open_questions: string[];
  stats: { words: number; sentences: number; sections: number };
  created_at: string;
}

/** The digest of a file, or null while it isn't ready (or the file has none). */
export async function getArtifactDigest(id: string): Promise<ArtifactDigest | null> {
  const res = await fetch(`${DAEMON_URL}/api/v1/artifacts/${id}/digest`, {
    headers: authHeaders(),
  });
  if (res.status === 404) return null;
  return jsonOrThrow<ArtifactDigest>(res);
}

export type SearchScope = "document_segments" | "atomic_units" | "messages";

export interface SearchHit {
  id: string;
  scope: SearchScope;
  content: string;
  score: number;
  artifact_id: string | null;
}

export async function search(text: string, scope: SearchScope, limit = 20): Promise<SearchHit[]> {
  const res = await fetch(`${DAEMON_URL}/api/v1/search/semantic`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ text, scope, limit }),
  });
  const body = await jsonOrThrow<{ hits: SearchHit[] }>(res);
  return body.hits;
}

// --- Graph: how entities and files connect --------------------------------

export interface GraphOverview {
  entities: { id: string; name: string; kind: string; weight: number }[];
  files: { id: string; name: string; kind: string; mentions: number }[];
  relations: {
    source: string;
    target: string;
    relation_type: string;
    count: number;
    confidence: number;
  }[];
  mentions: { file_id: string; entity_id: string; count: number }[];
  /** Projects (folders or .zip files uploaded whole). */
  projects: { id: string; name: string; source: "folder" | "zip"; files: number }[];
  /** Folders inside a project (only in a project's own graph). */
  folders: { id: string; project_id: string; name: string; path: string }[];
  /** A project or folder holding a folder or file (a file's id is its artifact's). */
  contains: {
    parent_type: "project" | "folder";
    parent: string;
    child_type: "folder" | "file";
    child: string;
  }[];
  /** Two projects that are alike, and why. */
  similar: { a: string; b: string; score: number; reasons: string[] }[];
  entity_total: number;
  truncated: boolean;
}

/** `maxProjects` 0 leaves projects out. */
export async function getGraphOverview(
  maxEntities = 150,
  maxFiles = 100,
  maxProjects = 150,
): Promise<GraphOverview> {
  const res = await fetch(
    `${DAEMON_URL}/api/v1/graph?max_entities=${maxEntities}&max_files=${maxFiles}&max_projects=${maxProjects}`,
    { headers: authHeaders() },
  );
  return jsonOrThrow<GraphOverview>(res);
}

// --- Semantic safety: why something was (or wasn't) concluded ------------

export type CertificateOutcome =
  | "auto_applied"
  | "needs_review"
  | "blocked"
  | "user_decision"
  | "superseded"
  | "retracted";

/** The record behind one automatic conclusion, or one Gather held back. */
export interface Certificate {
  id: string;
  conclusion_kind: string;
  conclusion_key: string;
  conclusion_id: string | null;
  subject_ids: string[];
  rule_id: string;
  rule_version: number;
  decision: CertificateOutcome;
  outcome: CertificateOutcome;
  evidence_class: string;
  inputs: { id: string; kind: string; class: string; detail: Record<string, unknown> }[];
  source_artifact_ids: string[];
  model_version: string | null;
  scope: Record<string, unknown>;
  temporal: Record<string, unknown>;
  reason_codes: string[];
  /** The reason codes in plain language. */
  reasons: { code: string; text: string }[];
  explanation: string;
  created_at: string;
  superseded_at: string | null;
  retracted_at: string | null;
  status_reason: string | null;
  caused_by: string | null;
}

export interface CertificateQuery {
  conclusion_id?: string;
  subject_id?: string;
  artifact_id?: string;
  reason?: string;
  outcome?: CertificateOutcome;
  kind?: string;
  live?: boolean;
  limit?: number;
}

export async function listCertificates(q: CertificateQuery): Promise<Certificate[]> {
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) {
    if (v !== undefined && v !== null && v !== "") params.set(k, String(v));
  }
  const body = await getJson<{ items: Certificate[] }>(`/certificates?${params}`);
  return body.items;
}

export function getCertificate(id: string): Promise<Certificate> {
  return getJson(`/certificates/${id}`);
}

/** A person says two photos are not copies; Gather never regroups them. */
export function markNotDuplicate(
  imageId: string,
  otherId: string,
): Promise<{ certificate: string }> {
  return postJson(`/images/${imageId}/not-duplicate`, { other_id: otherId });
}

export interface RetractionReport {
  event_certificate: string | null;
  units_retracted: string[];
  certificates_withdrawn: string[];
  contradictions_withdrawn: number;
  supersessions_reverted: number;
  images_ungrouped: number;
  merges_withdrawn: number;
  deleted: boolean;
  external_file_left: string | null;
}

/** Stop a source from supporting anything (optionally deleting it). */
export function retractArtifact(
  id: string,
  reason?: string,
  del = false,
): Promise<RetractionReport> {
  return postJson(`/artifacts/${id}/retract`, { reason: reason || null, delete: del });
}

// --- Explained away: flagged pairs Gather decided aren't contradictions ----

export interface ExplainedClaim {
  id: string;
  statement: string;
  status: string;
  valid_from: string | null;
  superseded_by_unit_id: string | null;
}

export interface ExplainedAway {
  certificate_id: string;
  unit_a: ExplainedClaim;
  unit_b: ExplainedClaim;
  /** Why it wasn't reported, in plain language. */
  reasons: { code: string; text: string }[];
  explanation: string;
  detection_method: string;
  score: number;
  decided_at: string;
}

export function listExplainedAway(limit = 100): Promise<{ items: ExplainedAway[]; total: number }> {
  return getJson(`/contradictions/explained-away?limit=${limit}`);
}

/** "This is a real conflict": reported, and never explained away again. */
export function confirmExplainedAway(
  certificateId: string,
  note?: string,
): Promise<{ contradiction_id: string; supersessions_reverted: number }> {
  return postJson(`/contradictions/explained-away/${certificateId}/confirm`, {
    note: note || null,
  });
}

/** "The explanation is right": the pair counts as not a conflict. */
export function agreeExplainedAway(certificateId: string, note?: string): Promise<unknown> {
  return postJson(`/contradictions/explained-away/${certificateId}/agree`, {
    note: note || null,
  });
}

// --- Projects: a folder or .zip kept as its tree ---------------------------

export interface ProjectSummary {
  id: string;
  name: string;
  source: "folder" | "zip";
  created_at: string;
  updated_at: string;
  folders: number;
  files: number;
  ingested: number;
  deduplicated: number;
  /** Files kept as they are, without text read from them. */
  stored: number;
  skipped: number;
  failed: number;
  /** Folders left out whole (version control, dependencies, caches). */
  left_out: number;
  bytes: number;
}

export interface ProjectItem {
  id: string;
  parent_id: string | null;
  item_kind: "folder" | "file";
  name: string;
  path: string;
  depth: number;
  /** A folder is "folder", or "skipped" when left out whole. */
  status: "folder" | "ingested" | "deduplicated" | "stored" | "skipped" | "failed";
  detail: string | null;
  byte_size: number | null;
  artifact_id: string | null;
  artifact_kind: string | null;
  /** Statements extracted from the file so far. */
  units: number;
}

export interface ProjectDetail extends ProjectSummary {
  items: ProjectItem[];
}

/** What happened to one file sent to a project. */
export interface ProjectFileResult {
  path: string;
  /** "left_out" is a folder left out whole; `path` is the folder. */
  status: "ingested" | "deduplicated" | "stored" | "skipped" | "failed" | "left_out" | "ignored";
  kind: string | null;
  artifact_id: string | null;
  detail: string | null;
  segments: number;
}

export function createProject(name: string): Promise<ProjectSummary> {
  return postJson("/projects", { name });
}

export interface ProjectFilesResponse {
  project_id: string;
  job_id: string;
  /** One result per file; a .zip unpacked in place gives one per file in it. */
  files: ProjectFileResult[];
  /** Why unpacking a .zip stopped early, if it did. */
  stopped: string | null;
}

/** Send one file to a project at `path` (relative to the project folder). */
export function uploadProjectFile(
  projectId: string,
  path: string,
  file: File,
): Promise<ProjectFilesResponse> {
  const form = new FormData();
  form.append("path", path);
  form.append("file", file, file.name);
  return postForm(`/projects/${projectId}/files`, form);
}

/** Tell a project what a folder held that wasn't sent: folders left out
 *  whole (`.git`, `node_modules`, …), secret-looking files withheld unread,
 *  and empty folders. The tree then shows all of it. */
export async function describeProject(
  projectId: string,
  held: { leftOut: string[]; withheld: string[]; folders: string[] },
): Promise<ProjectFileResult[]> {
  const form = new FormData();
  for (const folder of held.leftOut) form.append("left_out", folder);
  for (const path of held.withheld) form.append("withheld", path);
  for (const folder of held.folders) form.append("folder", folder);
  const body = await postForm<ProjectFilesResponse>(`/projects/${projectId}/files`, form);
  return body.files;
}

export interface ImportReport {
  project: ProjectSummary;
  files: ProjectFileResult[];
  stopped: string | null;
}

/** Unpack a .zip into a new project, folders and all. */
export function importProjectZip(file: File, name?: string): Promise<ImportReport> {
  const form = new FormData();
  if (name) form.append("name", name);
  form.append("file", file, file.name);
  return postForm("/projects/import", form);
}

export async function listProjects(): Promise<ProjectSummary[]> {
  const body = await getJson<{ items: ProjectSummary[] }>("/projects");
  return body.items;
}

export function getProject(id: string): Promise<ProjectDetail> {
  return getJson(`/projects/${id}`);
}

/** One project as a graph: its folders and files, what they mention, and
 *  the projects most like it. Same shape as the whole-collection graph. */
export function getProjectGraph(
  id: string,
  maxFiles = 250,
  maxEntities = 80,
): Promise<GraphOverview> {
  return getJson(`/projects/${id}/graph?max_files=${maxFiles}&max_entities=${maxEntities}`);
}

export interface SimilarProject {
  project_id: string;
  name: string;
  /** 0 to 1. */
  score: number;
  /** Each signal's score, or null when it doesn't apply to both projects. */
  signals: {
    files: number | null;
    layout: number | null;
    entities: number | null;
    content: number | null;
  };
  shared: {
    files: number;
    paths: number;
    path_examples: string[];
    entities: { id: string; name: string }[];
    terms: string[];
    content_by: "meaning" | "words" | null;
  };
  /** Plain-language reasons, strongest first. */
  reasons: string[];
}

/** The projects most like `id`, best first. */
export async function getSimilarProjects(id: string, limit = 10): Promise<SimilarProject[]> {
  const body = await getJson<{ items: SimilarProject[] }>(`/projects/${id}/similar?limit=${limit}`);
  return body.items;
}

/** Remove a project's tree; its files stay in Gather. */
export async function deleteProject(id: string): Promise<void> {
  const res = await fetch(`${DAEMON_URL}/api/v1/projects/${id}`, {
    method: "DELETE",
    headers: authHeaders(),
  });
  if (!res.ok) await jsonOrThrow(res);
}
