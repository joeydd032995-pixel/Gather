-- Clustering backbone (autonomous pipeline, Phase B).
--
-- One primitive (mutual-kNN + connected components, src/cluster) serves two
-- jobs, distinguished by `clusters.kind`:
--   'entity' — duplicate entities grouped for auto-merge / review.
--   'topic'  — atomic units grouped into themes (the "arrangement" surface).
-- Grouping is non-destructive: a unit's `topic_cluster_id` is a reversible tag,
-- unlike an entity merge, which redirects a node. All local, offline.

CREATE TABLE clusters (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       text NOT NULL,                           -- 'entity' | 'topic'
    label      text NOT NULL DEFAULT '',
    cohesion   real NOT NULL DEFAULT 0,                 -- mean intra-edge similarity
    size       integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT clusters_kind_ck CHECK (kind IN ('entity', 'topic'))
);

CREATE INDEX clusters_kind_idx ON clusters (kind, updated_at DESC);

CREATE TABLE cluster_members (
    cluster_id  uuid NOT NULL REFERENCES clusters (id) ON DELETE CASCADE,
    member_kind text NOT NULL,                          -- 'entity' | 'unit'
    member_id   uuid NOT NULL,
    sim         real NOT NULL DEFAULT 0,                -- representative similarity to the cluster
    PRIMARY KEY (cluster_id, member_id),
    CONSTRAINT cluster_members_kind_ck CHECK (member_kind IN ('entity', 'unit'))
);

-- "Which cluster is this member in" — the reverse lookup the read API uses.
CREATE INDEX cluster_members_member_idx ON cluster_members (member_kind, member_id);

-- Topic tag on units. Nullable and ON DELETE SET NULL: dropping a cluster just
-- un-tags its members, it never deletes a unit.
ALTER TABLE atomic_units
    ADD COLUMN topic_cluster_id uuid REFERENCES clusters (id) ON DELETE SET NULL;

-- Incremental cursor, mirroring contradiction_scanned_at (0003): each active
-- unit is clustered once, and re-clustering clears the stamp.
ALTER TABLE atomic_units ADD COLUMN clustered_at timestamptz;

CREATE INDEX atomic_units_cluster_pending_idx
    ON atomic_units (id)
    WHERE clustered_at IS NULL AND status = 'active';

CREATE INDEX atomic_units_topic_cluster_idx
    ON atomic_units (topic_cluster_id)
    WHERE topic_cluster_id IS NOT NULL;
