# Gather — Technical Write-Up

**A local-first AI data-sovereignty daemon**: Rust (Axum) + PostgreSQL/pgvector + Tauri v2/React.
Everything runs on your machine. Nothing leaves it unless you explicitly opt in.

This document is the engineering specification for the system in this repository. The runnable
skeleton (daemon, schema, Docker, CI, observability, desktop shell) is committed alongside it;
the extraction worker (§5, `daemon/src/extract/`) and the contradiction scanner (§6,
`daemon/src/scan/`) are implemented; the pseudocode in those sections is the behavioral spec
the implementations follow. Remaining *[pipeline — Phase 1]* markers denote MVP build-out items.

---

## 1. System architecture

### 1.1 Component diagram (textual)

```
┌──────────────────────────────── LOCAL MACHINE (trust boundary: OS user session) ─────────────────────────────────┐
│                                                                                                                   │
│  ┌───────────────────┐   loopback REST :7601 / gRPC :7602       ┌────────────────────────────────────────────┐   │
│  │  Tauri v2 UI      │ ───────────────────────────────────────▶ │  gather-daemon (Rust / Axum)               │   │
│  │  (React/TS)       │   Bearer token from OS keychain          │                                            │   │
│  │  • drag-and-drop  │                                          │  ┌──────────┐  ┌────────────┐  ┌────────┐  │   │
│  │  • file picker    │                                          │  │ Ingestion│─▶│ Extraction │─▶│ Graph  │  │   │
│  │  • review dash    │                                          │  │ +adapters│  │  workers   │  │ builder│  │   │
│  └───────────────────┘                                          │  └──────────┘  └────────────┘  └───┬────┘  │   │
│                                                                 │        │              │            │       │   │
│  ┌───────────────────┐   same loopback API                      │        ▼              ▼            ▼       │   │
│  │ downstream agents │ ───────────────────────────────────────▶ │  ┌──────────────────────────────────────┐  │   │
│  │ & tools (REST/gRPC)│                                         │  │        Contradiction scanner         │  │   │
│  └───────────────────┘                                          │  └──────────────────────────────────────┘  │   │
│                                                                 └───────────────┬────────────────────────────┘   │
│  ┌───────────────────┐  localhost only, opt-in                                  │ SQL (scram-sha-256,           │
│  │ Ollama (optional) │ ◀── extraction workers call 127.0.0.1:11434 ──┐          │  localhost / unix socket)     │
│  │ Tesseract (local  │ ◀── OCR invoked as local subprocess ──────────┤          ▼                               │
│  │ subprocess)       │                                               │  ┌───────────────────────────┐          │
│  └───────────────────┘                                               └──│ PostgreSQL 16 + pgvector  │          │
│                                                                        │ (single database "gather") │          │
│                                                                        └───────────────────────────┘          │
└───────────────────────────────────────────────┬───────────────────────────────────────────────────────────────┘
                                                │  OPT-IN ONLY: restic (AES-256, client-side) over SSH
                                                ▼  (disabled by default; no code path runs it automatically)
                              ┌─────────────────────────────────────┐
                              │  Hetzner CX22 backup VM (optional)  │
                              │  LUKS volume · never sees plaintext │
                              └─────────────────────────────────────┘
```

### 1.2 Data flow

1. **Ingestion** — three entry points, one persistence path:
   - *Chat exports* (`POST /api/v1/ingest/chat-export`): platform adapters normalize ChatGPT /
     Claude / any-platform exports.
   - *Agent logs* (`POST /api/v1/ingest/agent-log`): JSONL session logs (Claude Code, Goose,
     Aider, generic).
   - *Manual uploads* (`POST /api/v1/ingest/files`, multipart): PDFs, markdown, text, photos,
     screenshots — first-class, from the Tauri UI's drag-and-drop zone or native file picker.

   Every payload is sha-256 content-addressed into `artifacts` (identical bytes stored once),
   then normalized into modality tables: `conversations`/`messages`, `documents`/
   `document_segments`, `images`.

2. **Normalization** — adapters emit one canonical shape regardless of platform (§2), so
   everything downstream is source-agnostic.

3. **Extraction** *(shipped: `daemon/src/extract/`)* — a background worker turns normalized
   content into `atomic_units`
   (facts/claims/decisions/preferences/events) with per-unit `atomic_unit_provenance` rows
   pointing at the exact message, document segment, or image they came from (§5).

4. **Knowledge graph** — extraction also emits `entities` and typed, temporal `relationships`;
   traversal is a recursive CTE (`entity_neighborhood()` SQL function, shipped in the migration)
   behind `GET /api/v1/entities/{id}/graph`.

5. **Contradiction scan** *[pipeline — Phase 1]* — a periodic scanner pairs semantically-close
   active units (pgvector) sharing an entity and scores them for conflict (§6). Conflicts land in
   `contradictions` with full dual-sided provenance.

6. **Review & propagation** — the dashboard lists open contradictions; resolving one is
   transactional: audit row, status change, losing unit superseded, dependent relationships
   deactivated. Implemented and tested in the committed daemon.

7. **API/UI** — REST (`127.0.0.1:7601`) and gRPC (`127.0.0.1:7602`, contract in
   `proto/gather/v1/gather.proto`, served by tonic from the same process) expose query, search,
   export/import, and the review workflow to the UI and downstream agents.

### 1.3 Trust boundaries

| Boundary | Mechanism |
|---|---|
| UI / agents ↔ daemon | Loopback-only bind (non-loopback refused at startup unless `GATHER_ALLOW_NON_LOOPBACK=true`); optional bearer token held in the OS keychain; CORS restricted to `tauri://localhost` + Vite dev origin |
| Daemon ↔ Postgres | localhost/unix socket only; scram-sha-256 auth; credentials via env injected from keychain |
| Daemon ↔ Ollama/Tesseract (optional) | localhost HTTP / local subprocess; disabled unless configured |
| Local machine ↔ backup VPS (optional) | Client-side restic AES-256 encryption **before** transit; SSH transport; VM stores ciphertext on a LUKS volume (§7.5) |

Manual document/photo upload is not a bolt-on: uploads become `artifacts` exactly like chat
exports, flow through the same extraction pipeline, carry the same provenance links, and are
eligible for the same contradiction scans. A fact extracted from a screenshot's OCR text can
contradict a fact from a ChatGPT conversation, and the review UI shows both sources side by side.

---

## 2. Cross-platform AI ingestion

Ingestion is platform-agnostic by construction: a `SourceAdapter` boundary
(`daemon/src/adapters/`) maps any export format into `NormalizedConversation` /
`NormalizedMessage`, and only the ingest route touches the database. Adding a platform is one
adapter file plus a dispatch line.

| Platform | `source_platform` | Input format | Adapter status |
|---|---|---|---|
| ChatGPT / OpenAI | `chatgpt` | `conversations.json` from account data export; message **tree** in `mapping`, linearized via `current_node` parent-chain (regenerated branches preserved through `parent_message_id`) | **Shipped** (`adapters/chatgpt.rs`) |
| Claude.ai | `claude` | `conversations.json` from data export; `chat_messages[]` with `sender`, `text`/`content[]` blocks, RFC 3339 timestamps | **Shipped** (`adapters/claude.rs`) |
| Any platform / manual | `generic` | `gather-generic-v1` JSON (below) | **Shipped** (`adapters/generic.rs`) |
| Claude Code / Goose / Aider | `claude_code`, `goose`, `aider` | JSONL session logs, one `{role, content, timestamp}` object per line (string, `{text}`, or content-block arrays all accepted) | **Shipped** via the agent-log route, which converts JSONL to `gather-generic-v1` |
| Google Gemini | `gemini` | Takeout `MyActivity.json`: prompt from `title` ("Prompted …"), response from tag-stripped `safeHtmlItem.htmlValue` (often absent in Takeout — prompt-only records still ingest) | **Shipped** (`adapters/gemini.rs`, `google-takeout-myactivity-v1`) |
| Grok / xAI | `grok` | Account export JSON: `conversations[].responses[]` with `sender` ∈ {human, assistant}, epoch-ms `create_time` (number or numeric string); bare top-level arrays accepted | **Shipped** (`adapters/grok.rs`, `xai-export-v1`) |
| Perplexity | `perplexity` | Thread export JSON: `threads[].entries[]` with `query`/`answer` pairs and RFC 3339 `timestamp`; single-thread top-level `entries` accepted | **Shipped** (`adapters/perplexity.rs`, `perplexity-thread-export-v1`) |
| Copilot / VS Code chat | `copilot` | `chat.json` session files from VS Code workspace storage: `requests[]` with `message.text` and concatenated `response[].value` | **Shipped** (`adapters/copilot.rs`, `vscode-chat-session-v1`) |

