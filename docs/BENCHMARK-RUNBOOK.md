# Benchmark runbook — producing the go/no-go numbers

`docs/TECHNICAL-WRITEUP.md` §9 gates two decisions on measurements:

| Gate | Criterion | Produced by |
|---|---|---|
| **Phase 1 "Go"** (latency) | graph queries stay <150 ms at personal scale | `scripts/graph-benchmark.sh` |
| **Phase 1 "Go"** (quality, automated) | rule extractor holds ≥70% precision on the golden corpus | `cargo test --test extraction_quality` |
| **Phase 1 "Go"** (quality, real data) | ≥70% of real units you gave a verdict on are kept | `gather_realdata_precision` (continuous, from the feedback loop); `scripts/unit-quality-sample.sh` for a one-off audit |
| **Phase 3 trigger** | adopt Neo4j only if traversal p95 >150 ms at >1M relationship rows, *after index tuning* | `scripts/graph-benchmark.sh` |

The two benchmark scripts are **tier 2**: you run them deliberately, against a scratch database,
and the numbers they print are the evidence. CI runs a cut-down tier-1 version of the benchmark
(`graph-benchmark` job) purely as a regression guard — a shared runner's p95 is not evidence
about a 150 ms threshold, and the CI scale is far below the 1M-row bar.

The quality gate now has **two complementary halves**. The automated golden-corpus eval
(`daemon/tests/extraction_quality.rs`, §3 below) rides the ordinary `test` job on every push and
turns rule-extractor precision into a CI-enforced number so quality cannot silently regress. The
real-data half no longer needs a batch-labelling session: the autonomous pipeline's feedback loop
turns the rare corrections you make anyway (reject, restore, confirm, edit, review-tray answers)
into `gather_realdata_precision`, a live measurement on *your* ingested data (see
`docs/AUTONOMOUS-PIPELINE.md`). The human sampler (§2) remains for a deliberate one-off audit, for
example before a release or when you have given too few verdicts for the gauge to mean much.

---

## 1. Traversal latency — `scripts/graph-benchmark.sh`

### Read this before trusting any number it prints

The measurement is dominated by **graph shape, not row count**.

`entity_neighborhood()` (`daemon/migrations/0005_graph_traversal.sql`) walks nodes, expanding each
at most once, and is bounded by a node budget and an edge cap. Its cost therefore tracks the size
of the answer: a hub's 2-hop neighbourhood is genuinely ~94% of the graph, and no traversal makes
that small.

(Until 0005 the walk enumerated **paths** rather than nodes — its cycle guard was a per-row
`visited` array — so a dense neighbourhood produced combinatorially many intermediate rows and a
hub could exhaust a 2 GB temp file rather than merely being slow. If you are reading numbers from
before that migration, this is why they contain timeouts.)

One property that has not changed: the join is `source = … OR target = …`, which no single index
satisfies — Postgres needs a BitmapOr across `relationships_source_idx` and
`relationships_target_idx`.

A uniformly random graph therefore traverses cheaply no matter how many rows it has, and would
report a comfortable p95 against the wrong question. The seeder builds a hub-heavy (power-law-ish)
graph via `power(random(), alpha)`, and the report separates **hub** roots from **long-tail**
roots. Read them separately: a blended percentile hides the hub cliff, which is the failure mode
that actually matters.

`GATHER_BENCH_MAX_NODES` and `GATHER_BENCH_MAX_EDGES` must be held fixed across a before/after
comparison exactly as `GATHER_BENCH_SEED` is: they change how much of the walk is performed, so
runs at different budgets measure different work and are not comparable.

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

Since the 0005 traversal rewrite the two columns are close (74.5 vs 73.4 ms at hub depth 1): the
old walk's wildly wrong row estimate was what tripped `jit_above_cost`, and the plpgsql body
estimates sanely. A large gap reappearing is a signal that something has regressed the plan.

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
   `temp read/written` with a large row count means the walk is gathering far more than it
   returns — a query-shape or budgeting problem in `entity_neighborhood()`, still not a
   Postgres-vs-Neo4j question. Try lowering `GATHER_BENCH_MAX_NODES` first: hub cost scales with it
   (1034 ms at 5000, 387 ms at 1000, 265 ms at 300 — measured at 1.18M rows) while ordinary roots
   stay flat, because their cost is the edge gather rather than the budget.
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

---

