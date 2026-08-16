#!/usr/bin/env bash
# graph-benchmark.sh — produces the traversal-latency numbers the roadmap's
# go/no-go gates are written against (docs/TECHNICAL-WRITEUP.md §9):
#
#   Phase 1 "Go":     graph queries stay <150 ms at personal scale
#   Phase 3 trigger:  adopt Neo4j only if recursive-CTE traversal p95 exceeds
#                     150 ms at >1M relationship rows, after index tuning
#
# Both numbers come from entity_neighborhood() (0001_init.sql), the single seam
# where a graph-store swap would land.
#
# WHY GRAPH SHAPE, NOT ROW COUNT, IS THE VARIABLE THAT MATTERS
# ------------------------------------------------------------
# Two properties of entity_neighborhood() decide what this measures:
#
#   1. Its cycle guard is a per-row `visited` array, so the recursive CTE
#      enumerates PATHS, not nodes, collapsing only at the final DISTINCT ON.
#      A dense neighbourhood produces combinatorially many intermediate rows.
#   2. The recursive join is `source = ... OR target = ...`, which no single
#      index satisfies — Postgres needs a BitmapOr across
#      relationships_source_idx and relationships_target_idx.
#
# So a uniformly random graph — however many rows — traverses cheaply and would
# report a flatteringly low p95 against the wrong question. This seeder builds a
# hub-heavy (power-law-ish) graph instead, and reports hub roots separately from
# long-tail roots, because a blended percentile hides the hub cliff that is the
# actual failure mode.
#
# DESTRUCTIVE: truncates entities/relationships/atomic_units in $DATABASE_URL
# and bulk-inserts millions of synthetic rows. Never run this against a real
# database — point it at a scratch one.
#
# Required environment:
#   DATABASE_URL                              scratch Postgres, NOT a real one
#   GATHER_GRAPH_BENCH_ALLOW_DESTRUCTIVE=1    required safety gate
# Optional environment:
#   GATHER_BENCH_ENTITIES       entity count      (default 50000)
#   GATHER_BENCH_RELATIONSHIPS  edge count        (default 1200000, > the 1M bar)
#   GATHER_BENCH_ALPHA          hub skew exponent (default 3.0; 1.0 = uniform,
#                               higher = more concentrated hubs)
#   GATHER_BENCH_SEED           PRNG seed, -1..1 (default 0.42) — fixes both the
#                               graph topology and the long-tail root sample, so
#                               a before/after tuning comparison is like-for-like
#   GATHER_BENCH_ROOTS          roots sampled per tier   (default 40)
#   GATHER_BENCH_REPEATS        timed runs per root      (default 3)
#   GATHER_BENCH_DEPTHS         depths to measure (default "1 2 3")
#   GATHER_BENCH_TIMEOUT_MS     per-query statement_timeout (default 15000)
#   GATHER_BENCH_TEMP_LIMIT     per-query temp spill cap    (default 2GB)
#   GATHER_BENCH_THRESHOLD_MS   pass/fail line    (default 150)
#   GATHER_BENCH_ENFORCE        1 = exit non-zero if p95 exceeds the line
#                               (default 0: report, do not gate)
set -euo pipefail

if [ "${GATHER_GRAPH_BENCH_ALLOW_DESTRUCTIVE:-}" != "1" ]; then
  echo "graph-benchmark: refusing to run without GATHER_GRAPH_BENCH_ALLOW_DESTRUCTIVE=1" >&2
  echo "this script TRUNCATEs graph tables in \$DATABASE_URL and inserts millions of rows." >&2
  exit 1
fi

for tool in psql; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "graph-benchmark: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

if [ -z "${DATABASE_URL:-}" ]; then
  echo "graph-benchmark: DATABASE_URL must be set" >&2
  exit 1
fi

ENTITIES="${GATHER_BENCH_ENTITIES:-50000}"
RELATIONSHIPS="${GATHER_BENCH_RELATIONSHIPS:-1200000}"
ALPHA="${GATHER_BENCH_ALPHA:-3.0}"
SEED="${GATHER_BENCH_SEED:-0.42}"
ROOTS="${GATHER_BENCH_ROOTS:-40}"
REPEATS="${GATHER_BENCH_REPEATS:-3}"
DEPTHS="${GATHER_BENCH_DEPTHS:-1 2 3}"
TIMEOUT_MS="${GATHER_BENCH_TIMEOUT_MS:-15000}"
TEMP_FILE_LIMIT="${GATHER_BENCH_TEMP_LIMIT:-2GB}"
THRESHOLD_MS="${GATHER_BENCH_THRESHOLD_MS:-150}"
ENFORCE="${GATHER_BENCH_ENFORCE:-0}"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

