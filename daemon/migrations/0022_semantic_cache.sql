-- Commit-visible revisions catch semantic changes without a shared-row lock.
-- Cache checks compact the ledger and mark that compaction atomically. Even an
-- older transaction that commits late cannot hide behind a newer generation.
CREATE SEQUENCE gather_semantic_revision;
CREATE TABLE gather_semantic_revisions (
    revision bigint PRIMARY KEY DEFAULT nextval('gather_semantic_revision')
);
INSERT INTO gather_semantic_revisions DEFAULT VALUES;
CREATE FUNCTION gather_touch_semantics() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO gather_semantic_revisions DEFAULT VALUES;
    RETURN NULL;
END $$;
DO $$
DECLARE tab text; fields text; old_fields text; new_fields text;
BEGIN
    FOR tab, fields IN SELECT * FROM (VALUES
        ('atomic_units', 'status,subject_entity_id,statement'),
        ('relationships', 'atomic_unit_id,source_entity_id,target_entity_id,status'),
        ('entities', 'name,merged_into_entity_id'),
        ('entity_aliases', 'entity_id,alias'),
        ('atomic_unit_provenance', 'atomic_unit_id,artifact_id'),
        ('artifacts', 'retracted_at,content_hash'),
        ('documents', 'artifact_id'),
        ('document_segments', 'document_id,content,embedding'),
        ('project_items', 'project_id,artifact_id,path,status,item_kind'),
        ('projects', 'name,source')
    ) AS inputs(tab, fields)
    LOOP
        EXECUTE format('CREATE TRIGGER gather_touch_semantics
            AFTER INSERT OR DELETE ON %I
            FOR EACH STATEMENT EXECUTE FUNCTION gather_touch_semantics()', tab);
        SELECT string_agg('OLD.' || quote_ident(field), ','),
               string_agg('NEW.' || quote_ident(field), ',')
            INTO old_fields, new_fields
            FROM unnest(string_to_array(fields, ',')) AS f(field);
        EXECUTE format('CREATE TRIGGER gather_update_semantics
            AFTER UPDATE ON %I FOR EACH ROW
            WHEN (ROW(%s) IS DISTINCT FROM ROW(%s))
            EXECUTE FUNCTION gather_touch_semantics()', tab, old_fields, new_fields);
    END LOOP;
END $$;
