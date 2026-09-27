# Semantic safety: inference certificates and fail-closed automation

Gather turns local evidence into conclusions without asking: it merges duplicate entities,
groups near-duplicate photos, flags contradictions, and marks old facts as superseded. This
document describes the layer that keeps those automatic conclusions honest, the record each
one leaves behind, and how to extend it safely.

The code lives in `daemon/src/safety/`. The rules are pure functions (no I/O), so they are
deterministic and tested in isolation. `safety::store` persists certificates and
`safety::service` implements the database operations (retraction, user decisions, support).

## The core rule

**Similarity is not identity, and local evidence does not justify global closure.**

- A≈B and B≈C never imply A≈C. A group of entities is merged automatically only when *every
  pair* in it has its own direct, qualifying evidence. A group of photos is formed only when
  every photo is a near copy of every other.
- Two claims can only contradict if they are about the same thing, in the same sense, at the
  same time.
- Two copies of one source are one source.
- A missing fact never authorizes an action. When the information a rule needs is absent or
  ambiguous, the result is *review work* (or nothing), never an automatic state change. The
  automation is **fail-closed**.

## Kinds of evidence

Every input to a conclusion is labelled with how it came to be known. Inferred evidence is
never presented as if a source had asserted it.

| Class | Meaning | Example |
|---|---|---|
| `asserted` | Directly present in a source artifact | a quoted sentence, a photo's pixels |
| `extracted` | Structured information extracted from one source | an atomic unit, an entity name |
| `inferred` | A conclusion produced from one or more inputs | a similarity score, a merge, a contradiction |
| `user_confirmed` | An explicit user confirmation | a manual merge, a merge accepted from the tray |
| `rejected` | An explicit user rejection | "different", "not a duplicate", a split, a removed source |
| `blocked` | A plausible conclusion a safety predicate stopped | a merge across entity types |

## Inference certificates

Every consequential automatic conclusion — and every one the safety layer stopped — gets an
`inference_certificates` row. A certificate answers:

- **What did Gather conclude?** `conclusion_kind`, `conclusion_key` (deterministic), the row
  that materializes it (`conclusion_id`), and the ids it is about (`subject_ids`).
- **Which direct evidence caused it?** `inputs` (each with its evidence class and small
  details such as scores), `input_ids`.
- **Which rule made it permissible?** `rule_id` and `rule_version`.
- **Under what scope and time assumptions?** `scope` (e.g. the contradiction alignment) and
  `temporal` (the time interpretation of each claim).
- **Which model and configuration produced it?** `model_version`, `config` (the thresholds in
  force).
- **Which sources?** `source_artifact_ids` and `source_family_ids`.
- **Why automated — or exactly what stopped it?** `predicates` (each named, pass/fail, with
  detail) and `reason_codes` (the typed codes of failed predicates), plus a plain-language
  `explanation`.
- **Where does it stand now?** `decision` never changes; `outcome` is the decision, or
  `superseded` / `retracted` later, with `superseded_at` / `retracted_at`, a `status_reason`,
  and `caused_by` pointing at the certificate (or user decision) that caused the withdrawal.

| Field | Values |
|---|---|
| `decision` | `auto_applied`, `needs_review`, `blocked`, `user_decision` |
| `outcome` | the decision, or `superseded`, `retracted` |
| `conclusion_kind` | `entity_merge`, `photo_duplicate_group`, `contradiction`, `claim_canonicalization`, `fact_supersession`, `extraction_revision`, `user_decision` |

Certificates are idempotent: re-evaluating the same evidence under the same rule finds the
existing certificate (unique on `rule_id, conclusion_key, evidence_digest` among live rows).
When a conclusion's evidence changes, the new certificate supersedes the old one, so the
history of how its support changed is kept. `evidence_digest` is a SHA-256 over the rule,
decision, inputs, sources, predicates, scope and time — never over ids or timestamps assigned
at persistence — so two evaluations of the same evidence in any order produce the same digest.

