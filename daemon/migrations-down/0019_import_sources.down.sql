-- Reverse of migrations/0019_import_sources.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 19;
-- Conversations already imported stay; only the record of which files were
-- read goes, so files still in a watched folder would be read once more (and
-- deduplicated).

BEGIN;

DROP TABLE IF EXISTS import_sources;

COMMIT;
