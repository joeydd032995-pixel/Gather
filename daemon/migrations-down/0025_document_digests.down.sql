-- Reverse of migrations/0025_document_digests.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 25;
-- Digests are derived from the documents' text and are rebuilt by the
-- extraction worker if the migration is applied again.

BEGIN;

DROP TABLE IF EXISTS document_digests;

COMMIT;
