-- Reverse of migrations/0016_projects.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 16;
-- Project trees are lost. Word files and spreadsheets are deleted, with what
-- was extracted from them: PostgreSQL can't drop enum values, so the kind type
-- is rebuilt without them. Everything else in the projects stays in Gather.

BEGIN;

DROP TABLE IF EXISTS project_items;
DROP TABLE IF EXISTS projects;

DELETE FROM artifacts WHERE kind::text IN ('document_docx', 'document_spreadsheet');

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
