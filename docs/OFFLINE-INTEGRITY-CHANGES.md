# Offline integrity implementation

Gather continues to run on local PostgreSQL and its local daemon. Ollama remains
explicitly enabled by the user, with loopback validation. These changes add no
cloud fallback, model downloads, remote telemetry, or automatic backup/update
network activity.

| Priority | Change | Implementation |
| --- | --- | --- |
| P1 | Source withdrawal | Queue filters and a locked liveness check before persistence; photo grouping and captions check sources in their write transaction; remembered source lineage prevents unsupported restoration after hard deletion. |
| P1 | Corrections | Save the complete previous unit and its edges; withdraw dependent conclusions; rebuild deterministic structure or clear it; reject delayed scanner and embedding writes. |
| P1 | Temporal recurrence | Reuse an open episode or the same anchor; split on intervening structural conflicts even before scanning; retain ended episodes and explicit rejection. |
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

Migrations 0020–0024 add revision history, remove global statement uniqueness,
add the semantic cache ledger, add embedding identity/retry state, and remember source lineage after provenance deletion. Existing
claims, dates and provenance are retained; missing temporal history is not
fabricated. Ingestion time is not used as a source's asserted date. A returning state has a separate episode after an intervening conflicting
assertion, even if scanning has not run yet. Repeated statements without a
conflicting state can still corroborate one open episode. Embeddings with unknown identity are cleared and rebuilt using the
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

CI installs or stages each packaged desktop on its native operating system and
runs a real GUI lifecycle smoke check. An opt-in CI probe minimizes the actual
native window and reports native focus/minimize state. The controller launches
three additional processes from a minimized primary and verifies successful secondary exit, primary
focus/restoration, and unchanged database/daemon PIDs. It also checks graceful
service shutdown, instance-lock reacquisition, and adoption/shutdown after an
application crash. Linux uses Xvfb with Openbox; Windows and macOS use their
native runner desktop sessions. macOS also exercises Finder/Dock reopening
through Launch Services, which does not necessarily start a second process.
Secondary windows are hidden before signaling, and Windows launchers transfer
foreground permission to the primary PID recorded under the OS-held lock.
No cloud service is required by the app.

The probe is inactive outside GitHub Actions and unless
`GATHER_DESKTOP_SMOKE_REPORT` is explicitly set.
The installer/controller script refuses to run outside GitHub Actions. It does
not replace or mock the runtime, native window APIs, OS lock, or bundled services.

A Chromium browser regression drives the real frontend through file uploads
with controlled local API responses. It verifies refresh of the selected detail,
preserved selection and pagination depth, ignored stale list successes/errors,
and absence of non-loopback frontend requests. These UI fixtures complement the
real PostgreSQL ingestion/integrity tests and native packaged-app smoke checks.

The release-daemon job explicitly runs the normally ignored 5,000-project
similarity workload. Its timing/signature-size output is diagnostic on shared
CI runners, alongside the existing graph and memory gates.