Every artifact records `source_platform` + `source_format_version` (e.g.
`openai-conversations-json-v1`), so provenance, search, and contradiction detection treat all
platforms — and manual uploads — identically.

**`gather-generic-v1`** (the universal fallback; any platform can be ingested today by converting
to this):

```json
{
  "schema": "gather-generic-v1",
  "conversations": [{
    "id": "optional-external-id",
    "title": "optional",
    "model": "optional",
    "started_at": "RFC3339 optional",
    "ended_at": "RFC3339 optional",
    "messages": [{
      "role": "user | assistant | system | tool",
      "author": "optional display name",
      "model": "optional per-message model",
      "content": "required text",
      "created_at": "RFC3339 optional"
    }]
  }]
}
```

---

## 3. PostgreSQL schema (complete DDL)

The DDL below is the verbatim content of `daemon/migrations/0001_init.sql` (applied automatically
by the daemon at startup via embedded sqlx migrations). Design highlights:

- **Dedup**: `artifacts.content_hash` (sha-256, unique) — identical bytes ingest as a no-op that
  returns the existing artifact id.
- **Versioning**: `supersedes_artifact_id` chains re-uploads/newer exports; a trigger maintains
  monotonically increasing `version`.
- **Multimodal**: one `artifacts` table with a `kind` enum spanning chat, agent logs, three
  document kinds and two image kinds; modality detail lives in `conversations`/`messages`,
  `documents`/`document_segments`, `images`.
- **Provenance**: `atomic_unit_provenance` always carries `artifact_id` plus at most one
  fine-grained anchor (`message_id` | `document_segment_id` | `image_id`) with optional char
  offsets and a verbatim quote — so "why do you believe X?" is one indexed join away from any
  modality.
- **Vectors**: `vector(768)` (nomic-embed-text via local Ollama) on `atomic_units`,
  `document_segments`, `entities`, each with an HNSW cosine index; generated `tsvector` columns +
  GIN indexes give full-text search with zero extra infrastructure.
- **Graph**: `relationships` is indexed both directions (`(source_entity_id, relation_type,
  status)` and target-side) and traversed by the cycle-safe `entity_neighborhood()` recursive
  CTE function. Measured on the running stack: depth-2 traversal ≈ 5 ms (budget: <150 ms).
- **Import-friendliness**: self-referential FKs are `DEFERRABLE` so the portable bundle (§4.4)
  restores in one transaction regardless of row order.

