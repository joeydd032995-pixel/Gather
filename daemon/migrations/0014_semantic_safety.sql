-- Semantic safety: inference certificates, user decisions as evidence,
-- source derivations, and separate time fields for claims.
--
-- Purely additive: new tables, new nullable columns and indexes. Nothing
-- existing is rewritten except a backfill of the new atomic_units.asserted_at
-- from valid_from, which is what valid_from held for every non-event unit.
-- The reverse script is migrations-down/0014_semantic_safety.down.sql (kept
-- outside this directory: sqlx and the CI psql loop apply every file here).

-- Every consequential automatic conclusion — and every one the safety layer
-- stopped — gets a certificate: the rule and version, the direct inputs,
-- source artifacts and families, config/scope/time used, the predicates
-- evaluated and the typed reason codes of those that failed.
CREATE TABLE inference_certificates (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    conclusion_kind      text NOT NULL,
    -- Deterministic identity of the conclusion (e.g. 'entity-merge:<ids>').
    conclusion_key       text NOT NULL,
    -- The row that materializes it (merge audit row, cluster, contradiction, unit).
    conclusion_id        uuid,
    subject_ids          uuid[] NOT NULL DEFAULT '{}',
    rule_id              text NOT NULL,
    rule_version         integer NOT NULL,
    -- What the rule decided; never changes.
    decision             text NOT NULL,
    -- Where it stands now: the decision, or superseded / retracted later.
    outcome              text NOT NULL,
    evidence_class       text NOT NULL,
    inputs               jsonb NOT NULL DEFAULT '[]'::jsonb,
    input_ids            uuid[] NOT NULL DEFAULT '{}',
    source_artifact_ids  uuid[] NOT NULL DEFAULT '{}',
    source_family_ids    uuid[] NOT NULL DEFAULT '{}',
    model_version        text,
    config               jsonb NOT NULL DEFAULT '{}'::jsonb,
    scope                jsonb NOT NULL DEFAULT '{}'::jsonb,
    temporal             jsonb NOT NULL DEFAULT '{}'::jsonb,
    predicates           jsonb NOT NULL DEFAULT '[]'::jsonb,
    reason_codes         text[] NOT NULL DEFAULT '{}',
    explanation          text NOT NULL DEFAULT '',
    evidence_digest      char(64) NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    -- Withdrawal: when, why, and the certificate (event) that caused it.
    superseded_at        timestamptz,
    retracted_at         timestamptz,
    status_reason        text,
    -- Deferrable so a bundle import can restore rows in any order.
    caused_by            uuid REFERENCES inference_certificates (id) ON DELETE SET NULL
                         DEFERRABLE INITIALLY IMMEDIATE,
    CONSTRAINT inference_certificates_decision_ck CHECK
        (decision IN ('auto_applied', 'needs_review', 'blocked', 'user_decision')),
    CONSTRAINT inference_certificates_outcome_ck CHECK
        (outcome IN ('auto_applied', 'needs_review', 'blocked', 'user_decision',
                     'superseded', 'retracted')),
    CONSTRAINT inference_certificates_class_ck CHECK
        (evidence_class IN ('asserted', 'extracted', 'inferred', 'user_confirmed',
                            'rejected', 'blocked'))
);

-- Re-evaluating the same evidence under the same rule is a no-op: one live
-- certificate per (rule, conclusion, evidence).
CREATE UNIQUE INDEX inference_certificates_live_uq
    ON inference_certificates (rule_id, conclusion_key, evidence_digest)
    WHERE superseded_at IS NULL AND retracted_at IS NULL;
CREATE INDEX inference_certificates_key_idx
    ON inference_certificates (conclusion_key, created_at DESC);
CREATE INDEX inference_certificates_conclusion_idx
    ON inference_certificates (conclusion_id) WHERE conclusion_id IS NOT NULL;
CREATE INDEX inference_certificates_subjects_gin
    ON inference_certificates USING gin (subject_ids);
CREATE INDEX inference_certificates_inputs_gin
    ON inference_certificates USING gin (input_ids);
CREATE INDEX inference_certificates_sources_gin
    ON inference_certificates USING gin (source_artifact_ids);
CREATE INDEX inference_certificates_reasons_gin
    ON inference_certificates USING gin (reason_codes);
CREATE INDEX inference_certificates_outcome_idx
    ON inference_certificates (outcome, conclusion_kind, created_at DESC);
CREATE INDEX inference_certificates_caused_by_idx
    ON inference_certificates (caused_by) WHERE caused_by IS NOT NULL;

-- Explicit user decisions about pairs that no rule may reverse automatically
-- (entity dismissals keep living in entity_merge_audit).
CREATE TABLE semantic_user_decisions (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind        text NOT NULL,
    a_id        uuid NOT NULL,
    b_id        uuid NOT NULL,
    actor       text NOT NULL DEFAULT 'local-user',
    note        text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    revoked_at  timestamptz,
    CONSTRAINT semantic_user_decisions_kind_ck CHECK (kind IN ('photo_not_duplicate')),
    CONSTRAINT semantic_user_decisions_ordered_ck CHECK (a_id < b_id)
);
CREATE UNIQUE INDEX semantic_user_decisions_live_uq
    ON semantic_user_decisions (kind, a_id, b_id) WHERE revoked_at IS NULL;

-- Declared derivations between artifacts (a copy, a summary, an export, a
-- correction). Derived artifacts share a source family and never count as
-- independent corroboration of their origin.
CREATE TABLE artifact_derivations (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    child_artifact_id  uuid NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    parent_artifact_id uuid NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    kind               text NOT NULL,
    actor              text NOT NULL DEFAULT 'local-user',
    created_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT artifact_derivations_kind_ck CHECK
        (kind IN ('copy', 'summary', 'export', 'reingest', 'version', 'correction', 'other')),
    CONSTRAINT artifact_derivations_distinct_ck CHECK (child_artifact_id <> parent_artifact_id)
);
CREATE UNIQUE INDEX artifact_derivations_pair_uq
    ON artifact_derivations (child_artifact_id, parent_artifact_id);
CREATE INDEX artifact_derivations_parent_idx ON artifact_derivations (parent_artifact_id);

-- A retracted source stays on disk (unless deleted) but supports nothing.
ALTER TABLE artifacts
    ADD COLUMN retracted_at      timestamptz,
    ADD COLUMN retraction_reason text;

-- Time, kept apart: when the source said it, when the event was observed.
-- valid_from/valid_to (the claimed validity) and created_at (ingestion)
-- already exist.
ALTER TABLE atomic_units
    ADD COLUMN asserted_at timestamptz,
    ADD COLUMN observed_at timestamptz;

UPDATE atomic_units
   SET asserted_at = valid_from
 WHERE valid_from IS NOT NULL AND attrs->>'pattern' IS DISTINCT FROM 'temporal_event';
UPDATE atomic_units
   SET observed_at = valid_from
 WHERE valid_from IS NOT NULL AND attrs->>'pattern' = 'temporal_event';

-- A contradiction points at the certificate that reported it and carries
-- the seven-dimension alignment behind it.
ALTER TABLE contradictions
    ADD COLUMN certificate_id uuid REFERENCES inference_certificates (id) ON DELETE SET NULL
                              DEFERRABLE INITIALLY IMMEDIATE,
    ADD COLUMN alignment      jsonb,
    ADD COLUMN certainty      text;