A person's explicit decision (a split, "different", "not a duplicate", rejecting a claim,
removing a source) is recorded as a `user_decision` certificate. It is evidence, not an
inference, and later rules treat it as fixed.

### Shape in code

```rust
pub enum InferenceDecision {
    AutoApply(InferenceCertificate),
    NeedsReview(InferenceCertificate),
    Blocked(InferenceCertificate),
}
```

Rules return an `InferenceDecision`, so no code path can act on a conclusion without holding
its certificate. `certificate::decide_from_predicates` is the shared fail-closed policy: all
predicates pass → auto; a failure whose code the rule lists as blocking → blocked; any other
failure → review. The rules deliberately do not share one generic trait: entity resolution
plans over a whole component, photo grouping over a library, contradiction over a pair. What
they share is the certificate, the reason codes and the fail-closed decision.

## Rules

| Rule | Version | Module | Automates | Required predicates |
|---|---|---|---|---|
| `entity.auto_merge` | 2 | `safety::identity` | merging a group of entities | every cross pair has Auto evidence (`pairwise_complete`), `not_chained`, `no_user_rejection`, `has_provenance`, `component_size` |
| `entity.pair_gate` | 1 | `safety::identity` | nothing — records why a pair was held or blocked | `types_compatible`, `types_confirmed`, `sense_compatible`, `context_compatible`, `active_periods_overlap`, `discriminative_name`, `no_user_rejection`, `auto_threshold` |
| `photo.duplicate_group` | 2 | `safety::photo` | grouping near-duplicate photos | `pairwise_complete`, `no_user_rejection`, `bounded_degree`, `discriminative_image` |
| `contradiction.aligned_conflict` | 1 | `safety::contradiction` | reporting a contradiction | `no_user_rejection`, `same_subject`, `same_modality`, `polarity_readable`, `units_normalized`, `same_scope`, `same_granularity`, `same_time` |
| `fact.supersede_by_succession` | 1 | `safety::contradiction` | marking an older state superseded | `same_subject`, `same_state`, `sequenced_in_time`, `newer_is_current` |
| `claim.canonicalize_exact` | 1 | `safety::provenance` | folding a re-assertion into an existing proposition | `same_normalized_statement`, `modality_preserved` |
| `claim.corroboration` | 1 | `safety::provenance` | letting a new source raise confidence | `independent_source`, `not_a_derivation` |
| `extraction.revision` | 1 | `safety::drift` | recording a newer extractor's reading | `versions_agree` |

User decisions use `user.entity_merge`, `user.entity_split`, `user.entity_different`,
`user.photo_not_duplicate`, `user.reject_unit` and `user.retract_source`.

### Entity resolution (`entity.auto_merge` v2)

The clustering worker (`cluster::resolve`) plans over **base records**: live entities *and*
the entities merged into them. For every candidate pair it takes both similarity signals
(names, and embeddings when Ollama is on) through the conservative gate, then:

- **Types and context.** Two explicit, different kinds (a person and a place) are blocked. An
  untyped entity against a typed one is held, not merged: "Apple (organization)" and "apple"
  may be different things. Explicit context in `entities.metadata` — `employer`,
  `organization`, `location`, `role`, or non-overlapping `active_from`/`active_to` — holds the
  pair; a different `sense`/`disambiguator` blocks it.
- **Generic names and hubs.** A generic name ("Project", "Notes", "Meeting") never merges
  automatically. An entity with at least `GATHER_SAFETY_HUB_DEGREE` auto-strength matches
  that mostly don't match each other ("John" ≈ John Smith, John Doe, John Lee) is a hub: its
  links are dropped from automatic resolution and it gets one review item, so one weak name
  can't pull many things together.