```sql
-- Gather — initial schema
-- Single source of truth for the knowledge store. Mirrored verbatim in
-- docs/TECHNICAL-WRITEUP.md §3. Requires PostgreSQL 16+ with pgvector.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---------------------------------------------------------------------------
-- Enumerated types
-- ---------------------------------------------------------------------------

CREATE TYPE artifact_kind AS ENUM (
    'chat_export',
    'agent_log',
    'document_pdf',
    'document_markdown',
    'document_text',
    'image_photo',
    'image_screenshot'
);

CREATE TYPE ingestion_status AS ENUM (
    'pending', 'processing', 'completed', 'partial', 'failed'
);

CREATE TYPE extraction_status AS ENUM (
    'pending', 'processing', 'completed', 'failed', 'skipped'
);

CREATE TYPE unit_kind AS ENUM (
    'fact', 'claim', 'decision', 'preference', 'event'
);

CREATE TYPE unit_status AS ENUM (
    'active', 'superseded', 'retracted', 'disputed'
);

CREATE TYPE extraction_method AS ENUM (
    'rule_based', 'llm_local', 'manual'
);

CREATE TYPE entity_kind AS ENUM (
    'person', 'organization', 'project', 'tool',
    'concept', 'location', 'event', 'other'
);

CREATE TYPE contradiction_status AS ENUM (
    'open',        -- detected, awaiting review
    'resolved_a',  -- unit A kept, unit B superseded/retracted
    'resolved_b',  -- unit B kept, unit A superseded/retracted
    'both_valid',  -- reviewed: not actually contradictory (e.g. temporal change)
    'dismissed'    -- false positive
);

-- ---------------------------------------------------------------------------
-- updated_at touch trigger (shared)
-- ---------------------------------------------------------------------------

CREATE FUNCTION touch_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END $$;

-- ---------------------------------------------------------------------------
-- ingestion_jobs — one row per ingestion request (upload batch or export file)
-- ---------------------------------------------------------------------------

CREATE TABLE ingestion_jobs (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    source       text NOT NULL,                       -- 'rest', 'grpc', 'ui'
    status       ingestion_status NOT NULL DEFAULT 'pending',
    started_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz,
    stats        jsonb NOT NULL DEFAULT '{}'::jsonb,  -- {files: n, ok: n, failed: n, ...}
    error        text
);

CREATE INDEX ingestion_jobs_status_idx ON ingestion_jobs (status, started_at DESC);

-- ---------------------------------------------------------------------------
-- artifacts — every ingested thing, of any modality. Content-addressed dedup
-- via sha-256; explicit versioning via supersedes_artifact_id.
-- ---------------------------------------------------------------------------

CREATE TABLE artifacts (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind                   artifact_kind NOT NULL,
    source_platform        text NOT NULL DEFAULT 'manual',
        -- 'chatgpt','claude','gemini','grok','perplexity','copilot',
        -- 'claude_code','goose','aider','generic','manual'
    source_format_version  text,                      -- adapter format tag, e.g. 'openai-export-2025-12'
    original_filename      text,
    media_type             text,                      -- MIME type as received
    byte_size              bigint NOT NULL CHECK (byte_size >= 0),
    content_hash           char(64) NOT NULL,         -- hex sha-256 of raw bytes
    raw_content            bytea,                     -- inline storage (personal scale)
    storage_path           text,                      -- alternative on-disk path for large blobs
    version                integer NOT NULL DEFAULT 1 CHECK (version >= 1),
    -- Self-referential FKs are DEFERRABLE so bundle import (see §5 API) can
    -- restore rows in any order within a transaction.
    supersedes_artifact_id uuid REFERENCES artifacts (id) ON DELETE SET NULL
                           DEFERRABLE INITIALLY IMMEDIATE,
    source_created_at      timestamptz,               -- timestamp claimed by the source itself
    ingested_at            timestamptz NOT NULL DEFAULT now(),
    ingestion_job_id       uuid REFERENCES ingestion_jobs (id) ON DELETE SET NULL,
    metadata               jsonb NOT NULL DEFAULT '{}'::jsonb,
    CONSTRAINT artifacts_content_present
        CHECK (raw_content IS NOT NULL OR storage_path IS NOT NULL)
);

-- Dedup: identical bytes are stored exactly once.
CREATE UNIQUE INDEX artifacts_content_hash_uq ON artifacts (content_hash);
CREATE INDEX artifacts_kind_idx        ON artifacts (kind, ingested_at DESC);
CREATE INDEX artifacts_platform_idx    ON artifacts (source_platform, ingested_at DESC);
CREATE INDEX artifacts_supersedes_idx  ON artifacts (supersedes_artifact_id)
    WHERE supersedes_artifact_id IS NOT NULL;
CREATE INDEX artifacts_metadata_gin    ON artifacts USING gin (metadata jsonb_path_ops);

-- Versioning guard: an artifact that supersedes another carries the next
-- version number in the chain. When the predecessor row is not visible yet
-- (bundle import with deferred FKs), the provided version is kept and the
-- deferred FK still guarantees the predecessor exists by commit.
CREATE FUNCTION artifacts_versioning() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    prev_version integer;
BEGIN
    IF NEW.supersedes_artifact_id IS NOT NULL THEN
        SELECT version INTO prev_version FROM artifacts WHERE id = NEW.supersedes_artifact_id;
        IF prev_version IS NOT NULL THEN
            NEW.version = prev_version + 1;
        END IF;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER artifacts_versioning_trg
    BEFORE INSERT ON artifacts
    FOR EACH ROW EXECUTE FUNCTION artifacts_versioning();

-- ---------------------------------------------------------------------------
-- conversations / messages — normalized chat + agent-session structure.
-- Every chat/agent adapter, regardless of platform, writes into these tables.
-- ---------------------------------------------------------------------------

CREATE TABLE conversations (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    artifact_id     uuid NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    external_id     text,                              -- platform conversation id
    title           text,
    source_platform text NOT NULL,
    model           text,                              -- primary model, if known
    started_at      timestamptz,
    ended_at        timestamptz,
    metadata        jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE UNIQUE INDEX conversations_artifact_external_uq
    ON conversations (artifact_id, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX conversations_artifact_idx ON conversations (artifact_id);
CREATE INDEX conversations_started_idx  ON conversations (started_at DESC NULLS LAST);

CREATE TABLE messages (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    conversation_id   uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    external_id       text,
    parent_message_id uuid REFERENCES messages (id) ON DELETE SET NULL
                      DEFERRABLE INITIALLY IMMEDIATE, -- tree exports (ChatGPT)
    seq               integer NOT NULL,               -- linearized order within conversation
    role              text NOT NULL
        CHECK (role IN ('system','user','assistant','tool','function','other')),
    author            text,                            -- display name / agent name
    model             text,                            -- per-message model override
    content           text NOT NULL,
    content_tsv       tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    created_at        timestamptz,                     -- timestamp from the source
    metadata          jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX messages_conversation_seq_idx ON messages (conversation_id, seq);
CREATE INDEX messages_tsv_gin              ON messages USING gin (content_tsv);
CREATE INDEX messages_created_idx          ON messages (created_at DESC NULLS LAST);

-- ---------------------------------------------------------------------------
-- documents / document_segments — uploaded PDFs, markdown, plain text.
-- ---------------------------------------------------------------------------

CREATE TABLE documents (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    artifact_id       uuid NOT NULL UNIQUE REFERENCES artifacts (id) ON DELETE CASCADE,
    page_count        integer,
    language          text,
    extracted_text    text,
    extraction_tool   text,                            -- 'utf8-passthrough','pdfium','tesseract-fallback'
    extraction_status extraction_status NOT NULL DEFAULT 'pending',
    extracted_at      timestamptz,
    metadata          jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX documents_status_idx ON documents (extraction_status);

CREATE TABLE document_segments (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    document_id  uuid NOT NULL REFERENCES documents (id) ON DELETE CASCADE,
    seq          integer NOT NULL,
    page         integer,
    heading      text,
    content      text NOT NULL,
    content_hash char(64) NOT NULL,                   -- sha-256 of normalized content
    content_tsv  tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    embedding    vector(768),                          -- nomic-embed-text (Ollama), optional
    metadata     jsonb NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (document_id, seq)
);

CREATE INDEX document_segments_tsv_gin ON document_segments USING gin (content_tsv);
CREATE INDEX document_segments_embedding_hnsw
    ON document_segments USING hnsw (embedding vector_cosine_ops);

-- ---------------------------------------------------------------------------
-- images — uploaded photos / screenshots with EXIF + OCR results.
-- ---------------------------------------------------------------------------

CREATE TABLE images (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    artifact_id    uuid NOT NULL UNIQUE REFERENCES artifacts (id) ON DELETE CASCADE,
    width          integer,
    height         integer,
    exif           jsonb NOT NULL DEFAULT '{}'::jsonb,
    taken_at       timestamptz,                        -- EXIF DateTimeOriginal when present
    ocr_text       text,
    ocr_confidence real CHECK (ocr_confidence IS NULL OR (ocr_confidence >= 0 AND ocr_confidence <= 1)),
    ocr_status     extraction_status NOT NULL DEFAULT 'pending',
    ocr_tsv        tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(ocr_text, ''))) STORED,
    caption        text,
    caption_model  text,
    metadata       jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX images_ocr_status_idx ON images (ocr_status);
CREATE INDEX images_ocr_tsv_gin    ON images USING gin (ocr_tsv);
CREATE INDEX images_taken_idx      ON images (taken_at DESC NULLS LAST);

-- ---------------------------------------------------------------------------
-- entities / entity_aliases — knowledge-graph nodes.
-- ---------------------------------------------------------------------------

CREATE TABLE entities (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name                  text NOT NULL,
    kind                  entity_kind NOT NULL DEFAULT 'other',
    description           text,
    merged_into_entity_id uuid REFERENCES entities (id) ON DELETE SET NULL
                          DEFERRABLE INITIALLY IMMEDIATE,
    embedding             vector(768),
    metadata              jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX entities_name_kind_uq ON entities (lower(name), kind)
    WHERE merged_into_entity_id IS NULL;
CREATE INDEX entities_embedding_hnsw ON entities USING hnsw (embedding vector_cosine_ops);

CREATE TRIGGER entities_touch_trg
    BEFORE UPDATE ON entities
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE TABLE entity_aliases (
    id        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_id uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    alias     text NOT NULL
);

CREATE UNIQUE INDEX entity_aliases_uq  ON entity_aliases (entity_id, lower(alias));
CREATE INDEX entity_aliases_alias_idx  ON entity_aliases (lower(alias));

-- ---------------------------------------------------------------------------
-- atomic_units — timestamped facts / claims / decisions / preferences.
-- Deduplicated on normalized statement hash; multi-source provenance lives in
-- atomic_unit_provenance. Temporal validity via valid_from / valid_to.
-- ---------------------------------------------------------------------------

CREATE TABLE atomic_units (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind                  unit_kind NOT NULL,
    statement             text NOT NULL,
    statement_hash        char(64) NOT NULL,          -- sha-256 of normalized statement
    subject_entity_id     uuid REFERENCES entities (id) ON DELETE SET NULL,
    confidence            real NOT NULL DEFAULT 0.5
        CHECK (confidence >= 0 AND confidence <= 1),
    extraction_method     extraction_method NOT NULL,
    extraction_model      text,                        -- e.g. 'ollama:llama3.1:8b'
    embedding             vector(768),
    statement_tsv         tsvector GENERATED ALWAYS AS (to_tsvector('english', statement)) STORED,
    valid_from            timestamptz,                 -- when the statement became true/held
    valid_to              timestamptz,                 -- when it stopped (NULL = still valid)
    status                unit_status NOT NULL DEFAULT 'active',
    superseded_by_unit_id uuid REFERENCES atomic_units (id) ON DELETE SET NULL
                          DEFERRABLE INITIALLY IMMEDIATE,
    attrs                 jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT atomic_units_valid_range CHECK (valid_to IS NULL OR valid_from IS NULL OR valid_to >= valid_from)
);

CREATE UNIQUE INDEX atomic_units_statement_hash_uq ON atomic_units (statement_hash);
CREATE INDEX atomic_units_status_idx   ON atomic_units (status, kind);
CREATE INDEX atomic_units_subject_idx  ON atomic_units (subject_entity_id)
    WHERE subject_entity_id IS NOT NULL;
CREATE INDEX atomic_units_tsv_gin      ON atomic_units USING gin (statement_tsv);
CREATE INDEX atomic_units_embedding_hnsw
    ON atomic_units USING hnsw (embedding vector_cosine_ops);
CREATE INDEX atomic_units_valid_idx    ON atomic_units (valid_from, valid_to);

CREATE TRIGGER atomic_units_touch_trg
    BEFORE UPDATE ON atomic_units
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- ---------------------------------------------------------------------------
-- atomic_unit_provenance — links a unit to the exact place it came from.
-- Exactly one anchor granularity may be set; artifact_id is always set so a
-- provenance query never needs to branch on modality.
-- ---------------------------------------------------------------------------

CREATE TABLE atomic_unit_provenance (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    atomic_unit_id      uuid NOT NULL REFERENCES atomic_units (id) ON DELETE CASCADE,
    artifact_id         uuid NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    message_id          uuid REFERENCES messages (id) ON DELETE CASCADE,
    document_segment_id uuid REFERENCES document_segments (id) ON DELETE CASCADE,
    image_id            uuid REFERENCES images (id) ON DELETE CASCADE,
    char_start          integer CHECK (char_start IS NULL OR char_start >= 0),
    char_end            integer CHECK (char_end IS NULL OR char_end >= char_start),
    quote               text,                          -- verbatim supporting span
    created_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT provenance_single_anchor CHECK (
        (message_id IS NOT NULL)::int
        + (document_segment_id IS NOT NULL)::int
        + (image_id IS NOT NULL)::int <= 1
    )
);

CREATE INDEX provenance_unit_idx     ON atomic_unit_provenance (atomic_unit_id);
CREATE INDEX provenance_artifact_idx ON atomic_unit_provenance (artifact_id);
CREATE INDEX provenance_message_idx  ON atomic_unit_provenance (message_id)
    WHERE message_id IS NOT NULL;
CREATE INDEX provenance_segment_idx  ON atomic_unit_provenance (document_segment_id)
    WHERE document_segment_id IS NOT NULL;
CREATE INDEX provenance_image_idx    ON atomic_unit_provenance (image_id)
    WHERE image_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- relationships — typed, temporal edges between entities, each optionally
-- backed by the atomic unit that asserts it.
-- ---------------------------------------------------------------------------

CREATE TABLE relationships (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    source_entity_id uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    target_entity_id uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    relation_type    text NOT NULL,                    -- 'works_at','uses','decided_on',...
    atomic_unit_id   uuid REFERENCES atomic_units (id) ON DELETE SET NULL,
    confidence       real NOT NULL DEFAULT 0.5
        CHECK (confidence >= 0 AND confidence <= 1),
    valid_from       timestamptz,
    valid_to         timestamptz,
    status           unit_status NOT NULL DEFAULT 'active',
    metadata         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT relationships_no_self_loop CHECK (source_entity_id <> target_entity_id),
    CONSTRAINT relationships_valid_range  CHECK (valid_to IS NULL OR valid_from IS NULL OR valid_to >= valid_from)
);

-- Graph traversal in both directions:
CREATE INDEX relationships_source_idx ON relationships (source_entity_id, relation_type, status);
CREATE INDEX relationships_target_idx ON relationships (target_entity_id, relation_type, status);
CREATE INDEX relationships_unit_idx   ON relationships (atomic_unit_id)
    WHERE atomic_unit_id IS NOT NULL;
CREATE UNIQUE INDEX relationships_edge_uq
    ON relationships (source_entity_id, target_entity_id, relation_type,
                      coalesce(atomic_unit_id, '00000000-0000-0000-0000-000000000000'::uuid));

CREATE TRIGGER relationships_touch_trg
    BEFORE UPDATE ON relationships
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- ---------------------------------------------------------------------------
-- contradictions + audit trail
-- ---------------------------------------------------------------------------

CREATE TABLE contradictions (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    unit_a_id        uuid NOT NULL REFERENCES atomic_units (id) ON DELETE CASCADE,
    unit_b_id        uuid NOT NULL REFERENCES atomic_units (id) ON DELETE CASCADE,
    score            real NOT NULL CHECK (score >= 0 AND score <= 1),
    detection_method text NOT NULL,                    -- 'rule:negation','llm:ollama-judge',...
    explanation      text,                             -- human-readable why
    status           contradiction_status NOT NULL DEFAULT 'open',
    detected_at      timestamptz NOT NULL DEFAULT now(),
    resolved_at      timestamptz,
    resolved_by      text,                             -- OS username of reviewer
    resolution_note  text,
    CONSTRAINT contradictions_ordered_pair CHECK (unit_a_id < unit_b_id)
);

CREATE UNIQUE INDEX contradictions_pair_uq  ON contradictions (unit_a_id, unit_b_id);
CREATE INDEX contradictions_status_idx      ON contradictions (status, detected_at DESC);
CREATE INDEX contradictions_unit_a_idx      ON contradictions (unit_a_id);
CREATE INDEX contradictions_unit_b_idx      ON contradictions (unit_b_id);

CREATE TABLE contradiction_audit (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    contradiction_id uuid NOT NULL REFERENCES contradictions (id) ON DELETE CASCADE,
    action           text NOT NULL,                    -- 'detected','resolve','annotate','reopen','dismiss'
    actor            text NOT NULL DEFAULT 'local-user',
    from_status      contradiction_status,
    to_status        contradiction_status,
    note             text,
    created_at       timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX contradiction_audit_cid_idx ON contradiction_audit (contradiction_id, created_at);

-- ---------------------------------------------------------------------------
-- Graph traversal helper: bounded-depth, cycle-safe neighborhood walk.
-- Backs GET /api/v1/entities/{id}/graph and the gRPC QueryService.
-- ---------------------------------------------------------------------------

CREATE FUNCTION entity_neighborhood(root uuid, max_depth integer DEFAULT 2)
RETURNS TABLE (
    depth            integer,
    relationship_id  uuid,
    source_entity_id uuid,
    target_entity_id uuid,
    relation_type    text,
    confidence       real
)
LANGUAGE sql STABLE AS $$
    WITH RECURSIVE walk AS (
        SELECT r.id AS relationship_id,
               r.source_entity_id,
               r.target_entity_id,
               r.relation_type,
               r.confidence,
               1 AS depth,
               ARRAY[root,
                     CASE WHEN r.source_entity_id = root
                          THEN r.target_entity_id
                          ELSE r.source_entity_id END] AS visited
        FROM relationships r
        WHERE r.status = 'active'
          AND (r.source_entity_id = root OR r.target_entity_id = root)

        UNION ALL

        SELECT r.id,
               r.source_entity_id,
               r.target_entity_id,
               r.relation_type,
               r.confidence,
               w.depth + 1,
               w.visited ||
                   CASE WHEN r.source_entity_id = w.visited[array_upper(w.visited, 1)]
                        THEN r.target_entity_id
                        ELSE r.source_entity_id END
        FROM relationships r
        JOIN walk w
          ON (r.source_entity_id = w.visited[array_upper(w.visited, 1)]
              OR r.target_entity_id = w.visited[array_upper(w.visited, 1)])
        WHERE r.status = 'active'
          AND w.depth < max_depth
          AND NOT (
              CASE WHEN r.source_entity_id = w.visited[array_upper(w.visited, 1)]
                   THEN r.target_entity_id
                   ELSE r.source_entity_id END = ANY (w.visited)
          )
    )
    SELECT DISTINCT ON (relationship_id)
           depth, relationship_id, source_entity_id, target_entity_id,
           relation_type, confidence
    FROM walk
    ORDER BY relationship_id, depth;
$$;
```