## 3. Extraction quality — `cargo test --test extraction_quality`

The automated half of the quality gate. It scores the always-on, offline **rule-based** extractor
(`daemon/src/extract/rules.rs`) against a hand-labelled golden corpus
(`daemon/tests/fixtures/extraction_golden.json`) and fails the build if precision drops below the
threshold. Unlike the human sampler it needs no database and no Ollama — it calls the pure
`extract_units()` function — so it runs in the ordinary `test` job on every push.

```bash
cd daemon
cargo test --test extraction_quality -- --nocapture   # --nocapture prints the summary below
```

### What it measures

Each corpus case pairs an input chunk with the atomic units a human judged **usable** for that
text. The eval runs `extract_units()` over every input and matches produced units against the
labels on `(kind, normalized statement)` — normalization collapses case and whitespace and strips
trailing `.!?`, so a golden statement only has to agree on wording, not formatting.

- **precision = usable_produced / produced** — the automatable analogue of the write-up's
  "≥70% of sampled units judged usable" gate. This is the number the test asserts on
  (`threshold_precision` in the JSON, currently `0.70`).
- **recall = matched_expected / expected** — informational only. The rules are
  high-precision/low-recall by design (Ollama supplements them when enabled), so recall is *not*
  gated; it is printed to make coverage regressions visible.

The corpus deliberately includes noise cases that must produce nothing, adversarial over-matches
that produce an **unusable** unit (these are what cost precision), and facts the rules cannot catch
(these cost recall). A summary prints per run:

```
── extraction quality (rule-based, offline) ──
cases:       13
produced:    12  (usable 10, spurious 2)
labelled:    12  (found 10)
precision:   83.3%  (gate ≥ 70%)
recall:      83.3%  (informational — rules are high-precision/low-recall)
```

### Baseline

At the corpus's current 13 cases the rule extractor scores **83.3% precision, 83.3% recall**. The
two precision misses are the intentional adversarial-vacuous cases ("I have no idea what to do
next", "We decided to think about it later"); the two recall misses are the intentional
implicit/third-person facts the deterministic patterns don't reach. That leaves ~13 points of
head-room above the 70% gate, so an ordinary refactor won't trip it, but a change that starts
emitting vacuous units will.

### Extending the corpus

Add a case to `extraction_golden.json` whenever you find a real input the rules handle notably well
or badly. Write each `expected` statement as the **full sentence the extractor renders** (tidied,
trailing punctuation stripped) with the `kind` the rule assigns (`decision`, `fact`, `preference`,
`event`, `claim`). To pin a *precision* case, add an adversarial input with `expected: []`; to
track a known *recall* gap, add the input with its usable unit in `expected` and let it show up as a
miss until a pattern (or Ollama) covers it. Keep `threshold_precision` at 0.70 unless the release
gate itself moves.

### How it relates to §2

This eval and the real-data measurement answer different questions and neither replaces the
other. The eval proves the *rules* haven't regressed on a fixed, reviewable set — cheap,
deterministic, CI-enforced. `gather_realdata_precision` (or a sampler pass) proves *real ingested
data* clears the usable bar — the judgment the release actually turns on. Ship both: green CI
here, plus a real-data precision at or above 70% on a meaningful number of verdicts.

---

## 4. Autonomous-pipeline evals — `cargo test`

The organising machinery is guarded the same way as extraction: offline, deterministic tests on
the ordinary `test` job, so a change that weakens the pipeline fails CI.

| Suite | Guards |
|---|---|
| `tests/decision_policy.rs` | The conservative merge gate never auto-merges without two agreeing signals or a near-certain one; admission defaults never drop data |
| `tests/clustering.rs` | Mutual-kNN + union-find groups real duplicate names and keeps distinct ones apart; the chaining guard holds |
| `tests/tuning.rs` | The threshold tuner converges to a known quality boundary, never leaves its bounds, never moves on thin evidence, never oscillates, and never loosens on reject-only feedback; the tray ranks boundary hubs first |
| `tests/photo_pipeline.rs` | Re-encoded and resized copies of a scene hash within the duplicate distance, different scenes hash far apart, a mixed library groups exactly by scene, and albums split on time gaps and travel |

The matching integration suites (`cluster_integration`, `feedback_integration`,
`tune_integration`, `photo_integration`, `grpc_integration`) run the same paths end to end
against pgvector; the photo suite uses a mock loopback Ollama, so no model is needed.