- **User decisions.** A dismissed pair (from the tray, or a split) is a cannot-link. Merges a
  person made are fixed must-links: a candidate joining such a group needs its own evidence
  against *every* member, and a rejection against any member blocks it.
- **Components.** Auto edges are joined into components over units (a user-merged group is
  one unit). A component merges only if every cross pair has direct Auto evidence, no pair was
  rejected, at least one source artifact backs its names, and it is not larger than
  `GATHER_CLUSTER_MAX_COMPONENT`. Otherwise every Auto pair in it is parked in the review tray
  (`CHAINED_SIMILARITY`, `PAIRWISE_EVIDENCE_GAP`, `INSUFFICIENT_PROVENANCE`, …).
- **Withdrawal.** Because the plan is over base records, an automatic merge made when two
  names looked alike is **withdrawn** when a later name turns the pair into a chain (A+B
  merged, then C ≈ B arrives, and C ≉ A). The unmerge replays the merge journal exactly; it is
  not a user decision, so the pair is not dismissed and no tuning label is recorded — the
  pairs go to the tray. A merge sitting under a person's later merge can't be unwound
  automatically; it is parked as `withdraw-merge` (`RETRACTION_REQUIRED`).
- **Survivor.** The most specific kind, then the longest name, then the smallest id: a total
  order, so the choice never depends on arrival order.

The result is a function of the records and evidence, not of the order they arrived in: the
same corpus ingested in any order, in any batches, converges to the same partition.

### Photo duplicate groups (`photo.duplicate_group` v2)

Every photo in a group is within `GATHER_PHOTO_DUP_MAX_DISTANCE` bits of every other one.
Photos are ordered canonically (perceptual hash, then content hash, then id) before chains are
split, so the split never depends on the order photos arrived or rows came back. A person's
"not a duplicate" (`POST /images/{id}/not-duplicate`) is a cannot-link no regroup overrides. A
low-information image close to many shots that aren't close to each other (a white wall) is a
hub and is left out (`HUB_DEGREE_EXCEEDED`, `GENERIC_IDENTIFIER`). A chain that was split gets
a review certificate listing the pairs it separated.

### Contradictions (`contradiction.aligned_conflict` v1)

The structural scorer says why two claims *might* clash. The rule then aligns seven
dimensions and stores the alignment with the contradiction (`contradictions.alignment`):

| Dimension | Aligned when |
|---|---|
| `subject` | both claims are about the same entity |
| `predicate` | the same relation, with the same modality (done / planned / possible / …) and readable polarity |
| `unit` | the values are in the same unit after normalization (`$1.2M` = `$1,200,000`; `3 PM Central` = `4 PM Eastern`) |
| `value` | the dimension where they differ |
| `scope` | the same scope or metric (`validation` vs `production` differ) |
| `granularity` | the same level of detail — not a city and the state it is in (`located_in` / `part_of` edges) |
| `time` | the periods the claims describe overlap |