Example traversal and provenance queries the indexes are designed for:

```sql
-- Neighborhood of an entity, depth 3 (backs GET /entities/{id}/graph)
SELECT * FROM entity_neighborhood('11111111-...', 3);

-- Full provenance for a unit, across any modality, one join each
SELECT a.kind, a.source_platform, a.original_filename, p.quote,
       m.content AS message, s.content AS segment, i.ocr_text
FROM atomic_unit_provenance p
JOIN artifacts a            ON a.id = p.artifact_id
LEFT JOIN messages m         ON m.id = p.message_id
LEFT JOIN document_segments s ON s.id = p.document_segment_id
LEFT JOIN images i           ON i.id = p.image_id
WHERE p.atomic_unit_id = $1;

-- Contradiction candidates: semantically close active units sharing an entity
SELECT u2.id, 1 - (u1.embedding <=> u2.embedding) AS similarity
FROM atomic_units u1
JOIN atomic_units u2
  ON u2.subject_entity_id = u1.subject_entity_id
 AND u2.id > u1.id AND u2.status = 'active' AND u2.embedding IS NOT NULL
WHERE u1.id = $1 AND u1.status = 'active' AND u1.embedding IS NOT NULL
ORDER BY u1.embedding <=> u2.embedding
LIMIT 25;
```

