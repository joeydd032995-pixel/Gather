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
   unit keeps its row, a merged entity keeps its record with `merged_into_entity_id` and can be split
   back out exactly, and every decision leaves an audit row.
3. **Only the thin, genuinely ambiguous middle is parked**, in an *optional* review tray that
   never blocks anything. Parked items are still live in the brain.
4. **Your occasional correction is the training signal.** It's recorded, it's reversible, and
   it feeds a real-data precision metric (and, next, automatic threshold tuning).

5. **Automation is fail-closed.** An automatic action needs positive evidence for every
   condition its rule requires. When evidence is missing, ambiguous or contradicts itself
   (a chain of similarities, an unknown time, a generic name, a type mismatch, a copied
   source), the item becomes review work, not an autonomous state change. Every automatic
   conclusion, and every one held back, carries an **inference certificate** saying which
   rule allowed it or which check stopped it; see [SEMANTIC-SAFETY.md](SEMANTIC-SAFETY.md).

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

### What is worth keeping at all (`daemon/src/extract/worth.rs`)

The extractors match sentence *shapes* ("X is 75", "I use Y", "X means Y"), and a shape is not
meaning. Before a unit gets a confidence it has to be worth storing. A deterministic, offline
gate drops what is clearly not a statement about the world:

- **Arithmetic and equations** (`2+1 is 3 is equal to = 3`, `x = 4`), **code and markup**,
  **table rows and dumps of figures**, and **questions**.
- **Filler** with too few words of its own (`I am happy`, `It is what it is`). A stated figure
  counts as content, so `The budget is $40,000` stays.
- **Vacuous sentences** whose object is a fragment (`I have no idea what to do next`,
  `We decided to think about it later`).
- **Code and data inside a chunk**: fenced code, code-like lines, table rows and rows of bare
  figures are blanked out (byte for byte, so offsets still point into the file) and the prose
  around them is read as usual. A chunk with no prose left is stamped as read without producing
  units. A stated figure is not a "row of figures": `Budget is $40,000` stays.

The same gate decides what becomes a graph **entity**. A name is the name inside the phrase:
`dark mode in every editor` is about `dark mode`, and `Hetzner CX22 for the backup target` is
about `Hetzner CX22`, so one thing is not several nodes. A preposition followed by a capital is
part of the name (`Ruby on Rails`, `Research in Motion`). A statement whose subject can't be a
node (`the project`, `the team`) is still kept, just without a subject entity. A question is
never kept, including when a model rewords it as a statement. Numbers, expressions, pronouns, moods
(`sure`, `tired`) and sentence fragments never become entities, so they can't turn up as
"possible match" items either. The gate keeps what it is unsure about: it never rejects a
statement for being short or plain, only for being arithmetic, code, data, a fragment or filler.

Skipped candidates are counted in `gather_extraction_units_total{status="skipped_low_value"}`.

### What a document is about (`daemon/src/extract/digest.rs`)

Atomic units answer "which sentences have a certain shape?". A **digest** answers "what is this
document *for*?", which is where depth comes from. Once a document's text is read, the extraction
worker builds one (`GET /artifacts/{id}/digest`, shown as **Summary** at the top of a file in the
Library):

- **Key sentences**: every sentence is scored by how central its words are to the document, with
  a lift for decision, plan, deadline and risk language and for the opening of a section. A few
  are picked in reading order, and a sentence that repeats one already chosen is skipped. Every
  key point is a sentence from the file; nothing is invented. Code, tables, equations and
  questions are never candidates.
- **Topics** (recurring phrases such as `backup target`) and the **outline** (headings).
- **With a local AI model on**, the model rewords the key sentences into a short summary,
  takeaways and open questions. It sees only the outline and key sentences (so a small model with
  a short context copes), and anything it writes that shares too few words with them is dropped,
  so it can't add what the document never said. The digest is labelled with how it was written.

A document with too little prose to summarize gets an empty digest, once, rather than being
looked at on every pass.

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

