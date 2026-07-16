#!/usr/bin/env bash
# ci-restore-drill.sh — Tier 1 restore drill: a synthetic, no-real-secrets
# proof that backup -> restore -> import actually round-trips every table.
#
# Seeds data through real ingestion (not hand-inserted SQL), exports,
# backs up to a throwaway local restic repo, wipes the database exactly as
# docs/TECHNICAL-WRITEUP.md §4.4 describes ("export -> TRUNCATE ... CASCADE
# -> import -> identical query results"), restores, re-imports, and
# verifies both row-count fidelity and byte-for-byte content fidelity.
#
# DESTRUCTIVE: unconditionally truncates every Gather table in $DATABASE_URL.
# Never run this against a real database. Intended for a throwaway
# Postgres service container (see the `restore-drill` CI job), which is why
# it refuses to run without an explicit opt-in flag.
#
# Required environment:
#   DATABASE_URL              throwaway Postgres, e.g. the CI service container
#   GATHER_RESTORE_DRILL_ALLOW_DESTRUCTIVE=1   required safety gate
# Optional environment:
#   GATHER_DAEMON_BIN   path to the gather-daemon binary (default: `gather-daemon` on PATH)
#   GATHER_BASE_URL     default http://127.0.0.1:7601
set -euo pipefail

if [ "${GATHER_RESTORE_DRILL_ALLOW_DESTRUCTIVE:-}" != "1" ]; then
  echo "ci-restore-drill: refusing to run without GATHER_RESTORE_DRILL_ALLOW_DESTRUCTIVE=1" >&2
  echo "this script unconditionally TRUNCATEs every table in \$DATABASE_URL." >&2
  exit 1
fi

for tool in curl jq psql restic; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ci-restore-drill: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

if [ -z "${DATABASE_URL:-}" ]; then
  echo "ci-restore-drill: DATABASE_URL must be set" >&2
  exit 1
fi

GATHER_DAEMON_BIN="${GATHER_DAEMON_BIN:-gather-daemon}"
GATHER_BASE_URL="${GATHER_BASE_URL:-http://127.0.0.1:7601}"
TOKEN="ci-restore-drill-$(head -c8 /dev/urandom | od -An -tx1 | tr -d ' \n')"

workdir="$(mktemp -d)"
daemon_pid=""

cleanup() {
  if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf "$workdir"
}
trap cleanup EXIT

log() { printf '[ci-restore-drill] %s\n' "$1"; }

curl_json() {
  # curl_json <method> <path> [json-body]
  local method="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -fsS -X "$method" \
      -H "Authorization: Bearer $TOKEN" \
      -H "content-type: application/json" \
      -d "$body" \
      "$GATHER_BASE_URL$path"
  else
    curl -fsS -X "$method" -H "Authorization: Bearer $TOKEN" "$GATHER_BASE_URL$path"
  fi
}

# ---------------------------------------------------------------- start ----
log "starting daemon ($GATHER_DAEMON_BIN)"
GATHER_API_TOKEN="$TOKEN" \
  GATHER_BIND_ADDR="127.0.0.1:7601" \
  GATHER_GRPC_ENABLED=false \
  GATHER_EXTRACTION_ENABLED=true \
  GATHER_EXTRACTION_INTERVAL_SECS=1 \
  GATHER_SCAN_ENABLED=true \
  GATHER_SCAN_INTERVAL_SECS=1 \
  DATABASE_URL="$DATABASE_URL" \
  "$GATHER_DAEMON_BIN" &
daemon_pid=$!

ready=""
for _ in $(seq 1 60); do
  if curl -fsS "$GATHER_BASE_URL/readyz" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  log "FAILED: daemon never became ready"
  exit 1
fi
log "daemon ready"

# ----------------------------------------------------------------- seed ----
marker="drill-$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' \n')"
subject="Zeta${marker}Budget"

log "seeding conflicting chat facts (drives entities/atomic_units/relationships/contradictions)"
curl_json POST /api/v1/ingest/chat-export "$(jq -n --arg id "$marker-a" --arg text "My $subject is \$50 per month." '
  {platform:"generic", data:{schema:"gather-generic-v1", conversations:[
    {id:$id, messages:[{role:"user", content:$text, created_at:"2026-04-01T10:00:00Z"}]}
  ]}}')" >/dev/null
curl_json POST /api/v1/ingest/chat-export "$(jq -n --arg id "$marker-b" --arg text "My $subject is \$75 per month." '
  {platform:"generic", data:{schema:"gather-generic-v1", conversations:[
    {id:$id, messages:[{role:"user", content:$text, created_at:"2026-04-01T10:05:00Z"}]}
  ]}}')" >/dev/null

log "seeding a document + an image (drives documents/document_segments/images)"
fixtures_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/daemon/tests/fixtures"
curl -fsS -X POST -H "Authorization: Bearer $TOKEN" \
  -F "file=@$fixtures_dir/tiny.pdf;type=application/pdf" \
  "$GATHER_BASE_URL/api/v1/ingest/files" >/dev/null
curl -fsS -X POST -H "Authorization: Bearer $TOKEN" \
  -F "file=@$fixtures_dir/tiny.png;type=image/png" \
  "$GATHER_BASE_URL/api/v1/ingest/files" >/dev/null

log "waiting for extraction + scan workers to drain and produce a contradiction"
contradiction_id=""
for _ in $(seq 1 60); do
  open="$(curl_json GET '/api/v1/contradictions?status=open')"
  contradiction_id="$(echo "$open" | jq -r --arg s "$subject" '.items[] | select(.unit_a.statement | contains($s)) | .id' | head -1)"
  if [ -n "$contradiction_id" ]; then
    break
  fi
  sleep 1
