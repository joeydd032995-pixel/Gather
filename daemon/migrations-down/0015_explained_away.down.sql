-- Reverse of migrations/0015_explained_away.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 15;
-- Your reviews of explained-away contradictions are lost. Contradictions you
-- promoted stay in the contradictions table, and their certificates are kept.

BEGIN;

DELETE FROM semantic_user_decisions
 WHERE kind IN ('contradiction_confirmed', 'contradiction_not_conflict');
ALTER TABLE semantic_user_decisions DROP CONSTRAINT semantic_user_decisions_kind_ck;
ALTER TABLE semantic_user_decisions ADD CONSTRAINT semantic_user_decisions_kind_ck CHECK
    (kind IN ('photo_not_duplicate'));

COMMIT;