**Optional items are capped.** Low-confidence and "stated or not?" units are optional: they are
kept either way. The tray holds at most 25 of them open at once; past that they are admitted
without being queued, and as you answer some, newer ones can take their place. Decisions that
need a person (merges, contradictions, withdrawals) are never limited. A tray that is already
longer (from an earlier version) is trimmed on the next extraction pass: the most informative
items stay, the rest are closed (their units stay live). A model that gives no
confidence figure is no longer treated as unsure (it counts as 0.7, not 0.5).

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

(`daemon/src/cluster/resolve.rs`, rule `entity.auto_merge` v2 in `daemon/src/safety/identity.rs`.)
Every pass scores candidate pairs over live entities *and* the entities already merged into
them, passes each through the conservative gate above (both signals when they exist), and
then takes components of the **Auto** edges only:

- Each component is **one merge decision**, not one per pair.
- A component merges only if **every pair** in it cleared the Auto bar. A chain (A~B and B~C
  but not A~C) would otherwise fold unrelated entities together through one bridging name, so
  its pairs are parked in the review tray one by one instead, each shown as a possible
  duplicate you can merge or mark as different.
- Pairs of different explicit types are never merged; an untyped entity against a typed one,
  or two with different employer/location/period in their metadata, are held for review.
- Generic names ("Project", "Notes") and hubs (a name that matches several things that don't
  match each other) are never auto-merged; a hub gets one `generic-identifier` tray item.
- A pair you marked as different, or split, is never merged automatically — also not through
  another entity you merged by hand.
- A merge needs at least one source artifact behind its names.
- Components larger than `GATHER_CLUSTER_MAX_COMPONENT` are parked for review instead of
  merged wholesale.
- An **automatic merge is withdrawn** when a later name turns it into a chain, so the result
  does not depend on arrival order. The withdrawal replays the merge journal; the pairs go to
  the tray (it is not a "different" decision and teaches the tuner nothing).
- The survivor is the most specific entity: a typed entity (e.g. `person`) beats an
  extraction-created `other`, then the longest name, then the smallest id.

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
| `tests/digest_integration.rs` | A document gets a digest whose key sentences include its decision and deadline and leave out pleasantries; a model-reworded digest keeps what the text supports and drops what it invented |
| `tests/extraction_quality.rs` | What the extractor *stores* (rules plus the quality gate) has precision ≥ 90% on a labelled golden corpus that includes arithmetic, code, table rows, questions, filler and fragments (currently 100%). Subjects must match, and producing nothing for junk is required |
| `tests/decision_policy.rs` | The merge gate never auto-merges without agreement or a near-certain signal. Admission defaults never drop data |
| `tests/clustering.rs` | Real name similarity groups duplicates and keeps distinct names apart |
| `tests/photo_pipeline.rs` | Re-encoded and resized copies of a scene hash within the duplicate distance, different scenes hash far apart, a mixed library groups exactly by scene, and albums split on time gaps and travel |
| `tests/tuning.rs` | The tuner converges to a known boundary, never leaves its bounds, never moves on thin evidence, never oscillates, and never loosens on reject-only feedback; the tray ranks boundary hubs first |
| `gather-semantic-eval`, `tests/semantic_safety.rs` | The semantic fixture corpus: no merge or photo group without pairwise evidence, no contradiction without full alignment, no automatic conclusion without a certificate and a source, no order dependence, retraction reaches everything that relied on withdrawn evidence, user rejections hold, copies never corroborate |
| `tests/semantic_properties.rs` | Seeded property tests: permutation invariance, idempotence, bridge non-amplification, independent evidence, retraction propagation, user-decision protection, provenance completeness, no invalid closure, threshold boundaries, model-version visibility |

Integration tests (`feedback_integration.rs`, `cluster_integration.rs`, `tune_integration.rs`,
`semantic_safety_integration.rs`, `photo_integration.rs`, which uses a mock loopback Ollama) exercise the feedback
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
the score the item had when you judged it, and may move three thresholds:

| Key | Moves | Hard bounds |
|---|---|---|
| `admit.hold_below` | unit admission bar | 0.30–0.90, never below the drop floor |
| `merge.auto_single` | single-signal auto-merge bar | 0.85–0.99 |
| `merge.agree` | two-signal agreement bar (name *and* embedding) | 0.80–0.95 |

