-- Reverse of migrations/0014_semantic_safety.sql.
--
-- Run by hand (psql -v ON_ERROR_STOP=1 -f ...) against a stopped daemon,
-- then delete the migration's row so sqlx doesn't consider it applied:
--   DELETE FROM _sqlx_migrations WHERE version = 14;
-- Restoring the previous binary afterwards gives the pre-0014 behaviour.
-- Certificates, user photo decisions and declared derivations are lost;
-- nothing else is (unit time fields are copies of existing data).

BEGIN;

ALTER TABLE contradictions
    DROP COLUMN IF EXISTS certainty,
    DROP COLUMN IF EXISTS alignment,
    DROP COLUMN IF EXISTS certificate_id;

ALTER TABLE atomic_units
    DROP COLUMN IF EXISTS observed_at,
    DROP COLUMN IF EXISTS asserted_at;

ALTER TABLE artifacts
    DROP COLUMN IF EXISTS retraction_reason,
    DROP COLUMN IF EXISTS retracted_at;

DROP TABLE IF EXISTS artifact_derivations;
DROP TABLE IF EXISTS semantic_user_decisions;
DROP TABLE IF EXISTS inference_certificates;

COMMIT;
