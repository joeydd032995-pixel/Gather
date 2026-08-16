#!/usr/bin/env bash
# vps-restore-drill.sh — Tier 2 restore drill: proves your REAL off-site
# backup actually restores, using YOUR real restic repo/secrets. This is a
# user-run runbook, never wired into CI or any shared automation:
#   - never runs in a shared/CI context
#   - never writes your restic password, SSH key, or restored plaintext
#     anywhere outside a per-run temp directory that is deleted on exit
#   - never touches your live daemon or its database — everything happens
#     in a disposable scratch Postgres container on a different port
#   - never touches infra/terraform state
#
# Prerequisites: you have already provisioned the optional backup VPS
# (infra/terraform/) and have RESTIC_REPOSITORY / RESTIC_PASSWORD (or
# RESTIC_PASSWORD_COMMAND) set exactly as you use them for your normal
# manual/scheduled backups. This script introduces no new secret storage.
#
# Usage: scripts/vps-restore-drill.sh [--yes] [--daemon-bin PATH]
set -euo pipefail

assume_yes=""
daemon_bin="${GATHER_DAEMON_BIN:-gather-daemon}"
scratch_port=17601
pg_port=17432

while [ $# -gt 0 ]; do
  case "$1" in
    --yes)
      assume_yes=1
      shift
      ;;
    --daemon-bin)
      daemon_bin="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [ -z "${RESTIC_REPOSITORY:-}" ]; then
  echo "vps-restore-drill: RESTIC_REPOSITORY must be set to your real backup repo" >&2
  exit 1
fi
if [ -z "${RESTIC_PASSWORD:-}" ] && [ -z "${RESTIC_PASSWORD_COMMAND:-}" ]; then
  echo "vps-restore-drill: RESTIC_PASSWORD or RESTIC_PASSWORD_COMMAND must be set" >&2
  exit 1
fi
for tool in curl jq docker restic "$daemon_bin"; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "vps-restore-drill: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

echo "This will:"
echo "  1. Restore the LATEST snapshot from: $RESTIC_REPOSITORY"
echo "  2. Import it into a brand-new SCRATCH Postgres container (not your live DB)"
echo "  3. Run a throwaway gather-daemon against that scratch DB on 127.0.0.1:$scratch_port"
echo "  4. Report per-table counts and freshness, then tear everything down"
echo
if [ -z "$assume_yes" ]; then
  read -r -p "Proceed? [y/N] " reply
  case "$reply" in
    y | Y | yes | YES) ;;
    *)
      echo "aborted"
      exit 0
      ;;
  esac
fi

container_name="gather-restore-drill-$$"
workdir="$(mktemp -d)"
daemon_pid=""
pg_password="$(head -c12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
token="vps-drill-$(head -c8 /dev/urandom | od -An -tx1 | tr -d ' \n')"

cleanup() {
  echo "tearing down..."
  if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  docker rm -f "$container_name" >/dev/null 2>&1 || true
  # $workdir briefly holds your real restored plaintext bundle — always removed.
  rm -rf "$workdir"
}
trap cleanup EXIT

echo "[1/5] restic restore latest -> $workdir/restored"
restic restore latest --target "$workdir/restored"
bundle="$(find "$workdir/restored" -type f -name '*.ndjson' | head -1)"
if [ -z "$bundle" ]; then
  echo "vps-restore-drill: no .ndjson bundle found in the restored snapshot" >&2
  exit 1
fi
echo "found bundle: $bundle ($(wc -c <"$bundle") bytes)"

echo "[2/5] starting scratch Postgres (plain pgvector/pgvector:pg16, port $pg_port)"
docker run -d --name "$container_name" \
  -p "127.0.0.1:$pg_port:5432" \
  -e POSTGRES_DB=gather \
  -e POSTGRES_USER=gather \
  -e POSTGRES_PASSWORD="$pg_password" \
  pgvector/pgvector:pg16 >/dev/null

for _ in $(seq 1 30); do
  if docker exec "$container_name" pg_isready -U gather -d gather >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

echo "[3/5] starting scratch daemon on 127.0.0.1:$scratch_port (extraction/scan/gRPC disabled — import-only check)"
GATHER_API_TOKEN="$token" \
  GATHER_BIND_ADDR="127.0.0.1:$scratch_port" \
  GATHER_GRPC_ENABLED=false \
  GATHER_EXTRACTION_ENABLED=false \
  GATHER_SCAN_ENABLED=false \
  DATABASE_URL="postgres://gather:$pg_password@127.0.0.1:$pg_port/gather" \
  "$daemon_bin" &
daemon_pid=$!

ready=""
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$scratch_port/readyz" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "vps-restore-drill: scratch daemon never became ready" >&2
  exit 1
fi

echo "[4/5] importing restored bundle"
import_response="$workdir/import-response.json"
curl -fsS -X POST -H "Authorization: Bearer $token" \
  --data-binary "@$bundle" \
  "http://127.0.0.1:$scratch_port/api/v1/import" >"$import_response"

echo
echo "=== per-table import counts ==="
jq -r '.tables | to_entries[] | "\(.key): in_bundle=\(.value.in_bundle) inserted=\(.value.inserted)"' "$import_response"

echo
echo "[5/5] freshness check against the scratch DB"
artifacts_response="$(curl -fsS -H "Authorization: Bearer $token" "http://127.0.0.1:$scratch_port/api/v1/artifacts?limit=1")"
freshness="$(echo "$artifacts_response" | jq -r '.items[0].ingested_at // "no artifacts found"')"
echo "most recent artifact ingested_at: $freshness"
echo
echo "OK — restore from $RESTIC_REPOSITORY imported successfully."
echo "Judge for yourself: is '$freshness' recent enough for your own RPO expectations?"
