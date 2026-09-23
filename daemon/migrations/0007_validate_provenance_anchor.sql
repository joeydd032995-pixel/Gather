-- Validate the provenance_single_anchor CHECK added NOT VALID in 0006. Split
-- into its own migration (its own transaction) so the ADD's ACCESS EXCLUSIVE
-- lock is released before this scan, which takes the weaker SHARE UPDATE
-- EXCLUSIVE lock and does not block concurrent reads/writes. On a fresh
-- install the table is empty and this is instantaneous.

ALTER TABLE atomic_unit_provenance
    VALIDATE CONSTRAINT provenance_single_anchor;
