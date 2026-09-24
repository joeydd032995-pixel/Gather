-- Active learning + auto-tuning (autonomous pipeline, Phase C).
--
-- The tuner (src/tune) reads the user's latest verdict per target from
-- unit_feedback and nudges the live thresholds in decision_tuning. Two
-- additions make that sound and auditable. All local, offline.

-- The score the item carried WHEN it was labelled. A unit's confidence or a
-- merge pair's similarity can drift (re-scoring, edits), so the tuner must
-- learn from the value the user actually judged. Nullable: rows written before
-- this migration fall back to the current unit confidence.
ALTER TABLE unit_feedback ADD COLUMN score real;

-- Every threshold change, automatic or manual (reset), with the evidence that
-- justified it. The tuner is only trustworthy if every move it made can be
-- inspected and undone.
CREATE TABLE decision_tuning_audit (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    key        text NOT NULL,
    old_value  double precision,                        -- null when first tuned (was the env default)
    new_value  double precision,                        -- null when reset back to the env default
    actor      text NOT NULL DEFAULT 'auto-tuner',
    reason     jsonb NOT NULL DEFAULT '{}'::jsonb,      -- precision, support, rule that fired
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX decision_tuning_audit_key_idx ON decision_tuning_audit (key, created_at DESC);
