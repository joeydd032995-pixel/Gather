-- A persisted generation catches changes that row counts cannot detect.
-- A sequence avoids a shared-row lock on otherwise independent transactions.
CREATE SEQUENCE gather_semantic_revision;
CREATE FUNCTION gather_touch_semantics() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM nextval('gather_semantic_revision');
    RETURN NULL;
END $$;
DO $$
DECLARE tab text;
BEGIN
    FOREACH tab IN ARRAY ARRAY['atomic_units', 'relationships', 'entities',
        'entity_aliases', 'atomic_unit_provenance', 'artifacts',
        'documents', 'document_segments', 'project_items', 'projects']
    LOOP
        EXECUTE format('CREATE TRIGGER gather_touch_semantics
            AFTER INSERT OR UPDATE OR DELETE ON %I
            FOR EACH STATEMENT EXECUTE FUNCTION gather_touch_semantics()', tab);
    END LOOP;
END $$;
