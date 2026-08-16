# Backup & Restore Runbook

This is the operator-facing how-to for scheduled encrypted export and
restore verification — the architecture rationale lives in
`docs/TECHNICAL-WRITEUP.md` §7 (security model) and §11. In short: **the
Gather daemon never schedules or performs backups itself.** Every script
here is invoked by you, or by an OS-level scheduler you install yourself.
This mirrors the manual flow already described in §10 step 7 — it's the
same `curl` + `restic backup` commands, just repeatable.

## Prerequisites

You've already provisioned the optional backup VPS (`infra/terraform/`) and
completed its one-time setup (`gather-init-backup-volume`, `restic init`
against `sftp:gatherbackup@<ip>:/srv/backups/restic`) — see §10 steps 6-7.
If you haven't done that yet, do it first; everything below assumes
`RESTIC_REPOSITORY` and a restic password already work for a manual backup.

## 1. Install a scheduled backup

Pick your OS:

```bash
# Linux (systemd --user)
RESTIC_REPOSITORY=sftp:gatherbackup@<ip>:/srv/backups/restic \
RESTIC_PASSWORD=<your restic repo password> \
scripts/install-schedule-linux.sh --on-calendar daily

# macOS (launchd)
RESTIC_REPOSITORY=sftp:gatherbackup@<ip>:/srv/backups/restic \
RESTIC_PASSWORD=<your restic repo password> \
scripts/install-schedule-macos.sh --hour 3 --minute 0
```

```powershell
# Windows (Task Scheduler) — set these as User environment variables first,
# e.g. [Environment]::SetEnvironmentVariable("RESTIC_REPOSITORY", "...", "User")
scripts/install-schedule-windows.ps1 -At "03:00"
```

**Non-systemd Linux** (no systemd, or you'd rather not use a user timer):
add this to your crontab yourself (`crontab -e`) instead of running an
installer script — programmatically editing your one shared crontab is a
different, riskier class of mutation than the dedicated unit files the
installers above write, so that step stays manual and explicit:

```
0 3 * * * RESTIC_REPOSITORY=... RESTIC_PASSWORD=... /path/to/scripts/gather-backup.sh >> ~/.local/state/gather/backup.log 2>&1
```

### Retention

Every backup run also prunes old snapshots (`restic forget --keep-daily 7
--keep-weekly 4 --keep-monthly 6 --prune`). Without this, a daily schedule
fills the 20 GB Hetzner volume within weeks. Override with
`RESTIC_KEEP_DAILY` / `RESTIC_KEEP_WEEKLY` / `RESTIC_KEEP_MONTHLY` if you
want a different policy.

### Authentication: `GATHER_AUTH_MODE=env` vs `keychain`

If your daemon runs with `GATHER_AUTH_MODE=env` (the docker-compose default)
and `GATHER_API_TOKEN` is empty, the API is open on loopback and no token
handling is needed — the backup script just works.

If you run `GATHER_AUTH_MODE=keychain` (the packaged desktop default), the
backup script resolves the token by calling `gather-daemon print-api-token`
— the same OS-keychain entry the desktop app itself reads. This has one
real limitation, not a bug: **keychain access is tied to your logged-in
session.** Concretely:

- **Linux**: the installer runs `loginctl enable-linger` so your systemd
  `--user` timer keeps firing after logout — but Secret Service /
  keyring-backed keychains may still be locked outside an active session
  on some desktop environments. Test one manual run after logging out
  before relying on it unattended.
- **macOS**: launchd LaunchAgents are inherently per-login-session; a
  backup scheduled while logged out will not have Keychain access.
- **Windows**: the installed task runs with your interactive logon
  context. Registering a task to run "whether you're logged on or not"
  needs a separately stored credential (a different secret-storage
  problem this runbook doesn't solve) and isn't set up by
  `install-schedule-windows.ps1`.

**If you want truly unattended backups regardless of login state**, set a
static `GATHER_API_TOKEN` for the daemon (env mode) instead of relying on
keychain mode, and pass that same token to the scheduled script via
`GATHER_API_TOKEN` in its environment. In the common case — no token
configured at all — none of this matters.

## 2. Verify your backup actually restores

An unverified backup is not a backup. There are two tiers:

### Tier 1 — automatic, every push (`scripts/ci-restore-drill.sh`)

Runs in CI (`restore-drill` job in `.github/workflows/ci.yml`) against a
throwaway Postgres service container and a throwaway **local** restic
repo — no real credentials, ever. It seeds synthetic data through real
ingestion, exports, backs up, wipes the database, restores, re-imports, and
checks both row-count fidelity and byte-for-byte content fidelity. This
proves the *mechanics* work on every change to the codebase. It does not
touch your real VPS or your real data.

You can run it yourself against a scratch Postgres before pushing:

```bash
GATHER_RESTORE_DRILL_ALLOW_DESTRUCTIVE=1 \
DATABASE_URL=postgres://gather:gather-ci@localhost:5432/gather \
GATHER_DAEMON_BIN=daemon/target/debug/gather-daemon \
scripts/ci-restore-drill.sh
```

**Never point this at a real database** — it unconditionally
`TRUNCATE ... CASCADE`s every table.

### Tier 2 — your real backup, at your own initiative (`scripts/vps-restore-drill.sh`)

Restores the **latest snapshot from your real off-site repo** into a
disposable scratch Postgres container (not your live database, a different
port), imports it through a throwaway daemon instance, and reports per-table
counts plus the most recent artifact's timestamp so you can judge whether
the backup is both complete and recent enough for your own needs:

```bash
RESTIC_REPOSITORY=sftp:gatherbackup@<ip>:/srv/backups/restic \
RESTIC_PASSWORD=<your restic repo password> \
scripts/vps-restore-drill.sh
```

This uses whatever restic secrets you already have configured for manual
backups — it introduces no new secret storage, never runs from CI or any
shared automation, and cleans up its scratch container and any restored
plaintext on exit (even if interrupted). Run it periodically — quarterly is
a reasonable cadence for a personal-scale backup — as your own check that
"the backup exists" and "the backup restores" are actually the same thing.
