-- Reversible entity merges.
--
-- A merge rewrites a lot of rows: the loser's units and edges move to the
-- winner, its aliases move or are dropped, clashing edges are deleted, and
-- anything previously merged into the loser is flattened onto the winner. To
-- undo that precisely, each merge now journals exactly what it changed. Merges
-- recorded before this migration have no journal and cannot be undone
-- automatically.

ALTER TABLE entity_merge_audit
    -- What the merge changed (unit ids, edge ids and deleted edge rows,
    -- aliases added/moved, flattened descendants). Null for 'dismiss' rows
    -- and for merges made before this migration.
    ADD COLUMN undo      jsonb,
    -- The similarity that justified an automatic or tray-accepted merge; the
    -- tuner learns from it when the merge is undone. Null for manual merges.
    ADD COLUMN score     real,
    -- Set when the merge is reversed; the reversal itself is an 'unmerge' row.
    ADD COLUMN undone_at timestamptz;

-- "The live merge that folded this entity away": what unmerge looks up.
CREATE INDEX entity_merge_audit_undoable_idx
    ON entity_merge_audit (loser_entity_id, created_at DESC)
    WHERE action = 'merge' AND undone_at IS NULL;
