# API reference

Gather exposes the same data over two local APIs:

| API | Default address | Contract |
|---|---|---|
| REST (JSON) | `http://127.0.0.1:7601/api/v1` | this document |
| gRPC | `127.0.0.1:7602` | [`proto/gather/v1/gather.proto`](../proto/gather/v1/gather.proto) |

Both bind to loopback only. The REST and gRPC handlers share the same core functions, so
behavior is identical across the two.

## Conventions

**Authentication.** When a token is configured (`GATHER_API_TOKEN`, or `GATHER_AUTH_MODE=keychain`),
every `/api/v1` request and every gRPC call must carry:

```
Authorization: Bearer <token>
```

In keychain mode, print the token with `gather-daemon print-api-token`. Health and metrics
endpoints are never authenticated. With no token configured the API is open (loopback only)
and the daemon logs a warning at startup.

**Rate limiting.** A global limit (`GATHER_RATE_LIMIT_RPS`, default 50 req/s) is shared by REST
and gRPC. Exceeding it returns `429 Too Many Requests`.

**Errors.** Non-2xx responses have a uniform body:

```json
{ "error": { "code": "not_found", "message": "unit 3f…" } }
```

| Status | `code` |
|---|---|
| 400 | `bad_request` |
| 401 | `unauthorized` |
| 404 | `not_found` |
| 429 | `too_many_requests` |
| 500 | `database_error`, `internal_error` (details are logged, never returned) |

**Pagination.** List endpoints accept `limit` and `offset` query parameters.

**CORS.** Browser access is allowed only from the Tauri webview and the Vite dev server
(`http://localhost:1420`); methods `GET`, `POST`, `PATCH`.

---

## Health & metrics (no auth, not under `/api/v1`)

| Method | Path | Description |
|---|---|---|
| GET | `/healthz` | Liveness |
| GET | `/readyz` | Readiness: database reachable and pgvector loaded |
| GET | `/metrics` | Prometheus metrics |

---

## Ingestion

### `POST /ingest/chat-export`

Ingest an AI assistant's conversation export.

```json
{
  "platform": "chatgpt",
  "data": { "...": "the platform's raw export JSON" },
  "filename": "conversations.json"
}
```

`platform` is one of `chatgpt`, `claude`, `gemini`, `grok`, `perplexity`, `copilot`, `generic`.

For agent logs (below), `platform` is a free-form label recorded as the source, e.g. `claude_code`, `goose`, `aider` or `generic`.
Re-ingesting the same content is deduplicated by content hash.

### `POST /ingest/agent-log`

```json
{
  "platform": "claude_code",
  "jsonl": "{\"role\":\"user\",...}\n{\"role\":\"assistant\",...}",
  "session_id": "optional-session-id",
  "title": "optional title"
}
```

### `POST /ingest/files`

`multipart/form-data`, any number of file parts. PDFs, Markdown/text and images are
classified by extension and MIME type. Text and OCR extraction happen asynchronously in the
extraction worker.

```bash
curl -X POST $API/ingest/files -F file=@report.pdf -F file=@photo.jpg
```

Per-request size is capped by `GATHER_MAX_UPLOAD_MB`; larger requests get `413 payload_too_large`, refused from their declared `Content-Length` before the body is read. For large batches, send one file per request (as the desktop app does): memory then stays at one file, and one bad file doesn't fail the rest.

---

## Query

### `GET /artifacts`

Query: `kind` (`chat_export`, `agent_log`, `document_pdf`, `document_markdown`,
`document_text`, `image_photo`, `image_screenshot`), `source_platform`, `limit`, `offset`.

Each item also carries `unit_count` (live atomic units extracted from it) and `status`:
`processing` while text extraction, OCR or unit extraction is still pending, `failed` when
text extraction or OCR failed, else `done`.

### `GET /artifacts/{id}`

One artifact with its conversations/messages, document segments or image metadata, plus
`unit_count` and `status` as above.

### `GET /artifacts/{id}/content`

The artifact's readable text, in order: document segments, chat messages, or an image's OCR
text. Query: `limit` (default 50, max 200), `offset`.

```json
{ "source": "document", "total": 12,
  "items": [ { "seq": 0, "heading": "Notes", "page": null, "role": null, "text": "…" } ] }
```

`source` is `document`, `conversation`, `image` or `none`.

### `GET /atomic-units`

Query: `kind` (`fact`, `claim`, `decision`, `preference`, `event`), `status` (`active`,
`superseded`, `retracted`, `disputed`), `subject_entity_id`, `artifact_id` (units extracted
from that artifact), `limit`, `offset`.

