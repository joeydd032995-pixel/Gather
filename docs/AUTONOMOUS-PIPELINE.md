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
merge safely. It is ordered by `info_gain`, *most informative first*, so a handful of answers
settle many cases (see [Active learning](#active-learning-and-auto-tuning)).

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
| `tests/photo_pipeline.rs` | Re-encoded and resized copies of a scene hash within the duplicate distance, different scenes hash far apart, a mixed library groups exactly by scene, and albums split on time gaps and travel |
| `tests/tuning.rs` | The tuner converges to a known boundary, never leaves its bounds, never moves on thin evidence, never oscillates, and never loosens on reject-only feedback; the tray ranks boundary hubs first |

Integration tests (`feedback_integration.rs`, `cluster_integration.rs`, `tune_integration.rs`,
`photo_integration.rs`, which uses a mock loopback Ollama) exercise the feedback
endpoints and the clustering worker end to end against pgvector.

## Active learning and auto-tuning

(`daemon/src/tune/`.) A background worker runs every `GATHER_TUNE_INTERVAL_SECS` and does two
things.

### Ranking the tray

Each open tray item gets `info_gain = uncertainty × (1 + ln(1 + degree))`:

- **Uncertainty** is how close the item sits to the *Auto* edge of its hold band: 1.0 right at
  the boundary, 0.0 at the floor. That edge is the one that matters: your answer there decides
  whether the Auto bar can come down, which is what shrinks the tray.
- **Degree** is how much of the graph the answer touches: for a unit, the relationships it
  asserts plus other units about the same subject; for a merge pair, the units about either
  entity; for an oversized component, its size.

### Tuning the thresholds from your verdicts

The tuner reads your **latest** verdict per item (confirm/accept = keep, reject = not), with
the score the item had when you judged it, and may move two thresholds:

| Key | Moves | Hard bounds |
|---|---|---|
| `admit.hold_below` | unit admission bar | 0.30–0.90, never below the drop floor |
| `merge.auto_single` | single-signal auto-merge bar | 0.85–0.99 |

Your labels are biased, and the rules lean into that. People mostly reject wrong things they
happen to notice among auto-accepted items, so measured precision reads *low*, which pushes a
threshold *up*: the cautious direction. So the rules are asymmetric:

- **Raise** by 0.05 when at least `GATHER_TUNE_MIN_SAMPLES` labels sit at or above the threshold
  and their precision is below `GATHER_TUNE_TARGET_PRECISION`.
- **Lower** only to a score you have actually judged, at most 0.05 down, and only when the 95%
  Wilson lower bound of precision at the new bar clears the target *and* the newly admitted
  region has its own evidence. Three kept out of three is not enough.
- The gap between the two rules is hysteresis: the tuner settles instead of oscillating.

Neither the drop floor (the tuner can never auto-discard data) nor the two-signal agreement
bars nor the review floor are ever tuned. A threshold you set in the environment *outside* the
tuner's bounds (e.g. `GATHER_ADMIT_HOLD_BELOW=0.1`) is treated as a deliberate choice and left
alone.

Every move is written to `decision_tuning_audit` with the evidence behind it. When the admission
bar comes down, low-confidence tray entries that now clear it are dismissed automatically, so
the tray drains itself (and a held merge pair that a later pass auto-merges is closed too).
`GET /tuning` shows the current values and history, including the evidence at the new
threshold for every lowering. `POST /tuning/reset` returns to the env defaults *durably*: only
verdicts given after the reset count toward tuning that key again.
`GATHER_TUNE_ENABLED=false` freezes the thresholds.

**Known gap:** there is no entity *unmerge* yet, so an auto-merge can't be undone and can't
produce a negative label. The merge tuner therefore effectively only loosens on accepted tray
merges, which is why its floor (0.85) is high.

## Tuning by hand

- More silent admission and fewer tray items: lower `GATHER_ADMIT_HOLD_BELOW` (or let the tuner
  do it from your tray answers).
- Automatically discard the weakest units: raise `GATHER_ADMIT_DROP_BELOW` above 0. This loses
  data, so use with care.
- Tighter or looser topics: raise or lower `GATHER_CLUSTER_THRESHOLD`, and adjust
  `GATHER_CLUSTER_K`.
- Stricter automation overall: raise `GATHER_TUNE_TARGET_PRECISION`.

## Photos

(`daemon/src/photo/`.) Photos follow the same rule as everything else: **grouped, never
deleted**. A background worker (`GATHER_PHOTO_*`) runs three steps:

1. **Prepare.** Each new photo gets a 64-bit perceptual hash (DCT pHash: an area-filtered
   32×32 luma downscale, keeping the 8×8 lowest frequencies) and its EXIF GPS position. Decoding
   is pure Rust (JPEG, PNG, WebP, TIFF, GIF, BMP) with size and allocation limits. Formats it
   can't decode, such as HEIC, get no hash and are simply left out of duplicate grouping.
2. **Regroup** (whenever new photos have been prepared) over the whole library:
   - **Near-duplicates**: photos within `GATHER_PHOTO_DUP_MAX_DISTANCE` bits, transitively.
     Candidates come from banding the hash into `distance + 1` chunks: two hashes that close
     must match exactly on at least one chunk, so only photos sharing a chunk are compared. The
     sharpest copy (then the earliest) becomes the group's representative.
   - **Albums**: shots sorted by EXIF capture time, cut when the gap exceeds
     `GATHER_PHOTO_ALBUM_GAP_HOURS` or two located shots are more than
     `GATHER_PHOTO_ALBUM_SPLIT_KM` apart. A GPS-less shot in between doesn't hide the jump.
     Labels are dates; there is no reverse geocoding, so nothing leaves the machine.
   - Results are reconciled with the stored clusters. A group keeps its cluster id when most of
     its members already had it, so ids stay stable as photos arrive, and emptied groups are
     removed.
3. **Caption** (opt-in, only with `GATHER_OLLAMA_VISION_MODEL`): a local vision model captions
   the photo, the caption is embedded with the existing 768-dimension model, and the photo
   joins the topic of its nearest visual neighbour (pgvector HNSW) when they are at least
   `GATHER_PHOTO_TOPIC_THRESHOLD` similar. A caption that already exists (older data, an
   imported bundle) is kept and only embedded. If the model is down, captioning pauses and
   resumes next pass; if it rejects one particular image, that photo is skipped with the reason
   in `images.metadata.caption_error`, so it never blocks the rest.

Browse with `GET /clusters?kind=photo_dup|album|photo_topic` and render previews with
`GET /images/{id}/thumbnail`.

## Using it day to day

The desktop app surfaces everything above:

- **Review**: the optional tray, most informative first. Keys: `j`/`k` move, `a` accept, `r`
  reject, `d` dismiss, `e` edit, `u` undo. Rejecting a fact can be undone straight from the toast.
- **Groups**: topics and merged duplicates.
- **Photos**: albums, duplicate groups (the sharpest copy is marked *best*) and visual topics.
- **Tuning**: the thresholds in force, what the tuner learned and why, with per-key reset.

The same operations are available over REST and gRPC (`FeedbackService`, `ClusterService`,
`TuningService`, `PhotoService`).

## What's next

- **Entity unmerge**, so auto-merges become reversible and labelable.
