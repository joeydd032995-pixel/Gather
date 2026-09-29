# Configuration reference

The daemon is configured only through environment variables. With Docker Compose, put them in
`.env` (copy `.env.example`): Compose forwards them into the daemon container. When running the
binary directly (`cargo run`), export them in your shell.

Rules the daemon applies at startup:

- Unset variables use their default.
- Variables marked **validated** make startup fail on a malformed or out-of-range value. For
  example, `NaN` for a threshold, or `GATHER_ADMIT_DROP_BELOW` greater than
  `GATHER_ADMIT_HOLD_BELOW`.
- Other numeric variables are **clamped** to their range.

The packaged desktop app starts the daemon itself and sets `DATABASE_URL` (its bundled
database) and `GATHER_AUTH_MODE=keychain` unless you set `GATHER_AUTH_MODE` yourself. Any other
variable in the app's environment reaches the daemon unchanged. See [Desktop app](#desktop-app).

## Docker Compose / Postgres

These are read by `docker-compose.yml`, not by the daemon itself.

| Variable | Default | Description |
|---|---|---|
| `POSTGRES_PASSWORD` | — | **Required.** Generate with `openssl rand -hex 24` |
| `POSTGRES_DB` | `gather` | Database name |
| `POSTGRES_USER` | `gather` | Database user |
| `POSTGRES_PORT` | `5432` | Host port for Postgres (loopback only) |
| `GATHER_PORT` | `7601` | Host port for the REST API (loopback only) |
| `GATHER_GRPC_PORT` | `7602` | Host port for the gRPC API (loopback only) |
| `GRAFANA_ADMIN_PASSWORD` | `admin` | Grafana admin password (`--profile observability`) |

## Core daemon