---

## 4. API contract

All REST endpoints are implemented in the committed daemon and live under
`http://127.0.0.1:7601`. Errors use one envelope:
`{"error": {"code": "bad_request|unauthorized|not_found|unsupported_media_type|database_error|internal_error", "message": "…"}}`.
When `GATHER_API_TOKEN` is set, every `/api/v1/*` request requires
`Authorization: Bearer <token>`; `/healthz`, `/readyz`, `/metrics` stay open (loopback only).

The gRPC contract in [`proto/gather/v1/gather.proto`](../proto/gather/v1/gather.proto) mirrors
this surface 1:1 (`IngestService`, `QueryService`, `ContradictionService`, `ExportService`);
every protobuf field is annotated with the `table.column` it maps to. It is **served** (tonic,
same process, `daemon/src/grpc/`) on `127.0.0.1:7602` (`GATHER_GRPC_BIND_ADDR`, same
loopback-unless-overridden policy as the HTTP bind; disable with `GATHER_GRPC_ENABLED=false`).
Nontrivial logic — ingestion persistence, contradiction resolution, bundle build/import,
search — is shared code with REST, so the two surfaces cannot drift. The same bearer token is
enforced via `authorization: Bearer <token>` metadata (constant-time compare). File upload and
bundle export/import stream in 64 KiB chunks (`IngestFile`, `ExportBundle`/`ImportBundle`).

### 4.1 Health & metrics

| Method & path | Purpose | Response |
|---|---|---|
| `GET /healthz` | liveness | `{"status":"ok","service":"gather-daemon","version":"0.1.0"}` |
| `GET /readyz` | DB reachable + pgvector installed | `200 {"status":"ready"}` or `503 {"status":"unavailable","reason":…}` |
| `GET /metrics` | Prometheus exposition | text format |

### 4.2 Ingestion

**`POST /api/v1/ingest/chat-export`** → `202 Accepted`
Tables: `ingestion_jobs`, `artifacts` (kind `chat_export`), `conversations`, `messages`.

```jsonc
// request
{ "platform": "chatgpt | claude | generic",  // -> artifacts.source_platform
  "data": { /* raw export JSON */ },          // stored verbatim as artifacts.raw_content
  "filename": "conversations.json" }          // optional -> artifacts.original_filename
// response
{ "job_id": "uuid", "artifact_id": "uuid", "deduplicated": false,
  "conversations": 12, "messages": 340 }
```

**`POST /api/v1/ingest/agent-log`** → `202 Accepted`
Same tables, kind `agent_log`. JSONL lines accept `role|type|sender` and
`content` as string / `{text}` / content-block array (covers Claude Code, Goose, Aider).

```jsonc
{ "platform": "claude_code | goose | aider | generic",
  "jsonl": "{\"role\":\"user\",\"content\":\"…\"}\n…",
  "session_id": "optional", "title": "optional" }
```

**`POST /api/v1/ingest/files`** (multipart/form-data) → `202 Accepted`
Tables: `ingestion_jobs`, `artifacts`, `documents`, `document_segments`, `images`.
Each file is a part named `file` (kind auto-detected from MIME + extension) or named with an
explicit kind (`document_pdf`, `document_markdown`, `document_text`, `image_photo`,
`image_screenshot`) to override detection. Markdown/text are extracted + segmented synchronously;
PDFs/images get `documents`/`images` rows in `pending` state for the extraction workers.
Per-file failures don't fail the batch:

```jsonc
{ "job_id": "uuid",
  "files": [{ "filename": "notes.md", "kind": "document_markdown",
              "artifact_id": "uuid", "deduplicated": false,
              "status": "accepted | deduplicated | rejected",
              "detail": null, "segments": 7 }] }
```

### 4.3 Query & search

| Method & path | Backing tables | Notes |
|---|---|---|
| `GET /api/v1/artifacts?kind=&source_platform=&limit=&offset=` | `artifacts` | newest first |
| `GET /api/v1/artifacts/{id}` | `artifacts` + `conversations` + `documents(+segment count)` + `images` | one call answers "what is this artifact" |
| `GET /api/v1/atomic-units?kind=&status=&subject_entity_id=&limit=&offset=` | `atomic_units` (+ provenance count) | |
| `GET /api/v1/entities/{id}/graph?depth=1..5` | `entities`, `relationships` via `entity_neighborhood()` | returns `{root, nodes[], edges[], query_ms}`; latency recorded to `gather_graph_query_duration_seconds` |
| `POST /api/v1/search/semantic` | `atomic_units` / `messages` / `document_segments` | body below |

```jsonc
// POST /api/v1/search/semantic
{ "text": "vps budget",              // full-text (websearch syntax), optional
  "embedding": [0.01, …],            // 768-dim, optional; ranks by cosine when present
  "scope": "atomic_units | messages | document_segments",  // default atomic_units
  "limit": 20 }
// response: { "scope": "...", "hits": [{ "id", "scope", "content", "score", "artifact_id" }] }
```

When the caller supplies no `embedding` but `GATHER_OLLAMA_URL` is configured, the daemon embeds
the query `text` server-side (local Ollama `nomic-embed-text`, loopback-only §5.3) and takes the
vector path; if the embed call fails it degrades to full-text with a warning, so search never
breaks when Ollama is down. Callers can still supply their own 768-dim `embedding` to skip that
hop. With Ollama unset (the default) the daemon never calls out and full-text applies.
`messages` are full-text only (embeddings live on units and segments by design).

### 4.4 Export / import — the `gather-bundle-v1` portable format

| Method & path | Purpose |
|---|---|
| `GET /api/v1/export` | Streams NDJSON: first line a manifest `{"type":"manifest","row":{"format":"gather-bundle-v1","exported_at":…,"tables":[…]}}`, then one `{"type":"<table>","row":{…}}` line per row for all 14 tables in FK order — including raw artifact bytes, embeddings and audit trails |
| `POST /api/v1/import` | Accepts the same NDJSON; single transaction, FK-order inserts, `ON CONFLICT DO NOTHING` (idempotent merge); returns per-table `{in_bundle, inserted}` counts |

Rows are serialized by Postgres (`row_to_json`) and restored with `jsonb_populate_record`, so the
bundle round-trips every column type (bytea, vector, enums) with no bespoke serializers. Verified
end-to-end: export → `TRUNCATE … CASCADE` → import → identical query results. This bundle is also
the unit of optional VPS replication (§7.5).

### 4.5 Contradiction review