log() { printf '[graph-benchmark] %s\n' "$1"; }
psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -qtA "$@"; }

# ------------------------------------------------------------------ seed ----

log "seeding: ${ENTITIES} entities, ${RELATIONSHIPS} relationships, alpha=${ALPHA}, seed=${SEED}"
log "(dropping traversal indexes for the bulk load; recreated before measuring)"

psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -q <<SQL
-- Left over from a previous run, so an unconditional CREATE would fail here --
-- after the truncate and index drops below have already happened. Dropping
-- them first is what makes the tune-and-rerun workflow in the runbook work.
DROP TABLE IF EXISTS bench_entity_idx, bench_roots, bench_results;

TRUNCATE relationships, atomic_units, entities CASCADE;

-- Determinism matters more here than it looks: the runbook's tune-and-rerun
-- comparison is only meaningful if both runs measure the SAME topology. With
-- unseeded random(), a lower p95 after an index change could just as easily be
-- a differently-shaped graph. setseed fixes both the edge draw and the
-- long-tail root sample (see the second setseed before bench_roots).
\o /dev/null
SELECT setseed(${SEED});
\o

-- Indexes are rebuilt after the load: maintaining them per-row would dominate
-- a million-row insert and tell us nothing about traversal.
DROP INDEX IF EXISTS relationships_source_idx;
DROP INDEX IF EXISTS relationships_target_idx;
DROP INDEX IF EXISTS relationships_edge_uq;

INSERT INTO entities (name, kind)
SELECT 'bench-entity-' || g, 'other'
FROM generate_series(1, ${ENTITIES}) g;

-- Dense idx -> uuid mapping so the skew transform below can address entities
-- by integer without an ORDER BY over the whole table per edge.
CREATE UNLOGGED TABLE bench_entity_idx AS
SELECT row_number() OVER (ORDER BY name) AS idx, id
FROM entities;
CREATE UNIQUE INDEX ON bench_entity_idx (idx);
ANALYZE bench_entity_idx;

-- power(random(), alpha) biases toward 0, so low indices accumulate most
-- edges: a few hubs, a long tail. alpha = 1.0 degenerates to uniform.
--
-- The random indices are materialized first and then equi-joined. Selecting
-- them inside an uncorrelated LATERAL instead, matching idx against a volatile
-- expression, silently yields zero rows: the expression in the index condition
-- is not evaluated per outer row the way it reads. The join form is also far
-- cheaper -- one hash join rather than a million index probes.
-- DISTINCT because the unique index is dropped during the load, so
-- ON CONFLICT has nothing to fire against: duplicate pairs would instead
-- surface as a failure when relationships_edge_uq is recreated below. The
-- relation_type is spread over a few values, which both widens the unique key
-- (fewer collisions to discard, so the achieved count lands near the
-- requested one) and matches real graphs, which are not single-typed.
INSERT INTO relationships (source_entity_id, target_entity_id, relation_type, status)
SELECT DISTINCT se.id, te.id, p.rel, 'active'::unit_status
FROM (
  SELECT 1 + floor(power(random(), ${ALPHA}) * ${ENTITIES})::bigint AS s_idx,
         1 + floor(power(random(), ${ALPHA}) * ${ENTITIES})::bigint AS t_idx,
         'bench_rel_' || (g % 8) AS rel
  FROM generate_series(1, ${RELATIONSHIPS}) g
) p
JOIN bench_entity_idx se ON se.idx = p.s_idx
JOIN bench_entity_idx te ON te.idx = p.t_idx
-- relationships_no_self_loop rejects source = target.
WHERE se.id <> te.id;

-- Recreated exactly as 0001_init defines them, so we measure the shipped
-- schema rather than a benchmark-only one.
CREATE INDEX relationships_source_idx ON relationships (source_entity_id, relation_type, status);
CREATE INDEX relationships_target_idx ON relationships (target_entity_id, relation_type, status);
CREATE UNIQUE INDEX relationships_edge_uq
    ON relationships (source_entity_id, target_entity_id, relation_type,
                      coalesce(atomic_unit_id, '00000000-0000-0000-0000-000000000000'::uuid));
ANALYZE relationships;
ANALYZE entities;
SQL

edge_count="$(psql_q -c "SELECT count(*) FROM relationships;")"
log "seeded ${edge_count} relationship rows"

