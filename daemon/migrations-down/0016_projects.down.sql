-- Reverse of migrations/0016_projects.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 16;
-- Project trees are lost; the files in the projects stay in Gather.
--
-- PostgreSQL can't drop enum values, so the kind type is rebuilt without the
-- kinds this migration added. Files of those kinds (Word files, spreadsheets,
-- files kept without text) must be deleted through Gather first, so what was
-- learned from them is withdrawn properly (DELETE /api/v1/artifacts/{id});
-- deleting the rows here would leave their statements standing without a
-- source. The script refuses while any remain. To list them:
--   SELECT id, original_filename FROM artifacts
--    WHERE kind::text IN ('document_docx', 'document_spreadsheet', 'file_other');

BEGIN;

DO $$
DECLARE
    remaining bigint;
BEGIN
    SELECT count(*) INTO remaining FROM artifacts
     WHERE kind::text IN ('document_docx', 'document_spreadsheet', 'file_other');
    IF remaining > 0 THEN
        RAISE EXCEPTION '% Word, spreadsheet or kept files are still in Gather; delete them through Gather (DELETE /api/v1/artifacts/{id}) before reversing migration 16', remaining;
    END IF;
END
$$;

DROP TABLE IF EXISTS project_items;
DROP TABLE IF EXISTS projects;

ALTER TYPE artifact_kind RENAME TO artifact_kind_0016;
CREATE TYPE artifact_kind AS ENUM (
    'chat_export',
    'agent_log',
    'document_pdf',
    'document_markdown',
    'document_text',
    'image_photo',
    'image_screenshot'
);
ALTER TABLE artifacts ALTER COLUMN kind TYPE artifact_kind USING kind::text::artifact_kind;
DROP TYPE artifact_kind_0016;

COMMIT;
