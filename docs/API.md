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

## Status

### `GET /status`

What the daemon runs with and how far reading has got. Used by the desktop app's Settings page.

```json
{
  "version": "0.1.0",
  "ai": { "enabled": true, "url": "http://127.0.0.1:11434", "model": "llama3.2:1b",
          "embed_model": "nomic-embed-text" },
  "reading": { "chunks": 1840, "files": 212, "failed": 3 }
}
```

`reading.chunks` counts document sections, chat messages and image text not yet read into
units; `reading.files` the files they belong to (plus files not yet opened); `reading.failed`
the sections set aside after an error (the error is in the daemon log). `ai.model` is `null`
when Ollama is used for embeddings only; `ai.enabled` is `false` without Ollama.

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

`multipart/form-data`, any number of file parts. Files are classified by extension and MIME
type:

| Kind | Files | Read |
|---|---|---|
| `document_pdf` | `.pdf` | text layer, then OCR, in the extraction worker |
| `document_markdown` | `.md`, `.markdown` | as is |
| `document_docx` | `.docx` | paragraphs and table cells of the Word document |
| `document_spreadsheet` | `.xlsx`, `.xlsm`, `.xls`, `.ods` | every sheet, one row per line, cells separated by tabs |
| `document_text` | `.txt`, `.csv`, `.tsv`, `.json`, `.yaml`, `.xml`, `.html`, `.log`, source code (`.rs`, `.py`, `.ts`, `.go`, `.java`, `.sql`, `.sh`, …) | as text; HTML without its tags, scripts and styles |
| `image` | `.jpg`, `.png`, `.webp`, `.heic`, … | EXIF, then OCR, in the extraction worker |
| `file_other` | anything, when the part is named `file_other` | nothing: the file is kept as it is |

Word documents and spreadsheets are turned into text when they arrive; a damaged one, or one
that unpacks to far more than its size (over 256 MB, or 200× its compressed size), is refused
with `400`. A file whose extension says text but whose content is binary is refused with `400`,
and a file Gather can't read at all with `415`. Statements are pulled out asynchronously by the
extraction worker.

```bash
curl -X POST $API/ingest/files -F file=@report.pdf -F file=@photo.jpg
```

Per-request size is capped by `GATHER_MAX_UPLOAD_MB`; larger requests get `413 payload_too_large`, refused from their declared `Content-Length` before the body is read. For large batches, send one file per request (as the desktop app does): memory then stays at one file, and one bad file doesn't fail the rest.

---

## Projects

A project is a folder — of documents, a code repository, `.zip` files, anything — uploaded as
a whole and kept as the tree it came in: the project, the folders in it, and the files in
those. Every file counts. Each is stored exactly as a single upload would be (so it is
deduplicated by content, read, and linked into the graph like any other file); the project
records where each file sat and what happened to it.

- A file of a kind Gather reads is read. One whose kind Gather doesn't know by name is read as
  text when its content is text (a `Makefile`, `.gitignore`, a config file by another name).
- Anything else — a program, a design file, audio, a `.7z` — is **kept as it is**
  (`file_other`, status `stored`), and so is a known kind whose text couldn't be read (a
  damaged Word file), with why in `detail`.
- A `.zip` in a project is **unpacked where it sits**, as a folder of the same name
  (`archive/drafts.zip/…`); one holding a single folder is unpacked without repeating it. Zips
  inside zips are unpacked too, up to three deep; a fourth is kept as it is. One that can't be
  unpacked is kept as it is.

| Status | Meaning |
|---|---|
| `ingested` | Read into Gather |
| `deduplicated` | The same content was already in Gather; the project links to that file |
| `stored` | Kept as it is, without text read from it |
| `skipped` | Not kept, with the reason in `detail`: an empty or oversized file, a clash with the tree, or a file that looks like it holds keys or passwords (`.env`, `id_rsa`, `.pem`, `.key`, …), which is never opened |
| `failed` | It couldn't be stored; sending it again retries |

