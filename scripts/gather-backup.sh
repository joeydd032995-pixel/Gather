#!/usr/bin/env bash
# gather-backup.sh — export the local Gather store and push an encrypted
# backup to the restic repository. Formalizes docs/TECHNICAL-WRITEUP.md
# §10 step 7 into a script an OS-level scheduler can invoke (see
# install-schedule-linux.sh / install-schedule-macos.sh).
#
# This script is the ONLY thing that ever triggers the outbound VPS
# connection, and it only runs when a human or a human-installed OS
# scheduler invokes it — never the daemon (§7.4/§7.5). Do not wire this
# into the daemon process.
#
# Required environment:
#   RESTIC_REPOSITORY   restic repo URL, e.g. sftp:gatherbackup@<ip>:/srv/backups/restic
#   RESTIC_PASSWORD (or RESTIC_PASSWORD_COMMAND / RESTIC_PASSWORD_FILE)
# Optional environment:
#   GATHER_BASE_URL      default http://127.0.0.1:7601
#   GATHER_API_TOKEN     bearer token; if unset, tries `gather-daemon print-api-token`
#   GATHER_DAEMON_BIN    path to the gather-daemon binary (default: `gather-daemon` on PATH)
#   GATHER_BACKUP_LOG    path to the append-only run log
#   RESTIC_KEEP_DAILY / RESTIC_KEEP_WEEKLY / RESTIC_KEEP_MONTHLY  retention (defaults 7/4/6)
set -euo pipefail

GATHER_BASE_URL="${GATHER_BASE_URL:-http://127.0.0.1:7601}"
GATHER_DAEMON_BIN="${GATHER_DAEMON_BIN:-gather-daemon}"
RESTIC_KEEP_DAILY="${RESTIC_KEEP_DAILY:-7}"
RESTIC_KEEP_WEEKLY="${RESTIC_KEEP_WEEKLY:-4}"
RESTIC_KEEP_MONTHLY="${RESTIC_KEEP_MONTHLY:-6}"

if [ -z "${RESTIC_REPOSITORY:-}" ]; then
  echo "gather-backup: RESTIC_REPOSITORY must be set" >&2
  exit 1
fi

default_log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/gather"
GATHER_BACKUP_LOG="${GATHER_BACKUP_LOG:-$default_log_dir/backup.log}"
mkdir -p "$(dirname "$GATHER_BACKUP_LOG")"

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" | tee -a "$GATHER_BACKUP_LOG" >&2
}

bundle="$(mktemp -t gather-bundle.XXXXXX.ndjson)"
cleanup() {
  rm -f "$bundle"
}
trap cleanup EXIT

# Resolve the bearer token: explicit env var, else the OS keychain via the
# daemon's own CLI, else none (matches the documented open-on-loopback
# fallback when no auth is configured).
token="${GATHER_API_TOKEN:-}"
if [ -z "$token" ] && command -v "$GATHER_DAEMON_BIN" >/dev/null 2>&1; then
  token="$("$GATHER_DAEMON_BIN" print-api-token 2>/dev/null || true)"
fi

auth_header=()
if [ -n "$token" ]; then
  auth_header=(-H "Authorization: Bearer $token")
fi

log "backup: exporting from $GATHER_BASE_URL"
if ! curl -fsS "${auth_header[@]}" "$GATHER_BASE_URL/api/v1/export" -o "$bundle"; then
  log "backup: FAILED — export request failed"
  exit 1
fi
bundle_size=$(wc -c <"$bundle" | tr -d ' ')

if [ "$bundle_size" -eq 0 ]; then
  log "backup: FAILED — export returned an empty bundle"
  exit 1
fi

# Initialize the repo on first use (idempotent: restic init fails loudly if
# already initialized, which we treat as success).
if ! restic snapshots >/dev/null 2>&1; then
  log "backup: repository not yet initialized, running restic init"
  restic init
fi

if ! restic backup "$bundle" --tag gather-bundle; then
  log "backup: FAILED — restic backup failed"
  exit 1
fi
snapshot_id=$(restic snapshots --latest 1 --json | grep -o '"short_id":"[a-f0-9]*"' | head -1 | cut -d'"' -f4)

if ! restic forget \
  --keep-daily "$RESTIC_KEEP_DAILY" \
  --keep-weekly "$RESTIC_KEEP_WEEKLY" \
  --keep-monthly "$RESTIC_KEEP_MONTHLY" \
  --prune; then
  log "backup: WARNING — snapshot $snapshot_id taken but retention pruning failed"
  exit 1
fi

log "backup: OK — snapshot ${snapshot_id:-unknown} (${bundle_size} bytes)"
