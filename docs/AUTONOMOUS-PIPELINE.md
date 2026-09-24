# The autonomous pipeline: how Gather organizes itself

Gather is meant to be used by dropping *everything* in (thousands of documents, exports and
photos) and letting the system do the evaluating and organizing. A human should only step
in for the rare, precise correction: "this fact is wrong", "keep this out of my brain".
They should never have to review a stream of suggestions.

This document describes the machinery that makes that work: **confidence bands**, the
**feedback loop**, and **clustering**.

## The core idea: act, then allow undo

A system that *suggests* and waits for a human to *confirm* cannot scale. Pairwise duplicate
suggestions grow with the square of the data, so ten thousand items can produce millions of
questions. Gather inverts this:

1. **High-confidence work is applied automatically.** Units are admitted, duplicates are
   merged and topics are formed without asking.
2. **Every automatic action is reversible and audited.** Nothing is destroyed: a retracted
   unit keeps its row, a merged entity keeps its record with `merged_into_entity_id`, and every
   decision leaves an audit row.
3. **Only the thin, genuinely ambiguous middle is parked**, in an *optional* review tray that
   never blocks anything. Parked items are still live in the brain.
4. **Your occasional correction is the training signal.** It's recorded, it's reversible, and
   it feeds a real-data precision metric (and, next, automatic threshold tuning).

One distinction drives the safety design: **grouping is not merging**. Putting a unit in a
topic cluster is a reversible tag, so it is always safe to do automatically. Merging two
entities redirects a node in the graph, so it has a deliberately conservative gate.

## Confidence bands (`daemon/src/decide/`)

The decision policy is a set of pure functions that map scores to one of three bands:

| Band | Meaning |
|---|---|
| **Auto** | Apply silently |
| **Hold** | Apply, and park in the review tray for optional attention |
| **Drop** | Don't act (for admission: retract; for a merge candidate: don't even surface it) |

### Admitting extracted units

Each new unit's confidence is its extractor's base score (0.6 for the rule extractor), adjusted
by source context: +0.1 when it's about a named entity, +0.1 when you wrote it, −0.1 for
low-quality OCR.

| Confidence | Band (defaults) |
|---|---|
| ≥ `GATHER_ADMIT_HOLD_BELOW` (0.5) | Auto: admitted silently |
| below that, ≥ `GATHER_ADMIT_DROP_BELOW` (0.0) | Hold: admitted *and* parked for review |
| below the drop floor | Drop: retracted, and it creates no graph edges |

The default drop floor of 0.0 means **user data is never discarded automatically**.

### Merging duplicate entities (conservative gate)

Two independent signals may be available for a candidate pair: **text similarity** of the
names (trigram / token / prefix; always available) and **embedding cosine** (only with Ollama).

| Condition | Band |
|---|---|
| Both signals present and both ≥ 0.80 | Auto |
| Any single signal ≥ 0.92 | Auto |
| Best signal ≥ 0.60 but neither rule above met | Hold |
| Otherwise | Drop (not surfaced) |

For example, "postgres" / "postgresql" scores about 0.80 on text alone. Without a corroborating
embedding it is **held, not merged**. With an embedding that agrees, it merges.

## The review tray (`review_queue`)

The tray holds only what the policy could not decide: low-confidence units, ambiguous merge
pairs (keyed by the *pair*, so one entity can have several), and entity components too large to
merge safely. It is ordered by `info_gain`. Today that means most recent first; next it will
mean *most informative first*, so a handful of answers resolve many cases.

You never have to visit it. Items in it are already live.

## The feedback loop (`unit_feedback`)

| Action | Endpoint | Effect |
|---|---|---|
| Reject | `POST /units/{id}/reject` | Unit and the relationships it asserted become `retracted`; negative label |
| Restore | `POST /units/{id}/restore` | Undo a reject (only for `retracted` units); relationships reactivate |
| Confirm | `POST /units/{id}/confirm` | Positive label, no change |
| Edit | `PATCH /units/{id}` | New wording saved with the old wording kept for undo. The unit is marked `manual`, and its embedding, contradiction scan and cluster assignment are reset so it is re-processed |

Every action appends to `unit_feedback`, clears the unit's open tray entry, and is included in
export bundles.

**Real-data precision.** `gather_realdata_precision` is the fraction of units you gave a verdict
on whose *latest* verdict is "keep". A reject followed by a restore counts as a keep. This is
the live quality signal, measured on your real data from actions you take anyway.

## Clustering (`daemon/src/cluster/`)

One primitive serves both entity resolution and topic grouping:

1. **Mutual-kNN graph.** Connect two nodes only if each is among the other's *k* most similar
   (ties at the *k*-th score are kept) and their similarity is at least the threshold. Requiring
   the relation to be mutual is what stops "A~B, B~C" from chaining unrelated A and C together.
2. **Connected components** with union-find. Each component is one group.
3. **Cohesion** (mean intra-group similarity) and a **label** (dominant content word) for each
   group.

It is pure, deterministic and runs fully offline.

### Entity resolution

The clustering worker takes the existing merge-suggestion scorer's candidate pairs and passes
each through the conservative gate above, supplying *both* signals when they exist. It then
takes components of the **Auto** edges only:

- Each component is **one merge decision**, not one per pair.
- Components larger than `GATHER_CLUSTER_MAX_COMPONENT` are parked for review instead of
  merged wholesale (the chaining guard).
- The survivor is the most specific entity: a typed entity (e.g. `person`) beats an
  extraction-created `other`, then the longest name wins.

### Topic grouping

Unclustered active units are claimed in batches and grouped by statement similarity. A bounded
**context window** of already-clustered units is included in the graph, so a unit that arrives
later can join an existing topic instead of being left alone. A unit's topic is a reversible
`topic_cluster_id` tag.

Topic similarity currently uses statement-token overlap (offline, no model). Embedding-based
topics are a planned refinement.

Browse the results with `GET /clusters` and `GET /clusters/{id}`.

## Quality gates

These run offline in CI and fail the build on regressions:

| Eval | Guards |
|---|---|
| `tests/extraction_quality.rs` | Rule extractor precision ≥ 70% on a labelled golden corpus (baseline 83.3%). Subjects must match, and producing nothing fails |
| `tests/decision_policy.rs` | The merge gate never auto-merges without agreement or a near-certain signal. Admission defaults never drop data |
| `tests/clustering.rs` | Real name similarity groups duplicates and keeps distinct names apart |

Integration tests (`feedback_integration.rs`, `cluster_integration.rs`) exercise the feedback
endpoints and the clustering worker end to end against pgvector.

## Tuning

- More silent admission and fewer tray items: lower `GATHER_ADMIT_HOLD_BELOW`.
- Automatically discard the weakest units: raise `GATHER_ADMIT_DROP_BELOW` above 0. This loses
  data, so use with care.
- Tighter or looser topics: raise or lower `GATHER_CLUSTER_THRESHOLD`, and adjust
  `GATHER_CLUSTER_K`.
- More or fewer entity merges: the merge thresholds are fixed conservative defaults for now.
  Feedback-driven tuning (stored in `decision_tuning`) is next on the roadmap.

## What's next

- **Active learning:** order the tray by information gain (closeness to a decision boundary ×
  how central the item is in the graph), so a few answers settle many cases.
- **Auto-tuning:** feedback moves the band thresholds automatically, with no redeploy.
- **Photo pipeline:** perceptual-hash near-duplicate removal, EXIF time/place albums, optional
  local vision embeddings for visual topics.
- **Desktop review tray** and gRPC parity for the feedback and cluster endpoints.
