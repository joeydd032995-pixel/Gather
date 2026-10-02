-- Keep the previous state of corrected claims and guard delayed worker writes.
ALTER TABLE atomic_units ADD COLUMN content_revision bigint NOT NULL DEFAULT 0;
CREATE TABLE atomic_unit_revisions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    atomic_unit_id uuid NOT NULL REFERENCES atomic_units(id) ON DELETE CASCADE,
    revision bigint NOT NULL,
    before_state jsonb NOT NULL,
    relationships jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (atomic_unit_id, revision)
);
CREATE FUNCTION gather_unit_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF ROW(NEW.statement, NEW.attrs, NEW.subject_entity_id, NEW.valid_from,
           NEW.valid_to, NEW.status) IS DISTINCT FROM
       ROW(OLD.statement, OLD.attrs, OLD.subject_entity_id, OLD.valid_from,
           OLD.valid_to, OLD.status) THEN
        NEW.content_revision := OLD.content_revision + 1;
    END IF;
    IF NEW.statement IS DISTINCT FROM OLD.statement THEN
        INSERT INTO atomic_unit_revisions
            (atomic_unit_id, revision, before_state, relationships)
        SELECT OLD.id, OLD.content_revision, to_jsonb(OLD),
               coalesce(jsonb_agg(to_jsonb(r)), '[]'::jsonb)
        FROM relationships r WHERE r.atomic_unit_id = OLD.id
        ON CONFLICT DO NOTHING;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER gather_unit_revision BEFORE UPDATE ON atomic_units
FOR EACH ROW EXECUTE FUNCTION gather_unit_revision();
