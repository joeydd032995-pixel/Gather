# scripts/

Tooling for scheduled encrypted export and restore verification (see
[docs/BACKUP-RUNBOOK.md](../docs/BACKUP-RUNBOOK.md) for the full operator
guide, and `docs/TECHNICAL-WRITEUP.md` §11 for why all of this lives outside
the daemon process rather than as an in-daemon scheduler).

| Script | Safe to run ad hoc? | Where it runs |
|---|---|---|
| `gather-backup.sh` / `gather-backup.ps1` | Yes — this is exactly what the manual `curl` + `restic backup` flow (write-up §10 step 7) already does, just scripted. Requires `RESTIC_REPOSITORY` + a restic password. | Your machine, invoked by hand or by a scheduler you installed. |
| `install-schedule-linux.sh` | Yes, idempotent (safe to re-run to change the schedule). | Your machine (Linux, systemd `--user`). |
| `install-schedule-macos.sh` | Yes, idempotent (bootout-then-bootstrap). | Your machine (macOS, launchd). |
| `install-schedule-windows.ps1` | Yes, idempotent (`-Force` re-registration). | Your machine (Windows, Task Scheduler). |
| `ci-restore-drill.sh` | **CI-only. Do not run against a real database** — it unconditionally `TRUNCATE ... CASCADE`s every Gather table in `$DATABASE_URL`. Refuses to run without `GATHER_RESTORE_DRILL_ALLOW_DESTRUCTIVE=1`. Uses only synthetic data and a throwaway local restic repo — no real secrets. | GitHub Actions (`restore-drill` job in `.github/workflows/ci.yml`), or locally against a scratch Postgres you don't mind wiping. |
| `ci-restore-drill.sh` (local dry-run) | Same destructive caveat as above — only point it at a scratch/throwaway Postgres, never your real data. | Your machine, before pushing changes to the drill itself. |
| `vps-restore-drill.sh` | Yes — never touches your live daemon/DB (uses a disposable scratch Postgres container + a distinct port). Uses **your real** restic repo/secrets, read-only. Never run this from CI or any shared automation. | Your machine, at your own initiative, against your real off-site VPS repo. |

Non-systemd Linux (no `install-schedule-linux.sh` support): see the crontab
line documented in `docs/BACKUP-RUNBOOK.md` instead — editing a user's
shared crontab programmatically isn't something an installer script does
here.