| Method & path | Backing tables | Notes |
|---|---|---|
| `GET /api/v1/contradictions?status=open&limit=&offset=` | `contradictions` ⋈ `atomic_units` | ordered by score desc |
| `GET /api/v1/contradictions/{id}` | + `atomic_unit_provenance` ⋈ `artifacts` (both units, all modalities) + `contradiction_audit` | everything the review UI needs in one call |
| `POST /api/v1/contradictions/{id}/resolve` | `contradictions`, `atomic_units`, `relationships`, `contradiction_audit` | body: `{"resolution":"resolved_a|resolved_b|both_valid|dismissed","note":"…","actor":"…"}`; transactional; only `open` rows can be resolved; losing unit → `superseded`, `valid_to` closed, its relationships deactivated |
| `POST /api/v1/contradictions/{id}/annotations` | `contradiction_audit` (action `annotate`) | `201 {"id","contradiction_id","actor"}` |

---

## 5. Extraction pipeline design

All four paths converge on `atomic_units` + `atomic_unit_provenance`. **The worker below is
implemented and tested** (`daemon/src/extract/`: `mod.rs` loop, `pdf.rs`, `image.rs`, `rules.rs`,
`ollama.rs`, `persist.rs`), alongside the synchronous upload-time parts (adapter normalization;
markdown/text extraction + segmentation — `utf8-passthrough`, deterministic heading/paragraph
splitter capped at 2000 chars). Workers claim work via the `pending → processing →
completed|failed|skipped` status columns with `FOR UPDATE SKIP LOCKED` (stale `processing` rows
reset at startup), and unit chunks are stamped `units_extracted_at` atomically with their units,
so a crash mid-extraction is self-healing. Two §5.1 items are deliberately deferred: OCR fallback
for scanned PDFs (needs a native rasterizer; such PDFs are marked `skipped` with
`metadata.reason='scanned-pdf-needs-ocr'` and stay visible on the backlog gauge) and image
captioning (vision model). The pseudocode below remains the behavioral spec the implementation
follows.

### 5.1 Worker loop

```
loop every EXTRACTION_INTERVAL (default 30s):
    # 1. Modality-specific raw extraction
    for doc in SELECT * FROM documents WHERE extraction_status='pending'
               ORDER BY extracted_at NULLS FIRST LIMIT B FOR UPDATE SKIP LOCKED:
        mark processing
        bytes  = raw_content(doc.artifact_id)
        text   = pdfium_extract_text(bytes)            # per page
        if text_density(text) < MIN_DENSITY:           # scanned PDF
            text = tesseract(render_pages(bytes))      # OCR fallback, tool='tesseract-fallback'
        segments = segment(text)                       # same splitter as markdown path
        INSERT document_segments(..., content_hash=sha256(seg))
        UPDATE documents SET extracted_text, page_count, extraction_tool,
                             extraction_status='completed', extracted_at=now()
        counter gather_extraction_segments_total{tool}

    for img in SELECT * FROM images WHERE ocr_status='pending' ... SKIP LOCKED:
        mark processing
        bytes = raw_content(img.artifact_id)
        exif  = parse_exif(bytes)                      # width/height/taken_at/gps→metadata
        ocr   = tesseract(bytes)                       # text + mean word confidence
        cap   = ollama.generate(vision_model, bytes) if CAPTIONING_ENABLED else null
        UPDATE images SET exif, taken_at, ocr_text, ocr_confidence,
                          caption, caption_model, ocr_status='completed'

    # 2. Unified atomic-unit extraction (source-agnostic)
    for chunk in new_unprocessed_chunks():   # messages ∪ document_segments ∪ images.ocr_text,
                                             # tracked via metadata->>'extracted_rev'
        units  = rule_based_extract(chunk)             # §5.2, always on
        units += llm_extract(chunk) if OLLAMA_ENABLED  # §5.3, opt-in
        for u in dedupe_by(normalize(u.statement)):
            unit_id = INSERT atomic_units(kind, statement,
                          statement_hash=sha256(normalized),
                          confidence, extraction_method, extraction_model,
                          valid_from=chunk.timestamp,
                          embedding=ollama.embed(u.statement) if OLLAMA_ENABLED)
                      ON CONFLICT (statement_hash) DO NOTHING
                      or reuse existing id             # re-assertion = extra provenance
            INSERT atomic_unit_provenance(unit_id, chunk.artifact_id,
                          message_id|document_segment_id|image_id,
                          char_start, char_end, quote=u.evidence_span)
            upsert entities(u.subject, u.objects) via entity_aliases; link
            INSERT relationships(subject→object, u.relation, atomic_unit_id=unit_id)
                      ON CONFLICT DO NOTHING
        counter gather_extraction_units_total{method, status}
```

### 5.2 Rule-based extraction (always on, fully offline)

Deterministic, high-precision/low-recall patterns over sentence-split text:

```
FIRST_PERSON_FACT   "I|my|we (am|is|are|use|have|prefer|live|work) …"        -> fact/preference
DECISION            "(we|I) (decided|chose|switched to|will use|agreed) …"   -> decision (valid_from = msg time)
NUMERIC_ASSERTION   "<entity phrase> (is|costs|takes|weighs) <number><unit>" -> claim, attrs={value,unit}
TEMPORAL_EVENT      "(on|since|until) <date>, <clause>"                      -> event, valid_from/valid_to
DEFINITION          "<X> means|is defined as <Y>"                            -> fact + relationship(X, defined_as, Y)
NEGATION            "no longer | not anymore | stopped <verb>ing"            -> closes valid_to on matching prior unit
```

Confidence: pattern base (0.6) + subject-resolution bonus (+0.1 if the subject maps to a known
entity/alias) + source bonus (+0.1 user-authored message; −0.1 OCR text with `ocr_confidence`
< 0.7). Entity mentions are resolved through `entity_aliases` (case-insensitive), creating new
`entities` rows on first sight.

### 5.3 Local-LLM extraction (opt-in: Ollama, localhost only)

```
prompt = SYSTEM: "Extract atomic factual statements from the text. Return JSON:
                  [{kind: fact|claim|decision|preference|event, statement, subject,
                    objects: [{name, relation}], evidence_span, confidence}]
                  Statements must be self-contained and dated where possible. No commentary."
         USER:   chunk.text (≤ 2000 chars — segmentation guarantees this)

out = POST http://127.0.0.1:11434/api/chat {model: GATHER_OLLAMA_MODEL, format: "json"}
validate against JSON schema; drop items without evidence_span ⊆ chunk.text  # anti-hallucination
confidence = model_confidence × 0.9                    # LLM units never outrank rule hits
extraction_method='llm_local', extraction_model='ollama:<model>'
```

If Ollama is unreachable the worker logs once and continues rule-based-only: LLM assist degrades
gracefully and is never a dependency.

### 5.4 Document & image paths feed the same table

Because step 2 iterates *chunks* (`messages` ∪ `document_segments` ∪ `images.ocr_text`) rather
than sources, a PDF paragraph, a screenshot's OCR text, and a chat message are
indistinguishable to extraction and everything downstream — only their provenance anchors differ.

---

## 6. Contradiction detection

### 6.1 Scan algorithm *(shipped: `daemon/src/scan/` — worker in `mod.rs`, pure scoring rules
in `score.rs`; the Ollama judge is opt-in and runs only on structurally flagged pairs)*

```
loop every SCAN_INTERVAL (default 10 min), incremental over units created since last scan:
  candidates(u):                                  # blocking, cheap
      C  = units sharing subject_entity_id with u              (index scan)
      C += top-K by embedding distance, dist < 0.35            (HNSW, if embeddings on)
      C  = C where status='active' and id != u.id
  for u, v in ordered_pairs(candidates):          # unit_a_id < unit_b_id
      skip if (u,v) already in contradictions     # unique pair index
      s = score(u, v)                             # §6.2
      if s >= THRESHOLD (default 0.65):
          INSERT contradictions(u, v, s, method, explanation)
          INSERT contradiction_audit(action='detected', actor='scanner')
```