| Variable | Default | Description |
|---|---|---|
| `GATHER_MEMORY_PROFILE` | `standard` | `low` for machines with about 4 GB of RAM. It lowers the defaults marked *low:* on this page; any variable you set explicitly still wins. The desktop app sets it automatically on machines under 6 GB ([INSTALL.md](INSTALL.md#system-requirements)). **Validated** |
| `DATABASE_URL` | — | **Required** when running the binary directly (Compose builds it for you). `postgres://user:pass@host:port/db` |
| `GATHER_BIND_ADDR` | `127.0.0.1:7601` | REST listen address. A non-loopback address is refused unless `GATHER_ALLOW_NON_LOOPBACK=true` |
| `GATHER_ALLOW_NON_LOOPBACK` | `false` | Explicit opt-out of the loopback-only policy (for both listeners and the Ollama URL). Containers set this; desktops should not |
| `GATHER_DB_MAX_CONNECTIONS` | `8` (low: `4`) | Shared Postgres pool size. **Validated**, ≥ 1 |
| `GATHER_MAX_UPLOAD_MB` | `256` (low: `32`) | Maximum request body size, and the per-file limit for gRPC `IngestFile` streams (`RESOURCE_EXHAUSTED`). A request that declares a larger size is refused with `413` before any of it is read. The desktop app checks a picked file's size against the same limit before reading it. Storing a file briefly costs the daemon about 3.5× its size (the upload, plus the database driver's encoded copy of it), so keep this low on small-memory machines |
| `GATHER_PROJECT_MAX_FILES` | `20000` | Most files unpacked from `.zip` files in one request (a project `.zip` and the zips inside it together); the rest are reported as not read (`stopped`) |
| `GATHER_PROJECT_COMPARE_MAX` | `5000` (low: `1000`) | Most projects compared when finding similar ones (the most recently changed). Each takes a few kilobytes while cached, at most about 10 KB for a large one |
| `GATHER_PROJECT_MAX_MB` | `4096` (low: `1024`) | Most data unpacked from `.zip` files in one request, in MB. Each file is also held to `GATHER_MAX_UPLOAD_MB`, and one that unpacks to over 200× its compressed size is skipped |
| `GATHER_RATE_LIMIT_RPS` | `50` | Global requests/second shared by REST and gRPC; `0` disables. **Validated** |
| `RUST_LOG` | `info,sqlx=warn,tower_http=info` | Log filter (tracing `EnvFilter` syntax) |
| `GATHER_LOG_JSON` | `false` | Emit JSON logs |

## Desktop app

Read by the packaged app's supervisor, not the daemon ([INSTALL.md](INSTALL.md)).

| Variable | Default | Description |
|---|---|---|
| `GATHER_PG_PORT` | `7603` | Loopback port of the bundled PostgreSQL. Change it only if another program uses 7603 |
| `GATHER_MEMORY_PROFILE` | `auto` | `auto` picks `low` below 6 GB of RAM and `standard` otherwise; `low` or `standard` forces one. The app passes the result to the daemon and, in `low`, starts PostgreSQL with `shared_buffers=64MB`, `work_mem=2MB`, `maintenance_work_mem=32MB`, `max_connections=12`, one autovacuum worker, no parallel query workers and JIT off |

## Authentication

| Variable | Default | Description |
|---|---|---|
| `GATHER_AUTH_MODE` | `env` | `env`: use `GATHER_API_TOKEN`. `keychain`: get-or-create a token in the OS keychain (the packaged desktop default). If the keychain is unavailable, startup fails rather than silently running open |
| `GATHER_API_TOKEN` | *(empty)* | Bearer token required on every `/api/v1` request and gRPC call. Empty in `env` mode means open on loopback, with a loud startup warning and `gather_api_auth_enabled 0` |

Print the keychain token with `gather-daemon print-api-token`.

## gRPC

| Variable | Default | Description |
|---|---|---|
| `GATHER_GRPC_ENABLED` | `true` | Serve the gRPC API |
| `GATHER_GRPC_BIND_ADDR` | `127.0.0.1:7602` | gRPC listen address (same loopback policy) |

## Extraction worker

Extracts PDF text, runs image OCR and produces atomic units.

| Variable | Default | Range | Description |
|---|---|---|---|
| `GATHER_EXTRACTION_ENABLED` | `true` | | Run the worker |
| `GATHER_EXTRACTION_INTERVAL_SECS` | `30` | ≥ 1 | Seconds to wait when there is nothing to read. While work is queued, passes run back to back (a 200 ms pause between them) until the queue is empty |
| `GATHER_EXTRACTION_BATCH` | `8` | 1–256 | Rows claimed per queue per pass |
| `GATHER_EXTRACTION_AI_DUTY_PERCENT` | `60` (low: `30`) | 10–100 | While a local AI model is in use, the share of time the extraction worker may spend working. After each model request it rests long enough to stay within this share (at 30, 3 s of work is followed by 7 s of rest, up to 2 minutes), so a long pass cannot exceed it. Without it a model reading a big backlog would keep the processor near 100 %. `100` never rests. Not used without Ollama. In the desktop app, **Settings → AI model → Reading speed** sets it: Gentle 30, Balanced 60, Full speed 100 |

Document sections are read a file at a time, oldest file first, so each file finishes before
the next starts. A section that fails to save (for example a database error on one unit) is set
aside with its error in `units_extract_error` and logged, and the rest of the queue carries on;
before, one such section failed every pass and held up everything behind it. While busy, the
worker logs how much is left once a minute (`extraction: still reading`), and
`GET /api/v1/status` reports the same counts.
| `GATHER_TESSERACT_PATH` | `tesseract` | | Tesseract binary (bundled in the Docker image; install locally for OCR outside Docker) |

## Local LLM (opt-in)

| Variable | Default | Description |
|---|---|---|
| `GATHER_OLLAMA_URL` | *(empty)* | Empty disables all LLM and embedding features. For example `http://127.0.0.1:11434`. Must be loopback unless `GATHER_ALLOW_NON_LOOPBACK=true` |
| `GATHER_OLLAMA_MODEL` | `llama3.2:3b` (low: `none`) | Chat model for LLM-assisted extraction and the contradiction judge. `none` keeps Ollama to embeddings (and captions, with a vision model); set a model, e.g. `llama3.2:1b`, to use one in the low profile |
| `GATHER_OLLAMA_EMBED_MODEL` | `nomic-embed-text` | Embedding model. It must produce 768-dimension vectors to match the schema |
| `GATHER_OLLAMA_VISION_MODEL` | *(empty)* | Vision model for photo captions and visual topics, e.g. `moondream` or `llava`. Empty disables captions. Needs `GATHER_OLLAMA_URL` |
| `GATHER_OLLAMA_KEEP_ALIVE` | Ollama's default (low: `1m`) | How long Ollama keeps a model in memory after Gather's last request, as an Ollama duration (`30s`, `5m`, `0`) |
| `GATHER_OLLAMA_NUM_CTX` | Ollama's default (low: `2048`) | Context window for chat and caption requests. It sizes the model's cache, so smaller uses less memory. **Validated**, ≥ 512 |
| `GATHER_OLLAMA_ONE_AT_A_TIME` | `false` (low: `true`) | Send Ollama one request at a time across all of Gather's workers, so two models are never busy at once. Search may then wait for a running extraction request |

In the desktop app, **Settings → AI model** sets these instead: the Ollama address, the model
for reading files and the model for search, with a connection test that lists the models Ollama
has. Saving first checks that the search model returns 768-dimension vectors, then writes `ai-settings.json` in the app-data folder and restarts the background service
with `GATHER_OLLAMA_URL`, `GATHER_OLLAMA_MODEL` and `GATHER_OLLAMA_EMBED_MODEL` set from it (an
empty URL when turned off; an address without a port gets Ollama's 11434). Until something is
saved there, the app's own environment applies.

The model's reply to each request is capped (768 tokens for reading a section, 200 for a contradiction verdict), so a small model that loops on JSON output can't run for minutes on one section; a reply cut short is treated as no items from the model, and the built-in rules' items are kept.

With Ollama enabled you get LLM-extracted units, embeddings for units, segments and entities,
semantic search, and embedding-based entity-merge and contradiction candidates. Without it,
everything still works using the deterministic text similarity and full-text search paths.

## Contradiction scanner

| Variable | Default | Range | Description |
|---|---|---|---|
| `GATHER_SCAN_ENABLED` | `true` | | Run the scanner |
| `GATHER_SCAN_INTERVAL_SECS` | `600` | ≥ 1 | Seconds between passes |
| `GATHER_SCAN_BATCH` | `32` | 1–512 | Unscanned units per pass |
| `GATHER_SCAN_THRESHOLD` | `0.65` | 0–1 | Minimum score to record a contradiction |
| `GATHER_SCAN_MAX_CANDIDATES` | `25` | 1–200 | Candidates per blocking strategy per unit |

## Auto-act admission (autonomous pipeline)

See [AUTONOMOUS-PIPELINE.md](AUTONOMOUS-PIPELINE.md).

| Variable | Default | Description |
|---|---|---|
| `GATHER_ADMIT_HOLD_BELOW` | `0.5` | Units at or above this confidence are admitted silently. Units below it (but at or above the drop floor) are still admitted and are also parked in the review tray. **Validated**, 0–1 |
| `GATHER_ADMIT_DROP_BELOW` | `0.0` | Units below this confidence are retracted on ingest. `0.0` means user data is never dropped. **Validated**, 0–1, must be ≤ `GATHER_ADMIT_HOLD_BELOW` |

## Clustering worker

| Variable | Default | Range | Description |
|---|---|---|---|
| `GATHER_CLUSTER_ENABLED` | `true` | | Run entity auto-resolution and topic grouping |
| `GATHER_CLUSTER_INTERVAL_SECS` | `900` | ≥ 1 | Seconds between passes |
| `GATHER_CLUSTER_BATCH` | `500` | 2–2000 | Unclustered units claimed per topic pass (also the size of the context window of already-clustered units) |
| `GATHER_CLUSTER_K` | `6` | 1–50 | *k* for the mutual-kNN graph |
| `GATHER_CLUSTER_THRESHOLD` | `0.5` | 0–1 | Minimum edge similarity. **Validated** |
| `GATHER_CLUSTER_MAX_COMPONENT` | `50` | 2–10000 | Components larger than this are too diffuse to auto-label or auto-merge. Such entity components are parked for review instead |

## Semantic safety

See [SEMANTIC-SAFETY.md](SEMANTIC-SAFETY.md).

| Variable | Default | Range | Description |
|---|---|---|---|
| `GATHER_SAFETY_HUB_DEGREE` | `3` | 2–1000 | An entity (or photo) with at least this many auto-strength matches that mostly don't match each other is a generic hub: it is never merged or grouped automatically and gets one review item instead |
| `GATHER_SAFETY_SUCCESSION_DAYS` | `30` | 1–3650 | Two present-tense statements of state asserted at least this many days apart describe successive states: the later one supersedes the earlier (kept as history) instead of contradicting it |
| `GATHER_SAFETY_SAME_MOMENT_HOURS` | `24` | 0–720 | Present-tense statements asserted within this window describe the same moment and may contradict. Between the two windows the time relation is unknown and a conflict is reported for review only |

## Active learning & auto-tuning

See [AUTONOMOUS-PIPELINE.md](AUTONOMOUS-PIPELINE.md#active-learning-and-auto-tuning).

| Variable | Default | Range | Description |
|---|---|---|---|
| `GATHER_TUNE_ENABLED` | `true` | | Let your feedback move the decision thresholds. The tune worker always runs to re-rank the review tray; this gates only threshold changes |
| `GATHER_TUNE_INTERVAL_SECS` | `600` | ≥ 1 | Seconds between re-rank / tuning passes |
| `GATHER_TUNE_MIN_SAMPLES` | `20` | 5–100000 | Labels needed at or above a threshold before it may move |
| `GATHER_TUNE_TARGET_PRECISION` | `0.90` | 0–1 | Precision the auto-accepted band must hold. **Validated** |

Tuned values live in the database and override `GATHER_ADMIT_HOLD_BELOW` and the single-signal
merge threshold. Inspect them with `GET /api/v1/tuning`; `POST /api/v1/tuning/reset` returns to
these env defaults.

## Photo pipeline

See [AUTONOMOUS-PIPELINE.md](AUTONOMOUS-PIPELINE.md#photos).

| Variable | Default | Range | Description |
|---|---|---|---|
| `GATHER_PHOTO_ENABLED` | `true` | | Run the photo worker (hashing, duplicate groups, albums, optional captions) |
| `GATHER_PHOTO_INTERVAL_SECS` | `600` | ≥ 1 | Seconds between passes |
| `GATHER_PHOTO_BATCH` | `32` | 1–1000 | Photos hashed (and captioned) per pass |
| `GATHER_PHOTO_DUP_MAX_DISTANCE` | `6` | 0–16 | Max differing perceptual-hash bits for two photos to count as near-duplicates |
| `GATHER_PHOTO_ALBUM_GAP_HOURS` | `3` | 1–720 | A longer gap between shots starts a new album |
| `GATHER_PHOTO_ALBUM_SPLIT_KM` | `10` | 0.1–20000 | Consecutive located shots further apart than this start a new album |
| `GATHER_PHOTO_ALBUM_MIN_SIZE` | `2` | 1–1000 | Smallest group of photos that becomes an album |
| `GATHER_PHOTO_TOPIC_THRESHOLD` | `0.8` | 0–1 | Caption-embedding similarity for two photos to share a visual topic. **Validated** |

## Metrics worth watching

Exposed at `GET /metrics` (Prometheus format) and graphed in the provisioned Grafana dashboard.

| Metric | Meaning |
|---|---|
| `gather_api_auth_enabled` | 1 if the API requires a token, 0 if open |
| `gather_extraction_backlog{modality}` | Documents and images waiting for extraction |
| `gather_extraction_units_total{method,status}` | Units produced / deduplicated |
| `gather_contradictions_open` | Contradictions awaiting review |
| `gather_review_queue_open` | Items parked in the optional review tray |
| `gather_realdata_precision` | Of the units you gave a verdict on, the fraction whose latest verdict is "keep" |
| `gather_decision_threshold{key}` | Thresholds in force (tuned or default) |
| `gather_tuning_changes_total{key,direction}` | Threshold moves made by the auto-tuner |
| `gather_entity_merges_total` / `gather_entity_unmerges_total` | Entity merges made, and merges undone |
| `gather_http_request_duration_seconds` | Per-route latency |
| `gather_graph_query_duration_seconds` | Graph traversal latency |
