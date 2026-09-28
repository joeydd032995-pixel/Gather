-- Projects: a folder (or .zip) uploaded as a whole, kept as the tree it came
-- in: project -> folders -> files. Each file that Gather can read becomes an
-- ordinary artifact; the tree only records where it sat and what happened to
-- it (ingested, the same as a file already in Gather, skipped with a reason).
--
-- Also two artifact kinds for the office formats projects bring along.
-- The reverse script is migrations-down/0016_projects.down.sql.

ALTER TYPE artifact_kind ADD VALUE IF NOT EXISTS 'document_docx';
ALTER TYPE artifact_kind ADD VALUE IF NOT EXISTS 'document_spreadsheet';

CREATE TABLE projects (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name        text NOT NULL CHECK (length(btrim(name)) > 0),
    -- How it arrived: a folder picked or dropped, or a .zip unpacked here.
    source      text NOT NULL CHECK (source IN ('folder', 'zip')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE project_items (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  uuid NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    -- NULL for the project's top-level entries. Deferrable so a bundle
    -- import can restore rows in any order.
    parent_id   uuid REFERENCES project_items (id) ON DELETE CASCADE
                DEFERRABLE INITIALLY IMMEDIATE,
    item_kind   text NOT NULL CHECK (item_kind IN ('folder', 'file')),
    name        text NOT NULL,
    -- Relative to the project root, '/'-separated, no leading slash.
    path        text NOT NULL,
    depth       integer NOT NULL CHECK (depth >= 0),
    -- The file's content in Gather. Kept when the project is removed; set
    -- NULL when the artifact is deleted.
    artifact_id uuid REFERENCES artifacts (id) ON DELETE SET NULL
                DEFERRABLE INITIALLY IMMEDIATE,
    status      text NOT NULL
                CHECK (status IN ('folder', 'ingested', 'deduplicated', 'skipped', 'failed')),
    -- Why a file was skipped or failed, in plain language.
    detail      text,
    byte_size   bigint CHECK (byte_size >= 0),
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT project_items_kind_status_ck CHECK ((item_kind = 'folder') = (status = 'folder')),
    CONSTRAINT project_items_path_uq UNIQUE (project_id, path)
);
CREATE INDEX project_items_parent_idx ON project_items (project_id, parent_id);
CREATE INDEX project_items_artifact_idx ON project_items (artifact_id)
    WHERE artifact_id IS NOT NULL;

CREATE TRIGGER projects_touch BEFORE UPDATE ON projects
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