# --- skew sanity check --------------------------------------------------
# If the transform above silently produced a near-uniform graph, every number
# below would be measuring the wrong shape. Fail loudly rather than report it.
read -r max_deg p90_deg median_deg <<EOF
$(psql_q -F' ' -c "
  WITH deg AS (
    SELECT e.id, count(r.id) AS d
    FROM entities e
    LEFT JOIN relationships r
      ON r.source_entity_id = e.id OR r.target_entity_id = e.id
    GROUP BY e.id
  )
  SELECT max(d),
         percentile_disc(0.9) WITHIN GROUP (ORDER BY d),
         percentile_disc(0.5) WITHIN GROUP (ORDER BY d)
  FROM deg;")
EOF
log "degree distribution: max=${max_deg} p90=${p90_deg} median=${median_deg}"

# Checked in this order deliberately: an empty or degenerate graph must fail
# here, not sail through into a benchmark that reports a confident 0 ms.
if [ "${edge_count:-0}" -lt "$((RELATIONSHIPS / 2))" ]; then
  log "FAILED: seeded only ${edge_count} of ${RELATIONSHIPS} requested relationships"
  exit 1
fi
if [ "${max_deg:-0}" -lt 1 ]; then
  log "FAILED: no entity has any edges — the graph is empty"
  exit 1
fi
if [ "${median_deg:-0}" -lt 1 ]; then
  log "note: median degree is 0 (long tail is isolated); skew ratio not computable"
elif [ "$((max_deg / median_deg))" -lt 10 ]; then
  log "FAILED: graph is too uniform (max/median = $((max_deg / median_deg))x, expected >=10x)"
  log "the skew transform is not producing hubs; every latency number below would be meaningless"
  exit 1
else
  log "skew ratio max/median = $((max_deg / median_deg))x"
fi

# --------------------------------------------------------------- measure ----
# Timed server-side: clock_timestamp() deltas around the call, so the figure
# excludes client, network and psql overhead. This is the seam Phase 3 names.

log "measuring entity_neighborhood(): ${ROOTS} roots/tier x ${REPEATS} repeats, depths [${DEPTHS}], timeout ${TIMEOUT_MS}ms"

psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -q <<SQL
CREATE UNLOGGED TABLE bench_results (
  id bigserial primary key, tier text, depth int, root uuid, repeat int,
  ms double precision,
  -- Pre-seeded as 'not-completed'; a successful measurement overwrites it.
  -- Anything still marked that way at the end could not finish, which is a
  -- result rather than an absence of one.
  outcome text NOT NULL DEFAULT 'not-completed'
);

-- Same seed as the graph build, so the long-tail sample is stable across runs
-- too -- otherwise the roots would move even when the topology did not.
\o /dev/null
SELECT setseed(${SEED});
\o

-- Hub tier: the highest-degree entities, where path enumeration is worst.
-- Long-tail tier: a random sample of the rest, the common case.
CREATE UNLOGGED TABLE bench_roots AS
WITH deg AS (
  SELECT e.id, count(r.id) AS d
  FROM entities e
  LEFT JOIN relationships r
    ON r.source_entity_id = e.id OR r.target_entity_id = e.id
  GROUP BY e.id
),
hubs AS (
  SELECT id, 'hub'::text AS tier FROM deg ORDER BY d DESC LIMIT ${ROOTS}
),
tail AS (
  SELECT id, 'long-tail'::text AS tier FROM deg
  WHERE d <= (SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY d) FROM deg)
  ORDER BY random() LIMIT ${ROOTS}
)
SELECT * FROM hubs UNION ALL SELECT * FROM tail;

INSERT INTO bench_results (tier, depth, root, repeat, ms)
SELECT b.tier, d, b.id, rep, ${TIMEOUT_MS}
FROM bench_roots b,
     unnest(string_to_array('${DEPTHS}', ' ')::int[]) d,
     generate_series(1, ${REPEATS}) rep;
SQL

# Each measurement is issued as its OWN top-level statement, which is the only
# way statement_timeout actually arms: Postgres starts the timer per client
# statement, so a SET inside a DO block never applies to the queries nested in
# it. Verified directly -- pg_sleep(2) runs to completion under a 500 ms limit
# inside DO, and is cancelled correctly at top level. The earlier DO-based loop
# therefore had no working timeout at all, which is why a runaway hub traversal
# was able to exhaust the disk instead of being cancelled.
#
# statement_timestamp() is the start of this statement and clock_timestamp() is
# evaluated after the WHERE has forced the traversal to run, so ms stays a
# server-side measurement with no client or network time in it.
measure_sql="$workdir/measure.sql"
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -qtA -c "
COPY (
  SELECT format(
    'UPDATE bench_results SET ms = extract(epoch FROM clock_timestamp() - statement_timestamp()) * 1000, outcome = ''ok'' WHERE id = %s AND (SELECT count(*) FROM entity_neighborhood(%L::uuid, %s)) >= 0;',
    id, root, depth)
  FROM bench_results ORDER BY id
) TO STDOUT;" > "$measure_sql"

