-- A digest of each document: what it is about and the few sentences that say
-- the most (extract/digest.rs). Built by the extraction worker once a
-- document's text is read; `method` says how it was written.
--
--   'extractive'   key sentences chosen from the file itself; always present
--   'llm:<model>'  the same, with a summary, takeaways and open questions
--                  reworded by the local model and checked against the text
--
-- One row per artifact; deleting the artifact deletes its digest.

CREATE TABLE document_digests (
    artifact_id    uuid PRIMARY KEY REFERENCES artifacts (id) ON DELETE CASCADE,
    method         text        NOT NULL,
    summary        text        NOT NULL DEFAULT '',
    -- [{text, score, segment_seq}], in reading order
    key_points     jsonb       NOT NULL DEFAULT '[]'::jsonb,
    topics         jsonb       NOT NULL DEFAULT '[]'::jsonb,
    outline        jsonb       NOT NULL DEFAULT '[]'::jsonb,
    -- model-written, grounded in the text; empty for 'extractive'
    takeaways      jsonb       NOT NULL DEFAULT '[]'::jsonb,
    open_questions jsonb       NOT NULL DEFAULT '[]'::jsonb,
    stats          jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