### `GET /graph`

The whole collection at a glance: the most connected entities, the relationships among them,
and the files they were extracted from. Query: `max_entities` (default 150, max 1000),
`max_files` (default 100, max 1000; `0` leaves files out).

```json
{ "entities": [ { "id": "…", "name": "Me", "kind": "person", "weight": 16 } ],
  "files": [ { "id": "…", "name": "notes.md", "kind": "document_markdown", "mentions": 4 } ],
  "relations": [ { "source": "…", "target": "…", "relation_type": "works_at", "count": 1, "confidence": 0.6 } ],
  "mentions": [ { "file_id": "…", "entity_id": "…", "count": 2 } ],
  "entity_total": 10, "truncated": false }
```

An entity's `weight` is its relationships plus the units about it; entities with neither are
left out. `truncated` is true when more connected entities exist than were returned.

### `GET /entities/{id}/graph`

Bounded, cycle-safe neighborhood traversal. Query: `depth` (1–5, default 2), `max_edges`.

### `POST /search/semantic`

```json
{ "text": "backup target", "scope": "atomic_units", "limit": 10 }
```

- `scope`: `atomic_units` (default), `document_segments` or `messages`. Messages are full-text only; the others use embeddings when available.
- If Ollama is enabled, `text` is embedded server-side and results are ranked by cosine
  similarity. You can also pass a precomputed 768-dimension `embedding` instead of `text`.
- Without embeddings it falls back to Postgres full-text ranking.

Response: `{ "scope": "...", "hits": [ { "id": "...", "score": 0.83, ... } ] }`.

---

## Entities

| Method | Path | Description |
|---|---|---|
| GET | `/entities` | List. Query: `q` (name search), `kind`, `include_merged`, `limit`, `offset` |
| GET | `/entities/merge-suggestions` | Likely duplicate pairs. Query: `threshold` (0–1, default 0.6), `limit` |
| GET | `/entities/{id}` | Entity with aliases and merge history |
| POST | `/entities/{id}/merge` | Merge another entity into this one: `{ "loser_id": "…", "note": "…", "actor": "…" }` |
| POST | `/entities/{id}/merge-suggestions/dismiss` | Suppress a suggestion: `{ "other_id": "…", "note": "…" }` |
| POST | `/entities/{id}/aliases` | Add an alias: `{ "alias": "PG" }` |
| POST | `/entities/{id}/unmerge` | Split a merged-away entity back out: its units, edges, aliases and descendants are restored from the merge's journal, and the pair is never suggested or auto-merged again. Optional body `{ "note": "…", "actor": "…" }`. `404` if there is no merge to undo; `400` if it can't be undone exactly (the survivor has since been merged elsewhere, a later merge into the same survivor is still live, or the merge predates journaling) |

Merges are reversible: the losing entity is kept with `merged_into_entity_id` set, every merge
journals what it changed, and `unmerge` replays that journal in reverse. Every merge, unmerge
and dismissal is recorded in `entity_merge_audit`. Undoing an automatic or tray-accepted merge
also records a negative tuning label against the gate that admitted it, so that threshold learns
from wrong merges. Contradictions the merge surfaced between the two entities' units are
withdrawn, since they only existed because the pair was treated as one.
Entity kinds: `person`, `organization`, `project`, `tool`, `concept`, `location`, `event`,
`other`.

---

## Contradictions

| Method | Path | Description |
|---|---|---|
| GET | `/contradictions` | List. Query: `status` (`open`, `resolved_a`, `resolved_b`, `both_valid`, `dismissed`), `limit`, `offset` |
| GET | `/contradictions/{id}` | Detail: both units, score, detection method, explanation, audit trail |
| POST | `/contradictions/{id}/resolve` | `{ "resolution": "resolved_a", "note": "…", "actor": "…" }` |
| POST | `/contradictions/{id}/annotations` | `{ "note": "…" }` |

Resolving as `resolved_a` / `resolved_b` supersedes the losing unit, closes its validity window
and deactivates the relationships it asserted.

---

## Feedback loop & review tray

The rare corrections that keep the system honest. Every action is recorded in
`unit_feedback` and is reversible. See [AUTONOMOUS-PIPELINE.md](AUTONOMOUS-PIPELINE.md).

