-- Entity resolution write path. 0001_init laid out the full design —
-- entity_aliases, entities.merged_into_entity_id, and the partial unique index
-- entities_name_kind_uq (... WHERE merged_into_entity_id IS NULL) — and
-- resolve_or_create_entity already reads entities UNION aliases. Nothing ever
-- wrote either one, so the alias branch of that resolver never matched and
-- every surface form of a name ('Postgres' / 'PostgreSQL' / 'PG') became its
-- own graph node. This migration adds the audit trail the write path needs.

-- Merge history. Same shape and intent as contradiction_audit (0001): an
-- append-only record of reviewer decisions, keyed to the pair it concerns.
-- action 'merge'   — loser folded into winner (merged_into_entity_id set).
-- action 'dismiss' — reviewer rejected a suggested pair; suppresses the
--                    suggestion without changing either entity.
CREATE TABLE entity_merge_audit (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    winner_entity_id uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    loser_entity_id  uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    action           text NOT NULL,                    -- 'merge','dismiss'
    actor            text NOT NULL DEFAULT 'local-user',
    note             text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT entity_merge_audit_distinct CHECK (winner_entity_id <> loser_entity_id)
);

-- History for one entity, newest last (mirrors contradiction_audit_cid_idx).
CREATE INDEX entity_merge_audit_winner_idx
    ON entity_merge_audit (winner_entity_id, created_at);

-- Suggestion filtering looks a candidate pair up in both orderings, since a
-- dismissal of (a,b) must also suppress (b,a). Unlike contradictions, the pair
-- cannot be normalized by a CHECK: for 'merge' rows the column order carries
-- meaning (which entity survived), so both directions get an index instead.
CREATE INDEX entity_merge_audit_pair_idx
    ON entity_merge_audit (winner_entity_id, loser_entity_id, action);
CREATE INDEX entity_merge_audit_rpair_idx
    ON entity_merge_audit (loser_entity_id, winner_entity_id, action);

-- Suggestion scans read every live entity and score pairs in the daemon, so
-- the driving predicate is "not merged away".
CREATE INDEX entities_unmerged_idx
    ON entities (id)
    WHERE merged_into_entity_id IS NULL;

-- Entity embeddings are an opt-in enhancement (Ollama, §5.3): entities_embedding_hnsw
-- has always indexed an all-NULL column because nothing populated it. This
-- backfill cursor mirrors atomic_units_scan_pending_idx (0003) so
-- embed_pending_entities can find work without a sequential scan.
CREATE INDEX entities_embed_pending_idx
    ON entities (id)
    WHERE embedding IS NULL AND merged_into_entity_id IS NULL;
