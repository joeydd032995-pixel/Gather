#!/usr/bin/env bash
# memory-budget.sh — tier-1 check that the daemon's memory stays bounded while
# it ingests the kinds of input that used to blow it up: a batch of files, one
# large text file, and camera-sized photos. Gather has to run on 4 GB machines
# (docs/INSTALL.md), so the daemon's peak must not scale with input size.
#
# Starts the daemon against $DATABASE_URL, drives the workloads through the
# REST API the way the desktop app does (one file per request), samples the
# daemon's PSS every 0.2 s, and fails if the peak exceeds the budget.
#
# Adds data to $DATABASE_URL (it never deletes any); CI gives it its own
# throwaway database. Needs curl, psql and cargo (for the photo generator).
#
# Env:
#   DATABASE_URL                 required
#   GATHER_DAEMON_BIN            daemon binary (default daemon/target/debug/gather-daemon)
#   GATHER_MEMORY_PROFILE        standard (default) or low; low keeps the profile's own
#                                32 MB upload cap and its smaller defaults
#   GATHER_MEMORY_BUDGET_MB      peak PSS allowed (default 256, low 160; storing a file
#                                peaks near 3.5x its size + ~30 MB)
#   GATHER_MEMORY_TEXT_MB        size of the large text file (default 48, low 24)
set -euo pipefail

: "${DATABASE_URL:?DATABASE_URL must be set}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${GATHER_DAEMON_BIN:-$ROOT/daemon/target/debug/gather-daemon}
PROFILE=${GATHER_MEMORY_PROFILE:-standard}
if [ "$PROFILE" = low ]; then
  BUDGET_MB=${GATHER_MEMORY_BUDGET_MB:-160}
  TEXT_MB=${GATHER_MEMORY_TEXT_MB:-24}
  CAP_MB=32 # the low profile's default; deliberately not overridden below
  CAP_ENV=()
else
  BUDGET_MB=${GATHER_MEMORY_BUDGET_MB:-256}
  TEXT_MB=${GATHER_MEMORY_TEXT_MB:-48}
  CAP_MB=$((TEXT_MB + 16))
  CAP_ENV=(GATHER_MAX_UPLOAD_MB=$CAP_MB)
fi
PORT=7611
API="http://127.0.0.1:$PORT/api/v1"
WORK=$(mktemp -d)
echo "memory profile: $PROFILE"

env GATHER_MEMORY_PROFILE="$PROFILE" GATHER_BIND_ADDR="127.0.0.1:$PORT" GATHER_GRPC_ENABLED=false \
  GATHER_AUTH_MODE=env GATHER_API_TOKEN="" GATHER_RATE_LIMIT_RPS=0 "${CAP_ENV[@]}" \
  GATHER_EXTRACTION_INTERVAL_SECS=2 GATHER_PHOTO_INTERVAL_SECS=2 \
  "$BIN" >"$WORK/daemon.log" 2>&1 &
DAEMON=$!
SAMPLER=""
cleanup() {
  [ -n "$SAMPLER" ] && kill "$SAMPLER" 2>/dev/null || true
  kill "$DAEMON" 2>/dev/null || true
  wait "$DAEMON" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break
  kill -0 "$DAEMON" 2>/dev/null || { cat "$WORK/daemon.log"; exit 1; }
  sleep 1
done

pss_mb() { awk '/^Pss:/ { print int($2 / 1024) }' "/proc/$DAEMON/smaps_rollup" 2>/dev/null || echo 0; }
echo startup >"$WORK/phase"
( while kill -0 "$DAEMON" 2>/dev/null; do echo "$(cat "$WORK/phase") $(pss_mb)" >>"$WORK/samples"; sleep 0.2; done ) &
SAMPLER=$!
echo "idle: $(pss_mb) MB"

upload() { curl -s -o /dev/null -w '%{http_code}' -F "files=@$1" "$API/ingest/files"; }

phase() { echo "==> $*"; echo "$1" >"$WORK/phase"; }

phase batch "200 small documents, one per request"
for i in $(seq 1 200); do
  printf 'Note %s. Alice Smith met Bob Jones at Acme Corp in Berlin about project Orion.\n\n' "$i" \
    >"$WORK/note$i.txt"
  code=$(upload "$WORK/note$i.txt")
  [ "$code" = 202 ] || { echo "note $i: HTTP $code"; exit 1; }
done

phase large-text "one ${TEXT_MB} MB text file"
head -c $((TEXT_MB * 1024 * 1024 * 3 / 4)) /dev/urandom | base64 -w 100 >"$WORK/large.txt"
code=$(upload "$WORK/large.txt")
[ "$code" = 202 ] || { echo "large text: HTTP $code"; exit 1; }

phase photos "6 photos at 6000x4000 (24 MP)"
cargo build -q --manifest-path "$ROOT/daemon/Cargo.toml" --example make_test_photo
for i in $(seq 1 6); do
  "$ROOT/daemon/target/debug/examples/make_test_photo" "$WORK/photo$i.jpg" 6000 4000 "$i"
  code=$(upload "$WORK/photo$i.jpg")
  [ "$code" = 202 ] || { echo "photo $i: HTTP $code"; exit 1; }
done
# Let the photo worker hash them (it decodes each photo) before measuring.
for _ in $(seq 1 90); do
  pending=$(psql "$DATABASE_URL" -qAt -c "SELECT count(*) FROM images WHERE photo_prepared_at IS NULL")
  [ "$pending" = 0 ] && break
  sleep 1
done
[ "$pending" = 0 ] || { echo "photo worker did not finish ($pending pending)"; tail -20 "$WORK/daemon.log"; exit 1; }

phase oversized "oversized upload is refused before it is read"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Content-Length: $(( (CAP_MB + 1) * 1024 * 1024 ))" \
  -H "Content-Type: multipart/form-data; boundary=x" --data-binary $'--x--\r\n' "$API/ingest/files")
[ "$code" = 413 ] || { echo "oversized upload: expected 413, got $code"; exit 1; }

kill "$SAMPLER" 2>/dev/null || true
SAMPLER=""
echo "peak daemon PSS by phase (MB):"
awk '{ if ($2 > max[$1]) max[$1] = $2 } END { for (p in max) printf "  %-10s %s\n", p, max[p] }' "$WORK/samples"
peak=$(awk '{ print $2 }' "$WORK/samples" | sort -n | tail -1)
echo "peak daemon PSS: ${peak} MB (budget ${BUDGET_MB} MB)"
if [ "$peak" -gt "$BUDGET_MB" ]; then
  echo "FAIL: daemon memory exceeded its budget"
  exit 1
fi
echo "OK"
