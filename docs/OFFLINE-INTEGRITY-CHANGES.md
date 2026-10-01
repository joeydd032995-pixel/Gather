# Offline integrity implementation

Gather continues to run on local PostgreSQL and its local daemon. Ollama remains
explicitly enabled by the user, with loopback validation. These changes add no
cloud fallback, model downloads, remote telemetry, or automatic backup/update
network activity.

| Priority | Change | Implementation |
| --- | --- | --- |
| P1 | Source withdrawal | Queue filters and a locked liveness check before persistence; photo grouping and captions check sources in their write transaction. |
| P1 | Corrections | Save the complete previous unit and its edges; withdraw dependent conclusions; rebuild deterministic structure or clear it; reject delayed scanner and embedding writes. |
| P1 | Temporal recurrence | Reuse an episode with the same assertion time or anchor; retain ended episodes; allow a new episode after supersession; preserve explicit rejection. |
| P1 | Consistent exports | All export tables use one read-only repeatable-read snapshot. |
| P2 | Backup retention | Filter Gather's tag and group by host/tags across random export filenames. |
| P2 | Bundle memory and limits | Stream export with backpressure; stage imports in private files; enforce separate bundle/record limits; import atomically in dependency order. |
| P2 | Embedding retry | Backfill earlier ingestion and edits; bound batches; persist retry delay and attempt count; condition writes on content revision. |
| P2 | Embedding identity | Store configured model identity and generation; invalidate unknown/incompatible vectors; keep text search available during rebuild. |
| P2 | Browser origin guard | Reject mutating requests from untrusted, opaque or multiple origins before handlers run; preserve trusted desktop and command-line access. |
| P2 | Desktop ownership | Hold an OS file lock before adopting services; second launches signal the first window to focus and exit without adopting services. |
| P2 | Library refresh | Use upload completion version; retain selection and loaded page size; discard stale list responses. |
| P2 | Similarity cache | Use a bounded, commit-visible revision ledger; invalidate semantic signatures even when row counts do not change. |

## Migrations and compatibility

Migrations 0020–0023 add revision history, remove global statement uniqueness,
add the semantic cache ledger, and add embedding identity/retry state. Existing
claims, dates and provenance are retained; missing temporal history is not
fabricated. Ingestion time is not used as a source's asserted date. Distinct
dated assertions have separate episodes even if scanning has not run yet. Embeddings with unknown identity are cleared and rebuilt using the
user's configured local model. Startup model changes clear incompatible vectors
in one transaction. Import also clears incompatible vector-derived state.

Cache checks compact the revision ledger to the latest 257 visible revisions,
recording compaction in the same transaction. Cache keys use that committed set,
avoiding both a shared counter lock and a missed late commit. Writes append
revisions until the next cache check. Invalidation is conservative across the
bounded project cache.

The v1 bundle format remains supported, with new columns/tables exported and
defaults supplied for older bundles. Bundle limits and temporary disk needs are
documented in [BACKUP-RUNBOOK.md](BACKUP-RUNBOOK.md). The desktop requires Rust
1.89 or newer for portable OS-held file locks.

Before upgrading an existing installation, keep a local export. Returning to an
older schema should use that export with the corresponding older application;
schema rollback would discard the new revision history and temporal episodes.

## Validation

Database regressions cover withdrawal during pending extraction, corrected
structure/history, temporal episode reuse and rejection, concurrent snapshot
export, transactional import rollback, committed cache invalidation, local-model
outage/retry, delayed embedding responses and model switches. CI also runs its
existing semantic-safety evaluation, restore drill, memory budget, graph smoke
test and Linux/Windows/macOS desktop builds. Backup retention has a disposable
local restic regression.

Desktop window focusing and second-launch behavior still need an interactive
smoke check on each packaged operating system. No cloud service is required.
