#!/usr/bin/env bash
# Disposable local restic regression: random export paths share retention.
set -euo pipefail
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
export RESTIC_REPOSITORY="$work/repository"
export RESTIC_PASSWORD="gather-disposable-retention-test"
restic init >/dev/null
printf 'unrelated data\n' >"$work/unrelated"
restic backup "$work/unrelated" --tag unrelated --time "2026-01-01 12:00:00" >/dev/null
for day in 01 02 03 04; do
  bundle="$(mktemp "$work/gather-bundle.XXXXXX.ndjson")"
  printf 'synthetic bundle %s\n' "$day" >"$bundle"
  restic backup "$bundle" --tag gather-bundle --time "2026-02-$day 12:00:00" >/dev/null
  rm "$bundle"
done
restic forget --tag gather-bundle --group-by host,tags --keep-daily 2 --prune >/dev/null
snapshots="$(restic snapshots --tag gather-bundle --json)"
count="$(printf '%s' "$snapshots" | jq 'length')"
[ "$count" -eq 2 ] || { echo "Expected two Gather snapshots, got $count" >&2; exit 1; }
count="$(restic snapshots --tag unrelated --json | jq 'length')"
[ "$count" -eq 1 ] || { echo "Retention removed an unrelated snapshot" >&2; exit 1; }
restic check >/dev/null
echo "Local backup retention regression passed"