| Method | Path | Description |
|---|---|---|
| POST | `/units/{id}/reject` | Retract a unit and the relationships it asserted (negative label). Optional body `{ "note": "…" }` |
| POST | `/units/{id}/restore` | Undo a reject. Only valid for a `retracted` unit, otherwise `400` |
| POST | `/units/{id}/confirm` | Positive label; no state change |
| PATCH | `/units/{id}` | Correct the wording: `{ "statement": "…", "note": "…" }`. Stores before and after, marks the unit `manual`, and re-queues it for embedding, contradiction scanning and clustering. Returns `400` if the new text duplicates another unit |
| GET | `/review` | The optional review tray, highest information gain first. Query: `limit` |
| POST | `/review/{id}/accept` | Agree with a held item: keep a unit, or perform a held merge (the more specific / longer-named entity survives). Records a positive tuning label |
| POST | `/review/{id}/reject` | Disagree: retract a unit, or dismiss a held merge pair so it is never suggested again. Records a negative tuning label |
| POST | `/review/{id}/resolve` | Dismiss a tray item without acting on it (no label) |

Acting on a unit (reject, restore, confirm or edit) also clears its open tray entry.

Oversized entity components cannot be accepted as a whole (`400`); act on their members
individually.

---

## Tuning

| Method | Path | Description |
|---|---|---|
| GET | `/tuning` | Thresholds in force with their defaults and hard bounds, the tuner settings, and the 50 most recent changes with the evidence behind each |
| POST | `/tuning/reset` | Drop learned values so the env defaults apply again. Optional body `{ "key": "admit.hold_below" }` resets one key. Audited and durable: only verdicts given after the reset can tune that key again |

Keys: `admit.hold_below` (unit admission), `merge.auto_single` (single-signal auto-merge) and
`merge.agree` (auto-merge when name and embedding similarity both agree).

---

## Clusters

| Method | Path | Description |
|---|---|---|
| GET | `/clusters` | List. Query: `kind` (`topic`, `entity`, `photo_dup`, `album` or `photo_topic`), `limit`. Each item carries its `representative_id` when it has one |
| GET | `/clusters/{id}` | Cluster with members. Unit members include their statement; image members their file name, capture time and caption |

---

## Photos

Near-duplicate groups (`photo_dup`), albums (`album`) and visual topics (`photo_topic`) are
clusters, browsed with the endpoints above. The representative of a duplicate group is its
sharpest (then earliest) copy; of an album, its first shot.

| Method | Path | Description |
|---|---|---|
| GET | `/images/{id}/thumbnail` | JPEG, at most 256 px on its long side, rendered locally. `415` when the format can't be decoded locally (e.g. HEIC) |

---

## Export & import

| Method | Path | Description |
|---|---|---|
| GET | `/export` | The whole store as a `gather-bundle-v1` NDJSON stream (`application/x-ndjson`) |
| POST | `/import` | Import a bundle. Idempotent: existing rows are kept |

The bundle includes artifacts, extracted units, the graph, contradictions, merge history, the
feedback and review state, and clusters, so a restore reproduces the whole brain. It is used
by the backup scripts and restore drills (see [BACKUP-RUNBOOK.md](BACKUP-RUNBOOK.md)).

---

## gRPC

Nine services in package `gather.v1`, served with the same bearer-token interceptor. Each RPC
calls the same core function as its REST route:

| Service | RPCs |
|---|---|
| `IngestService` | `IngestChatExport`, `IngestAgentLog`, `IngestFile` (client streaming) |
| `QueryService` | `ListArtifacts`, `GetArtifact`, `GetArtifactContent`, `ListAtomicUnits`, `GetEntityGraph`, `GetGraphOverview`, `SemanticSearch` |
| `ContradictionService` | `ListContradictions`, `GetContradiction`, `ResolveContradiction`, `AnnotateContradiction` |
| `EntityService` | `ListEntities`, `ListMergeSuggestions`, `GetEntity`, `MergeEntities`, `UnmergeEntity`, `DismissMergeSuggestion`, `AddAlias` |
| `ExportService` | `ExportBundle` (server streaming), `ImportBundle` (client streaming) |
| `FeedbackService` | `RejectUnit`, `RestoreUnit`, `ConfirmUnit`, `EditUnit`, `ListReview`, `AcceptReview`, `RejectReview`, `ResolveReview` |
| `ClusterService` | `ListClusters`, `GetCluster` |
| `TuningService` | `GetTuning`, `ResetTuning` |
| `PhotoService` | `GetThumbnail` |

```bash
grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -import-path proto -proto gather/v1/gather.proto \
  127.0.0.1:7602 gather.v1.QueryService/ListArtifacts
```
