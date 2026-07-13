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