Your labels are biased, and the rules lean into that. People mostly reject wrong things they
happen to notice among auto-accepted items, so measured precision reads *low*, which pushes a
threshold *up*: the cautious direction. So the rules are asymmetric:

- **Raise** by 0.05 when at least `GATHER_TUNE_MIN_SAMPLES` labels sit at or above the threshold
  and their precision is below `GATHER_TUNE_TARGET_PRECISION`.
- **Lower** only to a score you have actually judged, at most 0.05 down, and only when the 95%
  Wilson lower bound of precision at the new bar clears the target *and* the newly admitted
  region has its own evidence. Three kept out of three is not enough.
- The gap between the two rules is hysteresis: the tuner settles instead of oscillating.

Each merge label is kept to the gate that admitted the merge: an undone agreement merge moves
`merge.agree`, never the single-signal bar, and vice versa. Neither the drop floor (the tuner
can never auto-discard data) nor the review floor is ever tuned. A threshold you set in the environment *outside* the
tuner's bounds (e.g. `GATHER_ADMIT_HOLD_BELOW=0.1`) is treated as a deliberate choice and left
alone.

Every move is written to `decision_tuning_audit` with the evidence behind it. When the admission
bar comes down, low-confidence tray entries that now clear it are dismissed automatically, so
the tray drains itself (and a held merge pair that a later pass auto-merges is closed too).
`GET /tuning` shows the current values and history, including the evidence at the new
threshold for every lowering. `POST /tuning/reset` returns to the env defaults *durably*: only
verdicts given after the reset count toward tuning that key again.
`GATHER_TUNE_ENABLED=false` freezes the thresholds.

**Undoing a merge.** Every merge journals exactly what it changed: moved units and edges, the
edges it had to delete, aliases moved or added, and flattened descendants. `POST
/entities/{id}/unmerge` (or **Undo merge** in the Groups view) replays that journal in
reverse and restores the entity as it was. The pair is then dismissed so it is never merged
again. If the merge was automatic or accepted from the tray, the undo is recorded as a
negative merge label at the merge's similarity, tagged with the gate that admitted it, so
wrong auto-merges push `merge.auto_single` or `merge.agree` *up*, and the merge tuner learns in
both directions. Contradictions the scanner found between the two entities' units after the
merge are withdrawn with it. An undo is refused when it can't be exact: when the surviving
entity has since been merged elsewhere, when a later merge into the same survivor is still
live (undo merges newest first), or for merges made before journaling existed.

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
   - **Near-duplicates** (rule `photo.duplicate_group` v2): photos within
     `GATHER_PHOTO_DUP_MAX_DISTANCE` bits of *every* other photo in their group, never two you
     marked "not a duplicate", and never a low-information image that resembles many
     unrelated shots. Photos are ordered canonically before a chain is split, so the split
     doesn't depend on arrival order. Each group carries a certificate. Candidates come from banding the hash into `distance + 1` chunks: two
     hashes that close must match exactly on at least one chunk, so only photos sharing a chunk
     are compared. Linked photos that only form a chain (A near B, B near C, A far from C) are
     split into groups whose members all match each other, so one in-between shot can't tie two
     different scenes together. The
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

## Contradictions and changing facts

The contradiction scanner's findings go through the `contradiction.aligned_conflict` rule
before anything is reported: subject, predicate (including modality), unit, value, scope,
granularity and time must line up. Claims about different periods, a city and the state it
is in, different scopes, or a plan versus a fact are not reported. A later statement of the
current state (asserted at least `GATHER_SAFETY_SUCCESSION_DAYS` after an earlier one)
**supersedes** the older unit, which is kept as history. A conflict whose time can't be
aligned is reported for review, never as a confident contradiction. Details in
[SEMANTIC-SAFETY.md](SEMANTIC-SAFETY.md).

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

- **One-download distribution**: each person installs their own private, offline copy; the pipeline runs per install with no shared state.