Folders that aren't the project's own work are **left out whole** and appear in the tree as a
folder with status `skipped` and the reason in `detail`: version-control history (`.git`,
`.hg`, `.svn`), installed dependencies (`node_modules`, `bower_components`, `.venv`, `venv`)
and tool caches (`__pycache__`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.tox`,
`.gradle`, `.next`, `.nuxt`, `.terraform`). Only operating-system clutter — `.DS_Store`,
`Thumbs.db`, `desktop.ini`, `._*` files and `__MACOSX` folders — is dropped without a record.

| Method | Path | Notes |
|---|---|---|
| GET | `/projects` | `{ "items": [ProjectSummary] }`, most recently changed first |
| POST | `/projects` | `{ "name": "Atlas" }` → `201` with the new, empty project |
| POST | `/projects/{id}/files` | Add files; see below → `202` |
| POST | `/projects/import` | Unpack a `.zip` into a new project; see below → `201` |
| GET | `/projects/{id}` | The project and every folder and file in it, ordered by path |
| DELETE | `/projects/{id}` | Removes the project and its tree → `204`. Its files stay in Gather |

A `ProjectSummary` is `{ id, name, source ("folder" or "zip"), created_at, updated_at, files,
folders, ingested, deduplicated, stored, skipped, failed, left_out, bytes }`, where `skipped`
counts files and `left_out` folders left out whole. Each item of `GET /projects/{id}`
is `{ id, parent_id, item_kind ("folder" or "file"), name, path, depth, status, detail,
artifact_id, artifact_kind, units, byte_size }`, where `units` counts the statements found in
the file so far.

### `POST /projects/{id}/files`

`multipart/form-data`. Each file part may be preceded by a text part named `path` giving the
file's path inside the project; without one the part's file name is used. Folders are created
as needed. Text parts describe what the sender didn't send:

- `left_out` — a folder left out whole instead of sending its files
  (`-F left_out=web/node_modules`); only the folder names above are taken.
- `withheld` — a file not sent because it looks like it holds keys or passwords
  (`-F withheld=.env`), so its bytes never leave the sender; it is listed as skipped. Only paths
  Gather wouldn't read are taken.
- `folder` — an empty folder, so the tree keeps it.

Anything else in those parts comes back `skipped` and isn't recorded. Requests adding to the
same project are handled one at a time. Sending a path again replaces that file's record, so a partly failed upload can be
resumed. Paths are relative and `/`-separated; absolute paths, drive letters, `..` and control
characters are skipped per file, as is a file where the project already has a folder, or one
inside what the project already has as a file; such a file isn't stored.

```bash
curl -X POST $API/projects -H 'content-type: application/json' -d '{"name":"Atlas"}'
curl -X POST $API/projects/$ID/files \
  -F path=docs/plan.md -F file=@Atlas/docs/plan.md \
  -F path=data/budget.xlsx -F file=@Atlas/data/budget.xlsx
```

The response is `{ project_id, job_id, files: [{ path, status, kind, artifact_id, detail,
segments }], stopped }`: one result per file, so a `.zip` gives one for each file in it, and
`left_out` for each folder left out (its `path` is the folder). `stopped` says why unpacking
stopped early, or is `null`. As with `/ingest/files`, one file per request keeps memory at one file; the
desktop app sends a picked or dropped folder that way.

### `POST /projects/import`

`multipart/form-data` with one `.zip` part and, optionally, a text part `name`. Without a name
the project is named after the folder the archive holds, or else the archive. Entries are
unpacked one at a time, and zips inside it are unpacked in place as above. Limits, shared by
every zip in the request however deeply nested: `GATHER_PROJECT_MAX_FILES` files,
`GATHER_MAX_UPLOAD_MB` per file and `GATHER_PROJECT_MAX_MB` in all; an entry that would unpack
to more than 200× its compressed size is skipped. Links inside the archive are skipped.

```bash
curl -X POST $API/projects/import -F file=@Atlas.zip
```

The response is `{ project: ProjectSummary, files: [...], stopped }`, where `stopped` says why
unpacking ended early (a limit reached), or is `null`.

### `GET /projects/{id}/similar`

The projects most like this one, best first. Query: `limit` (default 10, max 50). Two projects
are compared on four signals, each from 0 to 1:

| Signal | What it compares |
|---|---|
| `files` | Identical files (the same content) in both |
| `layout` | Files at the same paths (`src/main.rs`, `docs/plan.md`), which matches two versions of one repository even when every file changed |
| `entities` | The same people, organisations, tools and places mentioned |
| `content` | What the text is about: the average embedding when embeddings are on, otherwise the words used most |

Overlaps are weighted by rarity across projects, so what every project has (`README.md`,
`LICENSE`, your own name, common words) counts for little. A signal is `null` when either
project has nothing for it (a folder of photos has no text), and the score is the weighted
average of the signals that apply (files 0.3, layout 0.2, entities 0.3, content 0.2). Projects
scoring under 0.08 are left out. The most recently changed projects are compared, 5,000 by
default (`GATHER_PROJECT_COMPARE_MAX`); what they are made of is cached until a project changes
or more is read from its files. Each is kept as a few kilobytes however large: files, paths and
entities as hashes, and past 256 files or paths (128 entities) a consistent sample of them, so
counts for large projects are estimates (`shared.approximate`, and reasons that start "About").
`path_examples` are the first shared paths in alphabetical order.

```json
{ "project_id": "…",
  "items": [ { "project_id": "…", "name": "Orbit tracker v2", "score": 0.588,
    "signals": { "files": 0.1, "layout": 0.62, "entities": 0.5, "content": 0.71 },
    "shared": { "files": 1, "paths": 3, "path_examples": ["cargo.toml", "docs/spec.md"],
                "entities": [ { "id": "…", "name": "Dana Reyes" } ],
                "terms": ["orbit", "tracker"], "content_by": "words" },
    "reasons": [ "Both mention Dana Reyes", "3 files at the same place (cargo.toml, docs/spec.md and src/main.rs)", "1 identical file in common" ] } ] }
```

### `GET /projects/{id}/graph`

One project as a graph, in the same shape as [`GET /graph`](#get-graph): the project, the
folders and files in it (`contains` links), the entities its files mention and the
relationships among them, and up to five similar projects (`similar` links). Query:
`max_files` (default 250, max 2000; files with the most read from them come first, with the
folders above them) and `max_entities` (default 80, max 1000). A file stored at two paths is one
node held by both folders. `truncated` is true when files or entities were left out.

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
from that artifact), `live=true` (only `active` or `disputed` units, the ones that still count as
knowledge), `limit`, `offset`.

### `GET /graph`

The whole collection at a glance: the most connected entities, the relationships among them,
the files they were extracted from, and your projects. Query: `max_entities` (default 150,
max 1000), `max_files` (default 100, max 1000; `0` leaves files out), `max_projects` (default
150, max 1000; `0` leaves projects out; `projects=false` does the same).

```json
{ "entities": [ { "id": "…", "name": "Me", "kind": "person", "weight": 16 } ],
  "files": [ { "id": "…", "name": "notes.md", "kind": "document_markdown", "mentions": 4 } ],
  "relations": [ { "source": "…", "target": "…", "relation_type": "works_at", "count": 1, "confidence": 0.6 } ],
  "mentions": [ { "file_id": "…", "entity_id": "…", "count": 2 } ],
  "projects": [ { "id": "…", "name": "Atlas", "source": "folder", "files": 12 } ],
  "folders": [],
  "contains": [ { "parent_type": "project", "parent": "…", "child_type": "file", "child": "…" } ],
  "similar": [ { "a": "…", "b": "…", "score": 0.59, "reasons": ["3 files at the same place (…)"] } ],
  "entity_total": 10, "truncated": false }
```

Projects are the `max_projects` most recently changed. `contains` links a project to those of
the returned `files` it holds (the overview has no folders; a project's own graph does).
`similar` links projects that are alike, up to three per project, with why; pairs are compared
among the projects returned, weighted by rarity across all compared projects, and remembered
until a project changes. gRPC's `GetGraphOverview` returns
entities and files only.

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
| GET | `/contradictions/explained-away` | Pairs that looked contradictory but were not reported, with the reason (`TEMPORAL_SUCCESSION`, `CONTEXT_SCOPE_MISMATCH`, …). Query: `limit`, `offset`. Returns `{ items, total }` |
| POST | `/contradictions/explained-away/{certificate}/confirm` | "It's a real conflict": `{ "note": "…" }` (optional). Opens the contradiction, undoes a supersession between the two claims, and keeps later scans from explaining it away. Returns `{ contradiction_id, certificate, supersessions_reverted }` |
| POST | `/contradictions/explained-away/{certificate}/agree` | "The explanation is right": `{ "note": "…" }` (optional). The pair counts as not a conflict, and an open contradiction still left for it is closed. Returns `{ certificate, contradictions_closed }`. `404` once a pair has been reviewed |

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

## Semantic safety

Every automatic conclusion — and every one Gather held back — has an inference certificate
(see [SEMANTIC-SAFETY.md](SEMANTIC-SAFETY.md)).

| Method | Path | Description |
|---|---|---|
| GET | `/certificates` | List, newest first. Query (all optional, combined): `conclusion_id` (the conclusion's row — a contradiction, a cluster, a unit — or any id it is about), `subject_id`, `artifact_id` (conclusions derived from that source), `reason` (a reason code, e.g. `CHAINED_SIMILARITY`; `400` for an unknown code), `outcome` (`auto_applied`, `needs_review`, `blocked`, `user_decision`, `superseded`, `retracted`), `kind`, `rule`, `live` (only certificates still in force), `limit` (≤ 500), `offset` |
| GET | `/certificates/{id}` | One certificate: rule and version, decision and current outcome, inputs with their evidence class, sources, config, scope, time interpretation, predicates, reason codes with plain-language `reasons`, explanation, and withdrawal (`superseded_at`/`retracted_at`, `status_reason`, `caused_by`) |
| GET | `/certificates/{id}/chain` | The certificate, what caused its withdrawal (transitively), every certificate for the same conclusion (its history) and what it caused |
| GET | `/certificates/{id}/affected` | Conclusions withdrawn because of this one (a split, a "not a duplicate", a removed source) |
| GET | `/safety/summary` | Counts by outcome, review-routed and blocked counts by reason code, live certificates by kind |
| GET | `/artifacts/{id}/conclusions` | Certificates derived from a source (same filters as `/certificates`) |
| POST | `/artifacts/{id}/retract` | Stop a source from supporting anything: `{ "reason": "…", "delete": false }`. Units only it supported are retracted with their edges; what rested on them is withdrawn. Returns the retraction report |
| DELETE | `/artifacts/{id}` | Retract, then delete the artifact and the bytes stored in the database. Artifacts it linked (derivations, versions) stay in one source family. A `storage_path` file (only ever set by an imported bundle) is not removed; its path is returned as `external_file_left` |
| POST | `/artifacts/{id}/derivations` | Declare that this artifact derives from another: `{ "parent_id": "…", "kind": "copy|summary|export|reingest|version|correction|other" }`. It no longer counts as independent corroboration |
| GET | `/units/{id}/support` | The unit's live sources, their source families, independent-source count and the confidence independent support justifies |
| POST | `/units/{id}/revisions` | Record a newer extractor's reading: `{ "statement": "…", "value": "…", "model_version": "…" }`. Disagreement is recorded and parked for review (`model-disagreement`); the unit is never rewritten |
| POST | `/images/{id}/not-duplicate` | `{ "other_id": "…", "note": "…" }`. The two photos are never grouped as duplicates again |

A retraction report:

```json
{
  "event_certificate": "…",
  "units_retracted": ["…"],
  "certificates_withdrawn": ["…"],
  "contradictions_withdrawn": 1,
  "supersessions_reverted": 0,
  "images_ungrouped": 0,
  "merges_withdrawn": 0,
  "deleted": false,
  "external_file_left": null
}
```

Contradictions carry `certificate_id`, `alignment` (subject, predicate, unit, value, scope,
granularity, time) and `certainty` (`aligned` or `needs_review`) in the database and export
bundle. New review-tray reasons: `generic-identifier`, `withdraw-merge` (both dismiss-only),
`modality-uncertain` and `model-disagreement` (unit items). Parked entity items carry
`signals.certificate` and `signals.reasons`.

---

## Export & import

| Method | Path | Description |
|---|---|---|
| GET | `/export` | The whole store as a `gather-bundle-v1` NDJSON stream (`application/x-ndjson`) |
| POST | `/import` | Import a bundle. Idempotent: existing rows are kept |

The bundle includes artifacts, extracted units, the graph, contradictions, merge history, the
feedback and review state, clusters, inference certificates, declared source derivations,
"not a duplicate" decisions and projects, so a restore reproduces the whole brain. It is used
by the backup scripts and restore drills (see [BACKUP-RUNBOOK.md](BACKUP-RUNBOOK.md)).

---

## gRPC

Ten services in package `gather.v1`, served with the same bearer-token interceptor. Each RPC
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
| `SafetyService` | `GetCertificate`, `ListCertificates`, `ListAffected`, `GetSafetySummary`, `RetractArtifact`, `MarkNotDuplicate` |

```bash
grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -import-path proto -proto gather/v1/gather.proto \
  127.0.0.1:7602 gather.v1.QueryService/ListArtifacts
```
