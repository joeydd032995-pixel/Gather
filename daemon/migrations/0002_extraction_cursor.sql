-- Extraction worker cursors: a chunk (message, document segment, or image
-- OCR text) is eligible for atomic-unit extraction while its marker is NULL;
-- the worker stamps it after a pass. Partial indexes keep the "what's next"
-- scan index-only regardless of table size.

ALTER TABLE messages          ADD COLUMN units_extracted_at timestamptz;
ALTER TABLE document_segments ADD COLUMN units_extracted_at timestamptz;
ALTER TABLE images            ADD COLUMN units_extracted_at timestamptz;

CREATE INDEX messages_units_pending_idx
    ON messages (id) WHERE units_extracted_at IS NULL;
CREATE INDEX document_segments_units_pending_idx
    ON document_segments (id) WHERE units_extracted_at IS NULL;
CREATE INDEX images_units_pending_idx
    ON images (id) WHERE units_extracted_at IS NULL;
