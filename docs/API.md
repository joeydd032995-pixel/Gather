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

Per-request size is capped by `GATHER_MAX_UPLOAD_MB`.

---

## Query

### `GET /artifacts`

Query: `kind` (`chat_export`, `agent_log`, `document_pdf`, `document_markdown`,
`document_text`, `image_photo`, `image_screenshot`), `source_platform`, `limit`, `offset`.

### `GET /artifacts/{id}`

One artifact with its conversations/messages, document segments or image metadata.

### `GET /atomic-units`

Query: `kind` (`fact`, `claim`, `decision`, `preference`, `event`), `status` (`active`,
`superseded`, `retracted`, `disputed`), `subject_entity_id`, `limit`, `offset`.

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

Merges are reversible in the data model: the losing entity is kept with
`merged_into_entity_id` set, and every merge or dismissal is recorded in `entity_merge_audit`.
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
| POST | `/tuning/reset` | Drop learned values so the env defaults apply again. Optional body `{ "key": "admit.hold_below" }` resets one key. Audited |

Keys: `admit.hold_below` (unit admission) and `merge.auto_single` (single-signal auto-merge).

---

## Clusters

| Method | Path | Description |
|---|---|---|
| GET | `/clusters` | List. Query: `kind` (`topic` or `entity`), `limit` |
| GET | `/clusters/{id}` | Cluster with members (unit members include their statement) |

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

Five services in package `gather.v1`, served with the same bearer-token interceptor:

| Service | RPCs |
|---|---|
| `IngestService` | `IngestChatExport`, `IngestAgentLog`, `IngestFile` (client streaming) |
| `QueryService` | `ListArtifacts`, `GetArtifact`, `ListAtomicUnits`, `GetEntityGraph`, `SemanticSearch` |
| `ContradictionService` | `ListContradictions`, `GetContradiction`, `ResolveContradiction`, `AnnotateContradiction` |
| `EntityService` | `ListEntities`, `ListMergeSuggestions`, `GetEntity`, `MergeEntities`, `DismissMergeSuggestion`, `AddAlias` |
| `ExportService` | `ExportBundle` (server streaming), `ImportBundle` (client streaming) |

The feedback and cluster endpoints are REST-only for now; gRPC parity is on the roadmap.

```bash
grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -import-path proto -proto gather/v1/gather.proto \
  127.0.0.1:7602 gather.v1.QueryService/ListArtifacts
```