### 6.2 Conflict scoring

```
score(u, v):
    sim   = cosine(u.embedding, v.embedding)               # topical closeness
    base  = 0
    if NEGATION_MISMATCH(u, v):        base = 0.75  # "X is Y" vs "X is not Y"
    elif NUMERIC_MISMATCH(u, v):       base = 0.80  # same subject+attribute, values differ >10%
    elif ANTONYM_PREDICATE(u, v):      base = 0.60  # curated antonym pairs (on/off, always/never…)
    elif EXCLUSIVE_ASSIGNMENT(u, v):   base = 0.70  # same (subject, functional relation), different object
    else: return 0                                   # no structural signal → not a candidate
    if OLLAMA_ENABLED:                               # optional judge, localhost
        verdict = ollama("Do these two statements contradict? JSON {contradicts, confidence, why}")
        base = 0.5*base + 0.5*verdict.confidence if verdict.contradicts else base*0.4
        explanation = verdict.why
    temporal = 0.5 if validity_windows_disjoint(u, v) else 1.0   # sequenced facts often both true
    return clamp(base * (0.6 + 0.4*sim) * temporal, 0, 1)
```

Source modality never enters the score: chat-vs-chat, chat-vs-PDF, and OCR-vs-anything conflicts
are detected by exactly the same path, because units are modality-blind (§5.4). The UI surfaces
each side's provenance (platform, filename, quote, timestamp) from
`GET /contradictions/{id}`.

### 6.3 Resolution & propagation (shipped and verified)

`POST /contradictions/{id}/resolve` in one transaction:

1. Row locked (`FOR UPDATE`); only `open` may transition — double-resolution is a 400.
2. `contradictions.status` → resolution; `resolved_at/by/note` set.
3. `resolved_a`/`resolved_b`: losing unit → `status='superseded'`,
   `superseded_by_unit_id=<winner>`, `valid_to=coalesce(valid_to, now())`; relationships asserted
   by the losing unit → `status='superseded'`. Graph queries and search filter on
   `status='active'`, so propagation is immediate and reversible (nothing is deleted).
4. `both_valid` / `dismissed`: units untouched (temporal change or false positive).
5. `contradiction_audit` row records actor, from/to status, note — the full history (detection,
   annotations, resolution) is returned by `GET /contradictions/{id}`.

The **≥90% resolved within 7 days** metric comes from `gather_contradictions_open` (gauge,
refreshed every 30 s) against `gather_contradictions_resolved_total` on the Grafana dashboard.

---

## 7. Security model

### 7.1 Threat model boundaries

**In scope**: network attackers (zero listening surface off-loopback); other OS users on a shared
machine (keychain-scoped token, DB password, OS file permissions); accidental cloud exfiltration
(offline-by-default, §7.4); theft of the optional VPS (sees only AES-256 ciphertext on a LUKS
volume). **Out of scope** (accepted for a single-user desktop app): malware running *as the same
OS user*, a hostile root/administrator, and physical attacks on an unlocked, running machine —
no desktop application can defend those.

### 7.2 Authentication — OS-user bound

- The daemon binds `127.0.0.1:7601` (REST) and `127.0.0.1:7602` (gRPC) and **refuses to start**
  on a non-loopback address unless `GATHER_ALLOW_NON_LOOPBACK=true` is set explicitly — the same
  check guards both listeners (implemented in `config.rs`; exercised by CI). The gRPC
  interceptor enforces the same bearer token as the REST middleware, with the same constant-time
  comparison.
- Desktop flow (**implemented**: `daemon/src/auth_token.rs` + the Tauri `get_api_token`
  command): with `GATHER_AUTH_MODE=keychain` the daemon get-or-creates a 256-bit random token in
  the OS keychain — macOS Keychain / Windows Credential Manager / Secret Service (`keyring`
  crate), entry `gather-daemon`/`api-token` — under the current OS user. The Tauri app reads the
  same entry at startup and sends `Authorization: Bearer <token>`. Keychain ACLs make the token
  unreadable to other OS users, which is what binds API access to the OS user session. On hosts
  without a keychain (headless/containers) the daemon logs a prominent warning and continues
  loopback-open rather than failing; containers keep using `env` mode.
- The daemon compares tokens in constant time (`auth.rs`) and exempts only
  `/healthz`, `/readyz`, `/metrics` (loopback-only, non-sensitive).
- Verified on the running stack: no token → 401, wrong token → 401, correct token → 200.

### 7.3 Secrets & encryption at rest

- **No hard-coded secrets anywhere.** Compose refuses to start without `POSTGRES_PASSWORD`
  (`:?` expansion); the API token comes from the keychain (desktop) or env (dev); Terraform takes
  `HCLOUD_TOKEN` from the environment and marks it `sensitive`.
- Postgres auth is scram-sha-256 even for local connections (`POSTGRES_INITDB_ARGS` in compose).
- Encryption at rest, layered: (1) baseline = OS full-disk encryption (FileVault / BitLocker /
  LUKS), the correct layer for a live local database; (2) export bundles for off-machine
  movement are encrypted client-side by restic (AES-256-CTR + Poly1305-AES per its repo format)
  or `age` (X25519 + ChaCha20-Poly1305) for one-off archives; (3) the optional VPS volume adds
  LUKS2 underneath the already-encrypted restic repository.

### 7.4 Network policy — offline by default

- **Default outbound allowlist: empty.** The daemon initiates no outbound connections — its only
  sockets are the loopback listener and localhost Postgres. "Unauthorized outbound traffic" is
  defined as *any* packet leaving the machine that is not one of the two explicit opt-ins below;
  a `curl`-free `docker compose` stack plus loopback port bindings (`127.0.0.1:` prefixes in
  compose) make the default configuration verifiable with `ss -tlnp`.
- Opt-in 1 — **local LLM/OCR**: Ollama at `127.0.0.1:11434` and Tesseract as a subprocess never
  leave the machine (model downloads happen through Ollama's own tooling, at the user's initiative).
- Opt-in 2 — **VPS replication** (§7.5): the only path where data crosses the network, and it is
  never triggered by the daemon itself.
- The Tauri webview's CSP pins `connect-src` to the daemon's loopback origin; the daemon's CORS
  allowlist admits only `tauri://localhost` and the Vite dev origin.

### 7.5 What leaves the machine, exactly, and how it is protected

| Data | Condition (all must hold) | Protection in transit | Protection at rest (remote) |
|---|---|---|---|
| `gather-bundle-v1` export (artifacts incl. raw bytes, units, graph, audit) | User provisioned the VPS via `infra/terraform` **and** initialized restic **and** runs/schedules the backup command themselves | restic encrypts client-side with AES-256 before any byte leaves; transport is SSH (OpenSSH ≥ 9.6: chacha20-poly1305/aes-256-gcm, ed25519 host+client keys, key-only auth, fail2ban) | restic ciphertext on a LUKS2 (AES-256-XTS) volume; VM holds no decryption key for either layer |
| Nothing else | — | — | — |

Restore path: `restic restore` locally → `POST /api/v1/import`. If TLS-tunneled transport is
preferred over raw SSH, wrap the same restic traffic in WireGuard (ChaCha20-Poly1305) or an
stunnel TLS 1.3 listener — the firewall module already leaves 51820/udp documented for that
variant. There is no telemetry, no update phone-home, no crash reporting.

---

## 8. Infrastructure & operations