A known reason they are compatible (different period, nested place, different scope,
different modality, the user already resolved it as "both valid" or "dismissed") **blocks**
the contradiction: nothing is reported, and the certificate says why. Missing information
(unknown time, unknown scope, a time zone that can't be normalized, ambiguous negation)
**routes to review**: the contradiction is reported with `certainty = needs_review`. Only a
fully aligned pair is reported as `aligned`.

### Temporal semantics

Time is kept in separate fields:

| Field | Meaning | Where |
|---|---|---|
| `asserted_at` | when the source made the statement | `atomic_units.asserted_at` (message time, file time) |
| `observed_at` | when the reported event happened | `atomic_units.observed_at` (an ISO date in the text) |
| `valid_from` / `valid_to` | the interval the claim says it holds | `atomic_units.valid_from` / `valid_to` |
| `ingested_at` | when Gather processed the source | `atomic_units.created_at` |

`safety::temporal` reads the period each claim describes: explicit validity first, then a
named year ("in 2024", "during 2023"), then the observed time, then tense — a past-tense claim
ends at its assertion, a present-tense claim ("is", "now", "currently") starts at it. Two
present-tense claims asserted within `GATHER_SAFETY_SAME_MOMENT_HOURS` describe the same
moment; at least `GATHER_SAFETY_SUCCESSION_DAYS` apart they describe **successive states**;
in between the relation is unknown.

When two claims about the same state are sequenced and the newer one still holds, the older
one is marked `superseded` (with `superseded_by_unit_id` and `valid_to` set) rather than
flagged as a contradiction. Nothing is erased: the older claim keeps its row and provenance
and reads as history. "I live in Chicago" (2023) then "I live in St. Louis" (2026) is a move;
"Rent was $1,200 in 2024" and "Rent is $1,500 now" are both true.

### Modality and negation

`safety::modality` keeps a completed event, a plan, a possibility, a rejection, a condition
and a consideration distinct ("I moved to Chicago" / "I plan to…" / "I might…" / "I decided
not to…" / "If I move…" / "I considered…"). Canonicalization keys on (content, modality,
polarity), so "not" is never dropped. Only an actual, non-negated claim may assert a graph
edge: a plan or a hypothetical never becomes a present fact. When the markers conflict ("I
might decide not to…") the unit is kept but parked for review (`modality-uncertain`), and two
claims that differ only in modality are not a contradiction (`MODALITY_MISMATCH`).

### Provenance and independence

Artifacts form **source families**: declared derivations (`POST /artifacts/{id}/derivations`
with `copy`, `summary`, `export`, `reingest`, `version`, `correction`), version chains
(`supersedes_artifact_id`), and chunks with identical text all join one family, whose root is
the original. Only distinct families count as independent: an email, the note it was pasted
into, a summary, its export and the re-ingested export are one source. Stored confidence is
never multiplied by copies; `GET /units/{id}/support` reports the families and the confidence
independent support justifies (noisy-OR over families, capped at 0.99). Each re-assertion
records a `claim.corroboration` certificate that either allows the increase or blocks it with
`SOURCE_NOT_INDEPENDENT` / `DERIVED_SOURCE_DUPLICATION`. Declaring a derivation later
supersedes corroboration certificates that counted the derived source as independent.

### Model and extractor drift

`POST /units/{id}/revisions` records how a newer extractor version reads a unit's source.
Claims are compared by source anchor and subject: agreement and pure additions are recorded;
a flipped negation, a changed modality or value, or a claim the new model no longer finds is a
**disagreement**, recorded with both model versions and parked for review
(`model-disagreement`). The stored unit is never rewritten by a model update.

## Retraction

Evidence can be withdrawn by a person: a unit rejected, an entity split, two photos marked
"not a duplicate", a source retracted (`POST /artifacts/{id}/retract`) or deleted (`DELETE
/artifacts/{id}`). What follows:

- A user-decision certificate records the event.
- Units whose every live source was the removed artifact are retracted, with the graph edges
  they asserted and their tray items. Units supported elsewhere stay.
- Every live certificate that used withdrawn evidence as a direct input, or whose every
  source artifact is gone, is **retracted** — repeatedly, so conclusions built on conclusions
  follow — each with `caused_by` pointing at what withdrew it. `GET
  /certificates/{id}/affected` lists everything an event withdrew; `GET
  /certificates/{id}/chain` shows the causal chain.
- Open contradictions whose certificate was retracted, or that involve a retracted unit, are
  dismissed with a `withdraw` audit row.
- A supersession the retracted unit caused is reverted: the older claim is current again.
- Photos of a removed source leave their groups, and the rest of each group is re-derived.
- A split dismisses the pair (no automatic process re-merges it) and retracts the merge's
  certificate. Re-merging needs a person: the tray (a new review item on new evidence) or a
  manual merge.

## Reason codes

| Code | Plain language |
|---|---|
| `CHAINED_SIMILARITY` | These items are each similar to a third record, but not clearly to each other. |
| `PAIRWISE_EVIDENCE_GAP` | There isn't direct evidence that every item in the group is the same thing. |
| `ENTITY_TYPE_MISMATCH` | They are different kinds of thing. |
| `ENTITY_TYPE_UNCERTAIN` | One of them has no known type, so they may be different kinds of thing. |
| `CONTEXT_SCOPE_MISMATCH` | Their details differ (for example employer, place or scope). |
| `CONTEXT_SCOPE_UNKNOWN` | One statement names a scope the other doesn't, so they may measure different things. |
| `TIME_SCOPE_UNKNOWN` | It isn't clear the claims refer to the same time. |
| `TIME_WINDOWS_NON_OVERLAPPING` | These claims refer to different time periods. |
| `TEMPORAL_SUCCESSION` | The newer statement describes a later state; the older one is kept as history. |
| `SOURCE_NOT_INDEPENDENT` | Confidence not increased: these files come from the same original source. |
| `DERIVED_SOURCE_DUPLICATION` | This file is a copy or summary of another, so it counts once. |
| `NEGATION_AMBIGUITY` | The wording makes it unclear whether this is stated or denied. |
| `MODALITY_MISMATCH` | One describes something done, the other something planned, possible or hypothetical. |
| `UNIT_NORMALIZATION_REQUIRED` | The values use units that can't be compared automatically. |
| `GRANULARITY_MISMATCH` | One is more specific than the other (for example a city inside a state). |
| `MODEL_DISAGREEMENT` | Two versions of the extractor read this differently. |
| `USER_REJECTION_EXISTS` | You already said these are different. |
| `RETRACTION_REQUIRED` | An earlier automatic decision no longer holds and needs your attention to undo. |
| `INSUFFICIENT_PROVENANCE` | No source file backs this conclusion. |
| `GENERIC_IDENTIFIER` | The name or image is too generic to identify one specific thing. |
| `HUB_DEGREE_EXCEEDED` | It resembles many different items, so resemblance alone says little. |
| `COMPONENT_TOO_LARGE` | Too many items are linked to decide automatically. |

The codes and sentences live in `safety::reason`; the API returns both (`reason_codes` and
`reasons`). Reason codes describe why a *decision* went the way it did. A later withdrawal is
described by `outcome`, `status_reason` and `caused_by` instead ("This conclusion was
withdrawn because a supporting source was removed").

## Querying

REST (under `/api/v1`, see [API.md](API.md#semantic-safety)) and the gRPC `SafetyService`:

- `GET /certificates?conclusion_id=…` — the certificate(s) for a conclusion (a contradiction,
  a group's cluster, a merge) or anything it is about.
- `GET /artifacts/{id}/conclusions` — conclusions derived from a source.
- `GET /certificates/{id}/affected` — conclusions withdrawn by a split or retraction.
- `GET /certificates?reason=CHAINED_SIMILARITY&outcome=needs_review&live=true` — review-routed
  (or `outcome=blocked`) candidates by reason code.
- `GET /safety/summary` — counts by outcome and by reason.

The desktop app shows the certificate summary ("Why?") in the entity, group, photo-group,
contradiction and review detail views, in plain language: *Not merged automatically: these
items are each similar to a third record, but not clearly to each other.*

## Evaluation and CI

`gather-semantic-eval` runs the fixture corpus (`daemon/tests/fixtures/semantic/*.json`)
through the pure rules and checks global invariants. It needs no database, network or model.

```bash
cd daemon
cargo run --bin gather-semantic-eval -- --json semantic-safety-report.json
cargo test --test semantic_safety --test semantic_properties       # offline
DATABASE_URL=postgres://… cargo test --test semantic_safety_integration  # end to end
```

The report has: total / passed / failed scenarios, invariant failures, false automatic
actions, blocked and review-routed counts by reason code, retraction-propagation failures,
missing certificates, model disagreements, runtime, and a deterministic digest of the
semantic outcomes (SHA-256 over every scenario's canonical outcome; identical across runs and
corpus order). CI runs it in the `test` job and uploads the JSON. The build fails if an
auto-merge or photo group lacks pairwise evidence, a reported contradiction lacks an alignment
dimension, an automatic conclusion lacks a certificate or a source, a partition depends on
ingestion order, a retracted input still authorizes a conclusion, a user rejection is
overridden, or a derived chain counts as independent corroboration.

Property tests (`tests/semantic_properties.rs`, seeded, 200 cases each) cover: permutation
invariance, idempotence, bridge non-amplification, the independent-evidence constraint,
retraction propagation, user-decision protection, provenance completeness, no invalid
closure, threshold-boundary stability and model-version visibility.

## Adding a new inference rule safely

1. **Name and version it.** Add a `RuleId { id: "area.what", version: 1 }` next to the rule.
   Bump the version whenever the predicates or their meaning change.
2. **Write it pure.** Inputs are plain values; the output is an `InferenceDecision` (or a
   plan holding certificates). No I/O, no clocks, no hash-map iteration order in anything that
   reaches the output — sort, or use `BTreeMap`/`BTreeSet`.
3. **Make every requirement a predicate** with a `ReasonCode` for its failure. Add a code to
   `ReasonCode` (with its plain-language sentence and the `ALL` list) only when no existing one
   says it.
4. **Decide fail-closed.** Use `decide_from_predicates`: list the codes that mean "known to be
   compatible / forbidden" as blocking; every other failure — including missing data — is
   review. Never let a missing value pass a predicate.
5. **Fill the certificate**: subjects, direct inputs with their evidence class, source
   artifacts, config/scope/time used, model version, and a sentence a person can read.
6. **Persist and act together.** Record the certificate with `safety::store::record` in the
   same transaction as the action it justifies, and link `conclusion_id` to the row created.
7. **Withdrawal.** If the conclusion depends on inputs that can be retracted, make sure they
   are in `inputs` / `source_artifact_ids` so `propagate_withdrawal` reaches it; if acting on it
   changed state, add the reversal to `service::retract_unit_dependents` (as supersession does).
8. **Test it**: unit tests beside the rule, a fixture scenario, and — for a rule that closes
   over many items — a property test for order invariance and pairwise evidence.

## Adding a fixture or property test

Fixtures are JSON files in `daemon/tests/fixtures/semantic/`, one scenario each, with stable
string ids (turned into UUIDs deterministically) and pinned scores. The `category` picks the
runner in `safety::eval`:

| Category | Fields | Expectations (`expect`) |
|---|---|---|
| `identity` | `entities`, `pairs`, `cannot_links`, `existing` | `merge_groups`, `reasons_include`, `review_items_min` |
| `photo` | `photos` (hex hashes), `max_distance`, `hub_degree`, `cannot_links` | `groups`, `reasons_include` |
| `contradiction`, `temporal` | `units` (statement, subject, attrs, times, assignments, sources), `part_of`, `checks` | per check: `outcome` (`none` / `auto_applied` / `needs_review` / `blocked`), `reasons_include`, `supersedes`, `user_rejected` |
| `provenance` | `sources`, `derivations`, `base_confidence` | `independent_sources`, `derived_chain`, `confidence_increases` |
| `modality` | `statements` (text, modality, negated, positive_fact) | `distinct_propositions` |
| `retraction` | `certificates` (inputs, sources, conclusion), `withdraw` | `retracted` |
| `drift` | `old_claims`, `new_claims` | `disagreements` |
| `ingestion_order` | identity + photo + claim fields, `permutations`, `seed` | (invariance is the expectation) |

Identity and photo scenarios are also re-run under shuffled input orders. Use synthetic,
privacy-safe content only. For a new property, add a test to `tests/semantic_properties.rs`
using `safety::eval::SplitMix` with a fixed seed.

## Configuration

| Variable | Default | |
|---|---|---|
| `GATHER_SAFETY_HUB_DEGREE` | `3` | Auto-strength matches at which a generic name or image is a hub |
| `GATHER_SAFETY_SUCCESSION_DAYS` | `30` | Present-tense states this far apart succeed rather than contradict |
| `GATHER_SAFETY_SAME_MOMENT_HOURS` | `24` | Present-tense states this close describe the same moment |

## Migration

`daemon/migrations/0014_semantic_safety.sql` is additive (new tables, nullable columns,
indexes, and a backfill of `atomic_units.asserted_at` / `observed_at` from `valid_from`). The
reverse script is `daemon/migrations-down/0014_semantic_safety.down.sql`, kept outside the
migrations directory because sqlx and CI apply every file there. To roll back: stop the
daemon, run the down script with `psql -v ON_ERROR_STOP=1 -f …`, `DELETE FROM
_sqlx_migrations WHERE version = 14`, and start the previous binary. Certificates, photo
"not a duplicate" decisions and declared derivations are lost; nothing else is. The
integration suite applies up → down → up on a scratch database.

## Behaviour that changed

- **An automatic entity merge needs a source.** A group whose names no source artifact backs
  is held for review (`INSUFFICIENT_PROVENANCE`). Entities created by extraction always have
  one; this affects only rows inserted by hand or imported without their units.
- **Typed vs untyped.** An untyped (`other`) entity is no longer auto-merged into a typed one
  on name similarity alone; the pair is held (`ENTITY_TYPE_UNCERTAIN`).
- **Generic names and hubs** are never auto-merged.
- **Automatic merges can be withdrawn** when later evidence makes them part of a chain.
  Previously an early merge stayed, so the result depended on arrival order.
- **Photo hubs** are left out of automatic duplicate groups; chains are split in a canonical
  order instead of database row order.
- **Contradictions** across non-overlapping periods, nested places, different scopes or
  different modality are no longer reported; a later current state supersedes the older unit.
  Conflicts without time alignment are reported with `certainty = needs_review`.
- **Equivalent quantities** (`$1.2M` / `$1,200,000`) are normalized before comparison, and
  clock times in named US zones are compared as instants.
- **Rejecting a unit** also withdraws what rested on it (a supersession it caused, open
  contradictions, certificates). Restoring it re-queues it for scanning.
- **Non-actual claims don't create edges**: a plan, possibility, condition, rejection or
  negation asserts no graph relationship.

## Not yet covered

- **Topic grouping** (`clusters.kind = 'topic'`) is a reversible tag, not an identity claim,
  and carries no certificate. It still uses connected components over mutual-kNN edges.
- **Photo albums and visual topics** (time/place sessions, caption-neighbour topics) have no
  certificates.
- **Unit admission** (the confidence band that decides whether a unit is parked) records no
  certificate; the unit's `extraction_method`/`extraction_model` and review item are its record.
- **Relationship edges** are extracted evidence, not inferred: Gather does not infer graph
  relations transitively, so there is no inferred-relation rule. Merge repointing of edges is
  covered by the merge's journal and certificate.
- **Contradiction candidates from the optional Ollama judge** are covered by the same
  alignment rule, but the judge's own reasoning is not itself certified.
- **Scope and clock-time extraction.** The alignment rule reads `attrs.scope`, `attrs.metric`,
  `attrs.granularity` and `clock_time` values when an extractor provides them; the rule-based
  extractor does not produce them yet.
- **Geographic containment** comes only from `located_in` / `part_of` edges in the graph;
  there is no built-in gazetteer.
- **Automatic re-extraction.** Extractor revisions are recorded when submitted
  (`POST /units/{id}/revisions`); the pipeline does not yet re-run a new model over old sources
  by itself.
