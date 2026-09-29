-- Reading files again with the AI model.
--
-- A model that is switched on later only reads what is imported afterwards.
-- To let it go back over earlier files, each chunk (message, document
-- segment, image text) now remembers which model has read it, and a
-- "re-read" job walks the chunks it hasn't read yet.
--
--   units_llm_model   the model that read the chunk ("ollama:<name>"), NULL
--                     when only the built-in rules have (or the model failed)
--   units_reread_job  the last re-read job that visited the chunk, so one
--                     job never meets the same chunk twice, even if the model
--                     failed on it
--
-- The reverse script is migrations-down/0018_reread.down.sql.

ALTER TABLE messages          ADD COLUMN units_llm_model text, ADD COLUMN units_reread_job uuid;
ALTER TABLE document_segments ADD COLUMN units_llm_model text, ADD COLUMN units_reread_job uuid;
ALTER TABLE images            ADD COLUMN units_llm_model text, ADD COLUMN units_reread_job uuid;

CREATE TABLE reread_jobs (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    model       text        NOT NULL,
    status      text        NOT NULL DEFAULT 'running'
                CHECK (status IN ('running', 'done', 'cancelled')),
    total       bigint      NOT NULL,
    done        bigint      NOT NULL DEFAULT 0,
    failed      bigint      NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);

-- One job at a time.
CREATE UNIQUE INDEX reread_jobs_one_running_idx ON reread_jobs ((true)) WHERE status = 'running';
