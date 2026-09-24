#!/usr/bin/env bash
# Build the PostgreSQL + pgvector runtime that ships inside the desktop
# installer (Linux and macOS; Windows: bundle-postgres.ps1).
#
# Everything is compiled from pinned official sources: the PostgreSQL tarball
# is checked against a checksum pinned here (not one fetched next to it), and
# pgvector is cloned at a fixed tag. Nothing is downloaded at runtime; the
# installed app never touches the network to get its database.
#
# Usage: scripts/bundle-postgres.sh [OUT_DIR]
#   OUT_DIR defaults to apps/desktop/src-tauri/resources/postgres
set -euo pipefail

PG_VERSION=16.15
PG_SHA256=c1575341fa7bd40f5274ea465b34390f4dc64cdd0770af327005caaeb9f6b7ed
PGVECTOR_TAG=v0.8.6

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-"$ROOT/apps/desktop/src-tauri/resources/postgres"}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

case "$(uname -s)" in
  Linux) OS=linux; JOBS=$(nproc) ;;
  Darwin) OS=macos; JOBS=$(sysctl -n hw.ncpu) ;;
  *) echo "unsupported OS $(uname -s); use bundle-postgres.ps1 on Windows" >&2; exit 1 ;;
esac

sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

echo "==> PostgreSQL $PG_VERSION source"
curl -fsSL -o "$WORK/pg.tar.bz2" \
  "https://ftp.postgresql.org/pub/source/v$PG_VERSION/postgresql-$PG_VERSION.tar.bz2"
actual=$(sha256_of "$WORK/pg.tar.bz2")
if [ "$actual" != "$PG_SHA256" ]; then
  echo "checksum mismatch for postgresql-$PG_VERSION: got $actual" >&2
  exit 1
fi
tar -xjf "$WORK/pg.tar.bz2" -C "$WORK"
SRC="$WORK/postgresql-$PG_VERSION"

# OpenSSL is only here because migration 0001 creates the pgcrypto extension
# (which requires it); the server itself listens on loopback without TLS.
CONFIGURE_ENV=()
if [ "$OS" = macos ]; then
  SSL_PREFIX=$(brew --prefix openssl@3)
  CONFIGURE_ENV=(CPPFLAGS="-I$SSL_PREFIX/include" LDFLAGS="-L$SSL_PREFIX/lib")
fi

echo "==> configure + build"
(
  cd "$SRC"
  env ${CONFIGURE_ENV[@]+"${CONFIGURE_ENV[@]}"} ./configure --prefix="$OUT" \
    --with-openssl --without-icu --without-readline --disable-rpath >/dev/null
  make -s -j"$JOBS" >/dev/null
  make -s install >/dev/null
  make -s -j"$JOBS" -C contrib/pgcrypto install >/dev/null
)

echo "==> pgvector $PGVECTOR_TAG"
git -c advice.detachedHead=false clone -q --depth 1 --branch "$PGVECTOR_TAG" \
  https://github.com/pgvector/pgvector "$WORK/pgvector"
# OPTFLAGS="" : no -march=native, so the bundle runs on any CPU of this arch.
make -s -C "$WORK/pgvector" PG_CONFIG="$OUT/bin/pg_config" OPTFLAGS="" >/dev/null
make -s -C "$WORK/pgvector" PG_CONFIG="$OUT/bin/pg_config" install >/dev/null

echo "==> vendor OpenSSL libraries"
# The supervisor points the dynamic loader at lib/ (LD_LIBRARY_PATH /
# DYLD_LIBRARY_PATH), so these are found on machines without OpenSSL 3.
if [ "$OS" = macos ]; then
  cp -f "$SSL_PREFIX"/lib/libssl.3.dylib "$SSL_PREFIX"/lib/libcrypto.3.dylib "$OUT/lib/"
else
  for lib in libssl.so.3 libcrypto.so.3; do
    cp -fL "$(ldconfig -p | awk -v l="$lib" '$1 == l { print $NF; exit }')" "$OUT/lib/"
  done
fi

echo "==> trim to what the runtime needs"
# Keep: the server, initdb/pg_ctl to run it, pg_dump/pg_restore/psql for
# backups. Headers, static libs and docs are build-time only.
keep_bins="postgres initdb pg_ctl pg_dump pg_restore psql pg_isready"
for f in "$OUT"/bin/*; do
  name=$(basename "$f")
  case " $keep_bins " in *" $name "*) ;; *) rm -f "$f" ;; esac
done
rm -rf "$OUT/include" "$OUT/share/doc" "$OUT/share/man" "$OUT/lib/pgxs"
find "$OUT/lib" -name '*.a' -delete
rm -f "$OUT"/lib/libecpg* "$OUT"/lib/libpgtypes*   # embedded-SQL client libs, unused

# Record exactly what was built, for the release notes and bug reports.
cat >"$OUT/BUNDLE.txt" <<EOF
postgresql $PG_VERSION (sha256 $PG_SHA256)
pgvector $PGVECTOR_TAG
built for $OS $(uname -m)
EOF

echo "==> smoke test: initdb, start, CREATE EXTENSION vector + pgcrypto"
if [ "$OS" = macos ]; then export DYLD_LIBRARY_PATH="$OUT/lib"; else export LD_LIBRARY_PATH="$OUT/lib"; fi
DATA="$WORK/data"
"$OUT/bin/initdb" -D "$DATA" -U gather --auth=trust >/dev/null
"$OUT/bin/pg_ctl" -D "$DATA" -o "-p 7699 -c listen_addresses=127.0.0.1" -l "$WORK/pg.log" -w start >/dev/null
trap '"$OUT/bin/pg_ctl" -D "$DATA" -m immediate stop >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
"$OUT/bin/psql" -h 127.0.0.1 -p 7699 -U gather -d postgres -v ON_ERROR_STOP=1 -qAt \
  -c "CREATE EXTENSION vector; CREATE EXTENSION pgcrypto;" \
  -c "SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector, length(gen_random_uuid()::text)"
"$OUT/bin/pg_ctl" -D "$DATA" -m fast stop >/dev/null

du -sh "$OUT"
echo "bundled PostgreSQL runtime ready in $OUT"
