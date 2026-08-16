# Benchmark runbook — producing the go/no-go numbers

`docs/TECHNICAL-WRITEUP.md` §9 gates two decisions on measurements:

| Gate | Criterion | Produced by |
|---|---|---|
| **Phase 1 "Go"** (latency) | graph queries stay <150 ms at personal scale | `scripts/graph-benchmark.sh` |
| **Phase 1 "Go"** (quality) | ≥70% of sampled units judged usable | `scripts/unit-quality-sample.sh` |
| **Phase 3 trigger** | adopt Neo4j only if traversal p95 >150 ms at >1M relationship rows, *after index tuning* | `scripts/graph-benchmark.sh` |

Both scripts are **tier 2**: you run them deliberately, against a scratch database, and the
numbers they print are the evidence. CI runs a cut-down tier-1 version of the benchmark
(`graph-benchmark` job) purely as a regression guard — a shared runner's p95 is not evidence
about a 150 ms threshold, and the CI scale is far below the 1M-row bar.

---

## 1. Traversal latency — `scripts/graph-benchmark.sh`

### Read this before trusting any number it prints

The measurement is dominated by **graph shape, not row count**. Two properties of
`entity_neighborhood()` (`daemon/migrations/0001_init.sql`) cause this:

1. Its cycle guard is a per-row `visited` array, so the recursive CTE enumerates **paths, not
   nodes**, collapsing only at the final `DISTINCT ON`. A dense neighbourhood produces
   combinatorially many intermediate rows.
2. The recursive join is `source = … OR target = …`, which no single index satisfies — Postgres
   needs a BitmapOr across `relationships_source_idx` and `relationships_target_idx`.

A uniformly random graph therefore traverses cheaply no matter how many rows it has, and would
report a comfortable p95 against the wrong question. The seeder builds a hub-heavy (power-law-ish)
graph via `power(random(), alpha)`, and the report separates **hub** roots from **long-tail**
roots. Read them separately: a blended percentile hides the hub cliff, which is the failure mode
that actually matters.

`GATHER_BENCH_ALPHA` is the knob that sets hub concentration (1.0 = uniform, higher = more
concentrated). It is the single input the result is most sensitive to — if you change it, say so
when you quote the number.

### Running it

> **Destructive.** It truncates `entities` / `relationships` / `atomic_units` and inserts millions
> of rows. It refuses to start without `GATHER_GRAPH_BENCH_ALLOW_DESTRUCTIVE=1`. Point
> `DATABASE_URL` at a **scratch** database — never a real one.

```bash
# scratch database with the schema applied
createdb bench
psql "$SCRATCH_URL" -c 'CREATE EXTENSION IF NOT EXISTS vector; CREATE EXTENSION IF NOT EXISTS pgcrypto;'
for f in daemon/migrations/*.sql; do psql "$SCRATCH_URL" -v ON_ERROR_STOP=1 -f "$f"; done

export DATABASE_URL="$SCRATCH_URL"
export GATHER_GRAPH_BENCH_ALLOW_DESTRUCTIVE=1
scripts/graph-benchmark.sh
```

Defaults target the Phase 3 bar: 50k entities, 1.2M relationships, depths 1–3, 40 roots per tier
× 3 repeats. Full knob list is in the script header.

### Reading the output

- **Seeding line + skew ratio.** The script aborts if it seeded far fewer edges than requested, if
  the graph is empty, or if `max/median` degree is under 10× — any of which would mean the shape
  is wrong and every latency figure below it meaningless. Do not skip past these.
- **The latency table.** `p95_vs_threshold` marks each tier/depth `under` or `OVER` the 150 ms
  line. `timeouts` counts queries killed at `GATHER_BENCH_TIMEOUT_MS`; they are recorded at the
  timeout value rather than dropped, because discarding the slowest queries would bias every
  percentile downward. A non-zero timeout count is itself a result.
- **The EXPLAIN.** Printed for the slowest observed case. This is what makes the Phase 3
  precondition — "after index tuning" — answerable: it shows whether the BitmapOr over the two
  relationship indexes dominates, or whether the per-row path enumeration does. **A bare p95
  cannot tell you whether tuning is even available.** This script deliberately does not attempt
  index changes; that belongs in its own change, informed by this output.

