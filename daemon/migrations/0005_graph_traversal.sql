-- Bounded graph traversal.
--
-- 0001's entity_neighborhood() guarded against cycles with a per-row `visited`
-- array, which means the recursive CTE enumerates PATHS rather than nodes and
-- only collapses at the final DISTINCT ON. Measured at 1,184,926 relationship
-- rows (docs/TECHNICAL-WRITEUP.md §9.1): a depth-2 walk from a high-degree hub
-- could not complete at all — it exhausted a 2 GB temp_file_limit — while the
-- same neighbourhood computed as a node-set BFS finishes in ~10 s.
--
-- Two changes here:
--
--   1. Walk NODES, not paths. Each node is expanded at most once, so the work
--      is proportional to the answer rather than to the number of routes to it.
--   2. Bound the walk with a node budget. A hub's 2-hop neighbourhood is
--      genuinely ~94% of the graph (1,117,798 of 1,184,926 edges measured), so
--      no rewrite makes it small — the caller has to be able to ask for a
--      partial answer and be told that is what it got. Capping only the
--      returned rows would not help: the cost is in the walk, before any row
--      is discarded.
--
-- Implemented in plpgsql rather than SQL because a recursive CTE cannot bound
-- its own frontier (window functions are disallowed in the recursive term).
-- The lost inlining is a fair trade for a walk whose row estimate was wrong by
-- four orders of magnitude anyway.

-- Equivalence with the 0001 implementation is proven in CI rather than here:
-- daemon/tests/graph_traversal_integration.rs recreates the old recursive-CTE
-- body as a temporary function and diffs (relationship_id, depth) against this
-- one across every root and depth. Creating and dropping it inside this
-- migration would prove it once, at deploy time, and guard nothing afterwards.

-- The return type gains a column and the signature gains two parameters, so
-- this cannot be a CREATE OR REPLACE.
DROP FUNCTION entity_neighborhood(uuid, integer);

CREATE FUNCTION entity_neighborhood(
    root      uuid,
    max_depth integer DEFAULT 2,
    -- Nodes expanded before the walk stops and reports a partial answer.
    -- Measured at 1.18M rows: 5000 costs ~1034 ms on a 53k-degree hub, 1000
    -- costs ~387 ms, and an ordinary root is unaffected either way (~96 ms),
    -- because its cost is the edge gather rather than the budget.
    max_nodes integer DEFAULT 1000,
    -- Edges returned. 0 means unbounded, which callers should avoid.
    max_edges integer DEFAULT 0
)
RETURNS TABLE (
    depth            integer,
    relationship_id  uuid,
    source_entity_id uuid,
    target_entity_id uuid,
    relation_type    text,
    confidence       real,
    -- True when the node budget or edge cap cut the answer short. Constant
    -- across the result set; every row carries it so callers need no second
    -- query to find out.
    truncated        boolean
)
LANGUAGE plpgsql STABLE AS $$
DECLARE
    visited_ids  uuid[] := ARRAY[root];
    visited_lvl  integer[] := ARRAY[0];
    frontier     uuid[] := ARRAY[root];
    next_ids     uuid[];
    level        integer := 0;
    was_cut      boolean := false;
    room         integer;
BEGIN
    IF max_depth < 1 THEN
        RETURN;
    END IF;

    -- Level-by-level BFS. Each iteration expands the current frontier and
    -- keeps only nodes not already seen, so every node is expanded once.
    WHILE level < max_depth AND coalesce(array_length(frontier, 1), 0) > 0 LOOP
        room := max_nodes - coalesce(array_length(visited_ids, 1), 0);
        IF room <= 0 THEN
            was_cut := true;
            EXIT;
        END IF;

        -- EXCEPT rather than `<> ALL(visited_ids)`: the anti-join hashes, where
        -- the array form rescans the whole visited set per candidate row and
        -- degrades badly as the budget fills.
        --
        -- LIMIT room + 1 is what keeps a hub affordable. Without it the walk
        -- materializes every candidate before discarding all but `room` of
        -- them — for a 53k-degree hub that is 53k rows collected to keep 5k.
        -- The extra row is how we tell "exactly filled" from "overflowed".
        SELECT coalesce(array_agg(n), ARRAY[]::uuid[]) INTO next_ids
        FROM (
            SELECT n FROM (
                SELECT r.target_entity_id AS n
                FROM relationships r
                WHERE r.status = 'active' AND r.source_entity_id = ANY (frontier)
                UNION
                SELECT r.source_entity_id
                FROM relationships r
                WHERE r.status = 'active' AND r.target_entity_id = ANY (frontier)
                EXCEPT
                SELECT unnest(visited_ids)
            ) c
            LIMIT room + 1
        ) candidates;

        EXIT WHEN coalesce(array_length(next_ids, 1), 0) = 0;

        IF coalesce(array_length(next_ids, 1), 0) > room THEN
            next_ids := next_ids[1:room];
            was_cut := true;
        END IF;

        level := level + 1;
        visited_ids := visited_ids || next_ids;
        visited_lvl := visited_lvl || array_fill(level, ARRAY[array_length(next_ids, 1)]);
        frontier := next_ids;

        EXIT WHEN was_cut;
    END LOOP;

    -- Edges in one pass: everything incident to a node whose level is below
    -- max_depth, each kept at its lowest depth. This reproduces the v1
    -- contract, where an edge's depth is 1 + the minimum level of either
    -- endpoint.
    --
    -- A level-by-level variant was tried so the cap could bound the edge
    -- gather too, but it needs an anti-join against the already-emitted set,
    -- and `= ANY(array)` rescans that array per candidate row: it took an
    -- ordinary root from 97 ms to 780 ms to save ~45% on hubs. The single
    -- sorted pass is the better trade.
    -- count(*) OVER () rides along with the sort the DISTINCT ON already
    -- needs, so "is there more than the cap" costs no extra pass over the
    -- edges. A separate COUNT would gather a hub's edges twice.
    RETURN QUERY
    WITH ranked AS (
        SELECT DISTINCT ON (r.id)
               (t.lvl + 1)::integer AS depth,
               r.id                 AS relationship_id,
               r.source_entity_id,
               r.target_entity_id,
               r.relation_type,
               r.confidence
        FROM unnest(visited_ids, visited_lvl) AS t(id, lvl)
        JOIN relationships r
          ON (r.source_entity_id = t.id OR r.target_entity_id = t.id)
        WHERE r.status = 'active' AND t.lvl < max_depth
        ORDER BY r.id, t.lvl
    ),
    ordered AS (
        -- Closest first, so a truncated answer keeps the most relevant edges.
        SELECT ranked.*,
               row_number() OVER (ORDER BY ranked.depth, ranked.relationship_id) AS rn,
               count(*) OVER () AS total
        FROM ranked
    )
    SELECT o.depth, o.relationship_id, o.source_entity_id, o.target_entity_id,
           o.relation_type, o.confidence,
           (was_cut OR (max_edges > 0 AND o.total > max_edges)) AS truncated
    FROM ordered o
    WHERE max_edges <= 0 OR o.rn <= max_edges
    ORDER BY o.depth, o.relationship_id;
END
$$;
