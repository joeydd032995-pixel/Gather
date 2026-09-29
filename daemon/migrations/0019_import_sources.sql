-- What automatic import has already looked at.
--
-- Gather can watch an inbox folder and Claude Code's own session folder. Each
-- file it has read is recorded here with the size and modification time it
-- had, so an unchanged file is not read again, and a Claude Code session that
-- has grown is read again (only its new messages are added).
--
-- Paths are specific to this computer: bundle export leaves this table out.
-- The reverse script is migrations-down/0019_import_sources.down.sql.

CREATE TABLE import_sources (
    path       text PRIMARY KEY,
    kind       text        NOT NULL CHECK (kind IN ('claude_code', 'inbox')),
    status     text        NOT NULL CHECK (status IN ('imported', 'unrecognized', 'failed')),
    size       bigint      NOT NULL DEFAULT 0,
    mtime_ns   bigint      NOT NULL DEFAULT 0,
    session_id text,
    messages   integer     NOT NULL DEFAULT 0,
    detail     text,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX import_sources_recent_idx ON import_sources (updated_at DESC);
CREATE INDEX import_sources_kind_idx ON import_sources (kind, status);
