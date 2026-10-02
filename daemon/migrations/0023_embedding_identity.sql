-- Legacy vectors have unknown identity and must be rebuilt locally.
CREATE TABLE embedding_state (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    model text,
    generation bigint NOT NULL DEFAULT 0
);
INSERT INTO embedding_state(singleton) VALUES (true);
ALTER TABLE atomic_units ADD COLUMN embedding_model text;
ALTER TABLE document_segments ADD COLUMN embedding_model text;
ALTER TABLE entities ADD COLUMN embedding_model text;
ALTER TABLE images ADD COLUMN embedding_model text;
ALTER TABLE atomic_units ADD COLUMN embedding_retry_at timestamptz;
ALTER TABLE atomic_units ADD COLUMN embedding_attempts integer NOT NULL DEFAULT 0;
UPDATE atomic_units SET embedding = NULL, clustered_at = NULL,
    topic_cluster_id = NULL, contradiction_scanned_at = NULL;
UPDATE document_segments SET embedding = NULL;
UPDATE entities SET embedding = NULL;
UPDATE images SET embedding = NULL, captioned_at = NULL, topic_cluster_id = NULL;
DELETE FROM cluster_members m USING clusters c WHERE m.cluster_id = c.id
    AND (m.member_kind = 'unit' OR (m.member_kind = 'image' AND c.kind = 'photo_topic'));