done
if [ -z "$contradiction_id" ]; then
  log "FAILED: no contradiction detected for seeded data within the deadline"
  exit 1
fi
log "contradiction detected: $contradiction_id"

# Resolve it so contradiction_audit also gets a 'resolve' action row, not
# just 'detected' — a more complete fixture for the round-trip.
curl_json POST "/api/v1/contradictions/$contradiction_id/resolve" \
  '{"resolution":"resolved_a","note":"ci-restore-drill fixture","actor":"ci-restore-drill"}' >/dev/null
log "contradiction resolved"

# --------------------------------------------------------------- export ----
before_bundle="$workdir/gather-bundle-before.ndjson"
log "exporting bundle"
curl_json GET /api/v1/export >"$before_bundle"
if [ ! -s "$before_bundle" ]; then
  log "FAILED: export produced an empty bundle"
  exit 1
fi

# ------------------------------------------------------------ restic bu ----
export RESTIC_REPOSITORY="$workdir/restic-repo"
export RESTIC_PASSWORD="ci-restore-drill-disposable-password"
log "restic init + backup (local throwaway repo, no real credentials)"
restic init >/dev/null
(cd "$workdir" && restic backup "$(basename "$before_bundle")" --tag ci-restore-drill) >/dev/null

# --------------------------------------------------------------- wipe -----
all_tables="$(head -1 "$before_bundle" | jq -r '.row.tables | join(", ")')"
log "truncating: $all_tables"
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -c "TRUNCATE $all_tables CASCADE;" >/dev/null

artifacts_after_wipe="$(curl_json GET /api/v1/artifacts)"
if [ "$(echo "$artifacts_after_wipe" | jq '.items | length')" != "0" ]; then
  log "FAILED: /api/v1/artifacts not empty after truncate"
  exit 1
fi

# ------------------------------------------------------------- restore ----
log "restic restore"
restore_dir="$workdir/restored"
restic restore latest --target "$restore_dir" >/dev/null
# restic nests the restored file under its (resolved-absolute) backup path,
# so locate it by name rather than assuming the exact nesting.
restored_bundle="$(find "$restore_dir" -type f -name "$(basename "$before_bundle")" | head -1)"
if [ -z "$restored_bundle" ]; then
  log "FAILED: restored bundle not found under $restore_dir"
  exit 1
fi

log "importing restored bundle"
import_response="$workdir/import-response.json"
curl -fsS -X POST -H "Authorization: Bearer $TOKEN" \
  --data-binary "@$restored_bundle" \
  "$GATHER_BASE_URL/api/v1/import" >"$import_response"

# ------------------------------------------------------------ verify (a) --
# The manifest's "tables" list is the bundle FORMAT's static schema (every
# table the format version covers), not a per-export indicator of which
# tables actually hold data — so it cannot be used to detect "this table
# should have had rows but didn't". Instead, assert against the specific
# tables this drill's own seed data is known to drive. entity_aliases is
# deliberately excluded: verified directly against the source (no `INSERT
# INTO entity_aliases` exists anywhere in daemon/src today, confirmed by
# grep — it is read-only dead weight in the current extraction pipeline,
# a pre-existing product gap outside this drill's scope, not a regression).
expected_populated_tables="ingestion_jobs artifacts conversations messages documents document_segments images entities atomic_units atomic_unit_provenance relationships contradictions contradiction_audit"

for table in $expected_populated_tables; do
  counts="$(jq -r --arg t "$table" '.tables[$t] // empty | "\(.in_bundle) \(.inserted)"' "$import_response")"
  if [ -z "$counts" ]; then
    log "FAILED: expected table '$table' to have data from the seeded fixtures, but it has none"
    exit 1
  fi
  read -r in_bundle inserted <<<"$counts"
  if [ "$in_bundle" -eq 0 ] || [ "$inserted" != "$in_bundle" ]; then
    log "FAILED: $table: in_bundle=$in_bundle inserted=$inserted"
    exit 1
  fi
done
log "verified: all seed-driven tables round-tripped with inserted == in_bundle > 0"

# Belt-and-braces: whatever tables DID end up in the bundle (including any
# not in the expected list above) must still round-trip with fidelity —
# catches corruption in a table this drill doesn't deliberately seed.
mismatches="$(jq -r '.tables | to_entries[] | select(.value.inserted != .value.in_bundle) | "\(.key): in_bundle=\(.value.in_bundle) inserted=\(.value.inserted)"' "$import_response")"
if [ -n "$mismatches" ]; then
  log "FAILED: row-count mismatch after restore:"
  echo "$mismatches" >&2
  exit 1
fi

# ------------------------------------------------------------ verify (b) --
after_bundle="$workdir/gather-bundle-after.ndjson"
curl_json GET /api/v1/export >"$after_bundle"

before_sorted="$workdir/before.sorted"
after_sorted="$workdir/after.sorted"
grep -v '"type":"manifest"' "$before_bundle" | sort >"$before_sorted"
grep -v '"type":"manifest"' "$after_bundle" | sort >"$after_sorted"
if ! diff -q "$before_sorted" "$after_sorted" >/dev/null; then
  log "FAILED: bundle content differs after the full round trip"
  diff "$before_sorted" "$after_sorted" | head -20 >&2
  exit 1
fi
log "verified: exported bundle content is byte-identical before and after the round trip"

log "OK — restore drill passed"
