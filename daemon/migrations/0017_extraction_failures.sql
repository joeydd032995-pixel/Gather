-- Reading a chunk (message, document segment, image OCR text) into units
-- can fail for that chunk alone. It is then set aside with its error rather
-- than retried on every pass: a chunk that always fails used to stop the
-- whole queue behind it. `units_extracted_at` is stamped as for any other
-- chunk, so the file stops showing as still being read.
--
-- Also an index for taking a document's segments in order, so files finish
-- one at a time instead of all at once at the end of a long queue.
-- The reverse script is migrations-down/0017_extraction_failures.down.sql.

ALTER TABLE messages          ADD COLUMN units_extract_error text;
ALTER TABLE document_segments ADD COLUMN units_extract_error text;
ALTER TABLE images            ADD COLUMN units_extract_error text;

CREATE INDEX document_segments_units_pending_doc_idx
    ON document_segments (document_id, seq) WHERE units_extracted_at IS NULL;
