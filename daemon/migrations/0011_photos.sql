-- Photo pipeline (autonomous pipeline, Phase D).
--
-- Photos are organised the same way units are: grouped, never deleted.
--   'photo_dup'   — near-duplicate shots (perceptual hash), one representative.
--   'album'       — EXIF time/place sessions.
--   'photo_topic' — visually similar photos (opt-in local vision captions).
-- Each is a reversible tag column on images, re-derived as photos arrive. All
-- local, offline: hashing is pure Rust, captions need a loopback Ollama model.

ALTER TABLE images
    ADD COLUMN phash             bigint,                 -- 64-bit DCT perceptual hash
    ADD COLUMN latitude          double precision,       -- EXIF GPS, decimal degrees
    ADD COLUMN longitude         double precision,
    ADD COLUMN embedding         vector(768),            -- caption embedding (opt-in)
    ADD COLUMN photo_prepared_at timestamptz,            -- cursor: hash + GPS computed
    ADD COLUMN photo_grouped_at  timestamptz,            -- cursor: dup/album regroup done
    ADD COLUMN captioned_at      timestamptz,            -- cursor: caption attempted
    ADD COLUMN dup_cluster_id    uuid REFERENCES clusters (id) ON DELETE SET NULL,
    ADD COLUMN album_cluster_id  uuid REFERENCES clusters (id) ON DELETE SET NULL,
    ADD COLUMN topic_cluster_id  uuid REFERENCES clusters (id) ON DELETE SET NULL;

CREATE INDEX images_prepare_pending_idx ON images (id) WHERE photo_prepared_at IS NULL;
CREATE INDEX images_caption_pending_idx ON images (id) WHERE captioned_at IS NULL;
CREATE INDEX images_dup_cluster_idx   ON images (dup_cluster_id)   WHERE dup_cluster_id IS NOT NULL;
CREATE INDEX images_album_cluster_idx ON images (album_cluster_id) WHERE album_cluster_id IS NOT NULL;
CREATE INDEX images_topic_cluster_idx ON images (topic_cluster_id) WHERE topic_cluster_id IS NOT NULL;
CREATE INDEX images_embedding_hnsw ON images USING hnsw (embedding vector_cosine_ops);

-- The member a UI should show for the group (sharpest duplicate, first shot of
-- an album). Nullable: entity/topic clusters don't need one.
ALTER TABLE clusters ADD COLUMN representative_id uuid;

-- Widen the kind checks. Added NOT VALID (no table scan under an ACCESS
-- EXCLUSIVE lock) and validated in 0012, the same safe pattern as 0006/0007.
ALTER TABLE clusters DROP CONSTRAINT clusters_kind_ck;
ALTER TABLE clusters ADD CONSTRAINT clusters_kind_ck
    CHECK (kind IN ('entity', 'topic', 'photo_dup', 'album', 'photo_topic')) NOT VALID;

ALTER TABLE cluster_members DROP CONSTRAINT cluster_members_kind_ck;
ALTER TABLE cluster_members ADD CONSTRAINT cluster_members_kind_ck
    CHECK (member_kind IN ('entity', 'unit', 'image')) NOT VALID;
