-- Tighten atomic_unit_provenance's anchor invariant from "at most one" to
-- "exactly one". Every provenance row is written with exactly one fine-grained
-- anchor (message | document_segment | image) — see the only insert site,
-- extract/persist.rs, which sets the anchor from the 3-variant ChunkAnchor
-- enum. The original 0001 CHECK used `<= 1`, which silently permitted a
-- zero-anchor row that would break the "why do you believe X?" provenance
-- guarantee. No code path produces such a row, so this is a safe forward-only
-- tightening (consistent with the migrations' no-rollback convention). If a
-- pre-existing zero-anchor row somehow exists, ADD CONSTRAINT fails loudly —
-- which is the correct outcome, not a silent data-integrity hole.

ALTER TABLE atomic_unit_provenance
    DROP CONSTRAINT provenance_single_anchor;

-- Add the stricter CHECK as NOT VALID so the ADD takes only a brief lock
-- instead of scanning the whole table under ACCESS EXCLUSIVE, then validate it
-- separately (VALIDATE CONSTRAINT takes the weaker SHARE UPDATE EXCLUSIVE lock,
-- which does not block reads/writes). This daemon runs migrations at startup
-- before serving traffic, so the lock is moot here, but it keeps the migration
-- cheap on a large table and satisfies the standard safe-migration lint.
ALTER TABLE atomic_unit_provenance
    ADD CONSTRAINT provenance_single_anchor CHECK (
        (message_id IS NOT NULL)::int
        + (document_segment_id IS NOT NULL)::int
        + (image_id IS NOT NULL)::int = 1
    ) NOT VALID;

ALTER TABLE atomic_unit_provenance
    VALIDATE CONSTRAINT provenance_single_anchor;
