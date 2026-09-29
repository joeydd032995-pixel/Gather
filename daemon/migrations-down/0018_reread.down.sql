-- Reverse of migrations/0018_reread.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 18;

BEGIN;

DROP TABLE IF EXISTS reread_jobs;
ALTER TABLE messages          DROP COLUMN units_llm_model, DROP COLUMN units_reread_job;
ALTER TABLE document_segments DROP COLUMN units_llm_model, DROP COLUMN units_reread_job;
ALTER TABLE images            DROP COLUMN units_llm_model, DROP COLUMN units_reread_job;

COMMIT;