A measured run is warm: the harness runs an untimed warm-up pass first, because without it the
first depth in the loop absorbs all the cold-cache cost and reports a *higher* p95 than deeper
traversals — an artifact of measurement order rather than of traversal cost.

### Read the jit=on / jit=off pair before concluding anything

Every measurement runs twice, under `jit=on` (how a stock daemon behaves) and `jit=off`. This is
not a curiosity — on the first trustworthy full-scale run it was the difference between "every
tier and depth is over the line" and the truth.

The recursive CTE's row estimate is wildly high (191,173 estimated against 16 actual at depth 1).
That inflated cost crosses `jit_above_cost`, so Postgres spends ~225 ms JIT-compiling a query that
executes in ~3 ms. At depth 1 with >1M rows this alone put both tiers over the threshold; with
`jit=off` both land comfortably under. **If the two columns differ by a large constant, you are
looking at compilation overhead, not traversal cost.**

A useful sanity check: traversal cost must scale with degree. If the hub and long-tail tiers
report near-identical times at the same depth, something degree-independent is dominating — JIT
being the usual candidate.

### Invoking the Phase 3 clause

The roadmap says adopt Neo4j only if p95 exceeds 150 ms at >1M rows **after index tuning**. A
single `OVER` reading is not sufficient grounds — see §9.1 of the write-up, where the raw readings
looked like a clear trigger and were not. The honest sequence is:

1. Run at ≥1M rows and confirm which tier/depth combinations are `OVER`.
2. **Compare the jit=on and jit=off columns first.** If jit=off is under the line, the bottleneck
   is compilation and the tuning is a planner setting, not a graph store.
3. Read the EXPLAIN for the slowest *completed* case and identify what actually dominates. Heavy
   `temp read/written` with a large row count means path enumeration, which is a query-shape
   problem in `entity_neighborhood()` — still not a Postgres-vs-Neo4j question.
4. Attempt the indicated tuning in its own change, re-run **with the same `GATHER_BENCH_SEED`**,
   and compare like for like.
5. Only if p95 is still over the line, with tuning applied and the bottleneck understood, does the
   Neo4j clause apply — and `entity_neighborhood()` is the single seam it would land on.

---

## 2. Unit quality — `scripts/unit-quality-sample.sh`

"Judged usable" is a human call. This script does **not** automate that judgment, and no output of
it should be quoted as if it had. It handles sampling rigour and arithmetic; you supply the
judgment.

Read-only against the database.

```bash
export DATABASE_URL=...            # safe: this script only reads
scripts/unit-quality-sample.sh sample > sheet.tsv
# open sheet.tsv, mark every row's `usable` column y or n
scripts/unit-quality-sample.sh score sheet.tsv
```

- The draw is reproducible: `GATHER_SAMPLE_SEED` (default `0.42`) is echoed into the sheet, so a
  disputed result can be re-reviewed on the same sample rather than re-rolled onto a new one.
- Each row carries the statement, kind, confidence, extraction method, subject entity, source
  artifact and the provenance quote — enough to judge without going back to the database.
- `score` **refuses a partially marked sheet** rather than treating unmarked rows as failures,
  which would silently understate the result. It exits non-zero when the sample is below the bar.

### The flag_* columns are advisory only

`flag_no_provenance`, `flag_no_subject`, `flag_dup_statement`, `flag_short`, `flag_low_conf` detect
**malformedness, not usefulness**. They are useful for spotting extraction bugs and for deciding
where to look first, and they are deliberately excluded from the score. The gate number comes from
the human `usable` column and nothing else. Do not report a flag-derived percentage as the Phase 1
quality figure.

### Sample size

The default is 100 units. At that size a single unit moves the result by a full percentage point,
so a figure landing within a couple of points of 70% should be treated as inconclusive rather than
as a pass or a fail — draw a larger sample with `GATHER_SAMPLE_SIZE` before deciding.
