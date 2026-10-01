-- Keep source lineage after provenance is removed by a hard artifact delete.
-- Source-free manual claims remain distinguishable from extracted orphans.
ALTER TABLE atomic_units ADD COLUMN has_source_history boolean NOT NULL DEFAULT false;
UPDATE atomic_units u SET has_source_history = true
WHERE EXISTS (SELECT 1 FROM atomic_unit_provenance p WHERE p.atomic_unit_id = u.id);
CREATE FUNCTION gather_remember_unit_source() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE atomic_units SET has_source_history = true
    WHERE id = NEW.atomic_unit_id AND NOT has_source_history;
    RETURN NEW;
END $$;
CREATE TRIGGER gather_remember_unit_source
AFTER INSERT OR UPDATE OF atomic_unit_id ON atomic_unit_provenance
FOR EACH ROW EXECUTE FUNCTION gather_remember_unit_source();