| Piece | Where | Notes |
|---|---|---|
| Daemon image | `docker/daemon.Dockerfile` | multi-stage (rust:1.94-slim → debian:bookworm-slim), dependency-layer caching, non-root user, tini, HEALTHCHECK |
| Postgres image | `docker/postgres.Dockerfile` | `pgvector/pgvector:pg16` + initdb extension script + desktop-scale tuning |
| Local stack | `docker-compose.yml` | loopback-only ports, healthcheck-gated startup, resource limits, `--profile observability` adds Prometheus+Grafana |
| Optional VPS | `infra/terraform/` | Hetzner CX22 + 20 GB volume + default-deny firewall + cloud-init hardening; **~€5.25/mo (~$6)** list price (CX22 €3.79 + volume €0.96 + IPv4 €0.50) — ceiling $75 |
| gRPC API | `daemon/src/grpc/` + `daemon/build.rs` | tonic server on `127.0.0.1:7602`; proto compiled at build time by `protox` (pure Rust — no system `protoc` in the image or CI) |
| CI/CD | `.github/workflows/ci.yml` (single file) | fmt+clippy → unit+integration tests vs pgvector service → daemon release binary (+ loopback-guard smoke test) → Tauri bundles on Linux/Windows/macOS → artifacts attached to `v*` tag releases |
| Observability | `observability/` | Prometheus scrape config + auto-provisioned 9-panel Grafana dashboard: ingestion throughput by kind, per-file document/image success rate, extraction success rate + backlog, open/resolved contradictions, graph p50/p95 vs 150 ms line, API p95 |
| Scheduled backup + restore drills | `scripts/` | OS-level scheduler (systemd `--user` timer / launchd LaunchAgent / Windows Task Scheduler) invokes `gather-backup.{sh,ps1}` on a timer — the daemon itself is never involved. Tier-1 CI drill (`ci-restore-drill.sh`) proves backup→restore→import on every push; Tier-2 (`vps-restore-drill.sh`) is a user-run runbook against a real VPS + scratch Postgres. See §11 and `docs/BACKUP-RUNBOOK.md`. |

## 9. Roadmap & go/no-go criteria

- **Phase 0 — validation (this repo, now)**: `docker compose up`; ingest a real ChatGPT and
  Claude export plus sample PDFs/markdown/photos through the UI or curl. **Go** when ≥3 sources
  ingest cleanly (chat export, agent log, manual upload ✅ implemented), dedup works on
  re-upload, and `/artifacts` reflects ≥80% of exported conversations within 24h.
  **No-go** → fix adapters before building extraction.
- **Phase 1 — MVP (8 weeks)**: extraction workers (§5), contradiction scanner (§6), review
  dashboard in the Tauri app, remaining platform adapters (Gemini/Grok/Perplexity/Copilot),
  keychain token provisioning. **Go** when ≥70% of sampled units are judged usable and graph
  queries stay <150 ms at personal scale (already 5 ms at seed scale).
- **Phase 2 — hardening**: gRPC server (✅ shipped: all four services on `127.0.0.1:7602`,
  shared cores with REST), server-side query embeddings for semantic search (✅ shipped,
  Ollama opt-in), LLM-assisted extraction (✅ shipped, §5.3), scheduled encrypted export
  (✅ shipped: `scripts/gather-backup.{sh,ps1}` + per-OS scheduler installers — see §11),
  restore drills (✅ shipped: Tier-1 CI drill `scripts/ci-restore-drill.sh`; Tier-2 user
  runbook `scripts/vps-restore-drill.sh`, `docs/BACKUP-RUNBOOK.md`). CI/IaC/observability
  are already in place from day one.
- **Phase 3 — scale (only if measured)**: multi-user namespaces, VPS live replication.
  **Neo4j is explicitly deferred**: adopt only if recursive-CTE traversal p95 exceeds 150 ms at
  >1M relationship rows after index tuning — the `entity_neighborhood()` function is the single
  seam where a graph-store swap would land.

---

## 10. What to run first (quickstart)

```bash
# 0. prerequisites: Docker + Docker Compose v2; (for the desktop app) Rust + Node 22
git clone <this repo> && cd Gather

# 1. configure — one required secret
cp .env.example .env
sed -i "s/^POSTGRES_PASSWORD=$/POSTGRES_PASSWORD=$(openssl rand -hex 24)/" .env

# 2. start the stack (Postgres+pgvector, daemon w/ migrations)
docker compose up --build -d
curl -s http://127.0.0.1:7601/readyz          # {"status":"ready"}

# 3. Phase-0 validation: ingest something real
curl -s -X POST http://127.0.0.1:7601/api/v1/ingest/files \
     -F "file=@$HOME/notes/decisions.md;type=text/markdown"
curl -s -X POST http://127.0.0.1:7601/api/v1/ingest/chat-export \
     -H 'content-type: application/json' \
     -d "{\"platform\":\"chatgpt\",\"data\":$(cat ~/Downloads/chatgpt-export/conversations.json)}"
curl -s "http://127.0.0.1:7601/api/v1/artifacts" | jq '.items[] | {kind, source_platform}'
curl -s -X POST http://127.0.0.1:7601/api/v1/search/semantic \
     -H 'content-type: application/json' \
     -d '{"text":"<something you know is in there>","scope":"document_segments"}' | jq
# gRPC surface (same data, 127.0.0.1:7602):
(cd daemon && cargo run --example grpc_smoke)

# 4. desktop app (drag-and-drop + native picker)
cd apps/desktop && npm install && npm run tauri -- dev

# 5. optional: dashboards at :3000 (admin / $GRAFANA_ADMIN_PASSWORD)
docker compose --profile observability up -d

# 6. optional & opt-in: encrypted off-site backup target (~€5.25/mo)
cd infra/terraform
export HCLOUD_TOKEN=<token> TF_VAR_hcloud_token=$HCLOUD_TOKEN
terraform init && terraform plan \
  -var "ssh_public_key=$(cat ~/.ssh/id_ed25519.pub)" \
  -var "admin_cidr=$(curl -s ifconfig.me)/32"
# review, then: terraform apply  → follow the cloud-init final_message steps

# 7. take a backup (only after 6, entirely at your initiative)
curl -s http://127.0.0.1:7601/api/v1/export -o gather-bundle.ndjson
restic -r sftp:gatherbackup@<vps-ip>:/srv/backups/restic backup gather-bundle.ndjson

# 8. optional: install a recurring scheduled backup (formalizes step 7 — see §11)
RESTIC_REPOSITORY=sftp:gatherbackup@<vps-ip>:/srv/backups/restic RESTIC_PASSWORD=<repo password> \
  scripts/install-schedule-linux.sh   # or install-schedule-macos.sh / install-schedule-windows.ps1

# 9. optional: periodically verify your off-site backup actually restores
RESTIC_REPOSITORY=sftp:gatherbackup@<vps-ip>:/srv/backups/restic RESTIC_PASSWORD=<repo password> \
  scripts/vps-restore-drill.sh
```

Next engineering task after Phase-0 validation: the extraction worker loop (§5.1) —
`documents`/`images` rows in `pending` state are already queuing work for it.

## 11. Scheduled backups stay outside the daemon — on purpose

§7.4 states the daemon's default outbound allowlist is empty and it initiates no
outbound connections of its own; §7.5 states the VPS backup path is "never triggered by
the daemon itself." Scheduled export (§9) is built entirely as OS-level automation —
a systemd `--user` timer, a launchd LaunchAgent, or a Windows Scheduled Task — that
invokes `scripts/gather-backup.{sh,ps1}` directly. That script does exactly what step 7
above does by hand: export via the REST API, then `restic backup`. The daemon process is
never aware any of this is happening and never opens an outbound connection to make it
happen.

**Do not "helpfully" move this into an in-daemon timer.** Doing so would silently break
the security invariant this whole design is built to preserve. If unattended scheduling
that survives logout is ever needed badly enough to reconsider this boundary, that is a
deliberate architecture decision requiring its own review — not a refactor.

See `scripts/README.md` and `docs/BACKUP-RUNBOOK.md` for the full operator guide,
including the two-tier restore-drill design (a CI-safe synthetic round-trip, and a
separate user-run runbook against the real VPS) and the OS-keychain/login-session
caveats that come with scheduling a keychain-authenticated backup unattended.
