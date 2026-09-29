-- Reverse of migrations/0017_extraction_failures.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 17;
-- Chunks set aside after an error are queued again, so the next pass
-- retries them.

BEGIN;

UPDATE messages          SET units_extracted_at = NULL WHERE units_extract_error IS NOT NULL;
UPDATE document_segments SET units_extracted_at = NULL WHERE units_extract_error IS NOT NULL;
UPDATE images            SET units_extracted_at = NULL WHERE units_extract_error IS NOT NULL;

DROP INDEX IF EXISTS document_segments_units_pending_doc_idx;
ALTER TABLE messages          DROP COLUMN units_extract_error;
ALTER TABLE document_segments DROP COLUMN units_extract_error;
ALTER TABLE images            DROP COLUMN units_extract_error;

COMMIT;