# Warm-up: same statements, results discarded by resetting afterwards. Without
# it the first depth absorbs all the cold-cache cost and reports a HIGHER p95
# than deeper traversals -- an artifact of measurement order.
log "warm-up pass"
psql "$DATABASE_URL" -q \
  -c "SET statement_timeout = ${TIMEOUT_MS};" \
  -c "SET temp_file_limit = '${TEMP_FILE_LIMIT}';" \
  -f "$measure_sql" >/dev/null 2>&1 || true
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -q \
  -c "UPDATE bench_results SET outcome = 'not-completed', ms = ${TIMEOUT_MS};"

log "measured pass"
# ON_ERROR_STOP stays OFF here on purpose: a cancelled or resource-limited
# traversal must not abort the remaining measurements. Its row simply keeps the
# pre-seeded 'not-completed' outcome.
psql "$DATABASE_URL" -q \
  -c "SET statement_timeout = ${TIMEOUT_MS};" \
  -c "SET temp_file_limit = '${TEMP_FILE_LIMIT}';" \
  -f "$measure_sql" 2>"$workdir/measure.err" >/dev/null || true
if [ -s "$workdir/measure.err" ]; then
  log "$(grep -c 'ERROR' "$workdir/measure.err" || true) traversals did not complete (timeout or resource limit)"
fi

echo
echo "=== entity_neighborhood() latency (ms) — threshold ${THRESHOLD_MS} ms ==="
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -c "
  SELECT tier, depth,
         count(*)                                                    AS samples,
         count(*) FILTER (WHERE outcome = 'ok')                       AS completed,
         count(*) FILTER (WHERE outcome <> 'ok')                      AS not_completed,
         round(percentile_cont(0.5)  WITHIN GROUP (ORDER BY ms)::numeric, 1) AS p50,
         round(percentile_cont(0.95) WITHIN GROUP (ORDER BY ms)::numeric, 1) AS p95,
         round(percentile_cont(0.99) WITHIN GROUP (ORDER BY ms)::numeric, 1) AS p99,
         CASE WHEN percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) <= ${THRESHOLD_MS}
              THEN 'under' ELSE 'OVER' END AS p95_vs_threshold
  FROM bench_results GROUP BY tier, depth ORDER BY tier, depth;"

# ------------------------------------------------------------- diagnose ----
# The roadmap gates Neo4j on p95 exceeding the line *after index tuning*, so a
# bare number cannot answer it. This shows whether the BitmapOr or the per-row
# path enumeration dominates — i.e. whether tuning is even available.

slowest="$(psql_q -c "SELECT root || ' ' || depth FROM bench_results ORDER BY ms DESC LIMIT 1;")"
slow_root="${slowest% *}"
slow_depth="${slowest#* }"
echo
echo "=== EXPLAIN (ANALYZE, BUFFERS) for the slowest observed case (depth ${slow_depth}) ==="
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -c "SET statement_timeout = ${TIMEOUT_MS};" \
  -c "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM entity_neighborhood('${slow_root}'::uuid, ${slow_depth});" \
  || log "EXPLAIN did not complete within the timeout (itself a finding)"

# ----------------------------------------------------------------- gate ----

worst_p95="$(psql_q -c "
  SELECT round(max(p95)::numeric, 1) FROM (
    SELECT percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) AS p95
    FROM bench_results GROUP BY tier, depth) s;")"
echo
log "worst p95 across all tiers/depths: ${worst_p95} ms (threshold ${THRESHOLD_MS} ms)"

if [ "$ENFORCE" = "1" ]; then
  over="$(psql_q -c "
    SELECT count(*) FROM (
      SELECT percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) AS p95
      FROM bench_results GROUP BY tier, depth) s
    WHERE s.p95 > ${THRESHOLD_MS};")"
  if [ "$over" -gt 0 ]; then
    log "FAILED: ${over} tier/depth combination(s) exceed the ${THRESHOLD_MS} ms p95 line"
    exit 1
  fi
  log "OK — every tier/depth p95 is within the ${THRESHOLD_MS} ms line"
else
  log "reporting only (GATHER_BENCH_ENFORCE=1 to gate on the threshold)"
fi
