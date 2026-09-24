# Build the PostgreSQL + pgvector runtime that ships inside the Windows
# installer (Linux/macOS: bundle-postgres.sh, which this mirrors).
#
# Compiled from pinned official sources: PostgreSQL from its release tag,
# verified against the commit pinned here, and pgvector at a fixed tag.
# (The release tarball can't be used: PostgreSQL 16 tarballs carry
# pre-generated parser files, and meson requires a clean source tree.)
# Nothing is downloaded at runtime.
#
# Needs an MSVC developer shell (cl, nmake), Strawberry Perl, meson + ninja,
# win_flex/win_bison (a git checkout generates its parsers), and OpenSSL from
# vcpkg (see .github/workflows/ci.yml).
#
# Usage: scripts/bundle-postgres.ps1 [-Out <dir>] [-OpenSsl <vcpkg prefix>]
param(
  [string]$Out = "$PSScriptRoot/../apps/desktop/src-tauri/resources/postgres",
  [string]$OpenSsl = "$env:VCPKG_INSTALLATION_ROOT/installed/x64-windows"
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$PgVersion = '16.15'
$PgTag = 'REL_16_15'
$PgCommit = '7d3e000c5961a544302072058a1184e9a588837b'
$PgvectorTag = 'v0.8.6'

function Invoke-Checked([string]$What, [scriptblock]$Block) {
  & $Block
  if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path $Out).Path
$Work = Join-Path ([IO.Path]::GetTempPath()) ("gather-pg-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Work | Out-Null

try {
  Write-Host "==> PostgreSQL $PgVersion source ($PgTag)"
  $src = Join-Path $Work 'postgresql'
  Invoke-Checked 'clone postgresql' {
    git -c advice.detachedHead=false clone -q --depth 1 --branch $PgTag https://github.com/postgres/postgres $src
  }
  $actual = (git -C $src rev-parse HEAD).Trim()
  if ($actual -ne $PgCommit) { throw "$PgTag resolved to $actual, expected $PgCommit" }
  $build = Join-Path $Work 'build'

  Write-Host '==> meson setup + build'
  # OpenSSL is only here because migration 0001 creates the pgcrypto
  # extension (which requires it); the server itself serves loopback only.
  $env:PKG_CONFIG_PATH = "$OpenSsl/lib/pkgconfig"
  Invoke-Checked 'meson setup' {
    meson setup $build $src --prefix=$Out --buildtype=release `
      -Dssl=openssl -Dicu=disabled -Dreadline=disabled -Dnls=disabled `
      -Dzlib=disabled -Dlz4=disabled -Dzstd=disabled -Dtap_tests=disabled `
      -Dplperl=disabled -Dplpython=disabled -Dpltcl=disabled `
      "-Dextra_include_dirs=$OpenSsl/include" "-Dextra_lib_dirs=$OpenSsl/lib" `
      -DBISON=win_bison -DFLEX=win_flex
  }
  Invoke-Checked 'meson build' { meson compile -C $build }
  Invoke-Checked 'meson install' { meson install -C $build --quiet }

  # pgvector's Makefile.win links against lib\postgres.lib, the server's
  # import library; meson builds it but does not install it.
  $implib = Join-Path $Out 'lib/postgres.lib'
  if (-not (Test-Path $implib)) {
    $found = Get-ChildItem $build -Recurse -File |
      Where-Object { $_.Name -in 'postgres.lib', 'postgres.exe.lib' } |
      Select-Object -First 1
    if (-not $found) { throw 'the server import library (postgres.lib) was not built' }
    Copy-Item $found.FullName $implib
  }

  Write-Host "==> pgvector $PgvectorTag"
  $pgvector = Join-Path $Work 'pgvector'
  Invoke-Checked 'clone pgvector' {
    git -c advice.detachedHead=false clone -q --depth 1 --branch $PgvectorTag https://github.com/pgvector/pgvector $pgvector
  }
  Push-Location $pgvector
  try {
    $env:PGROOT = $Out
    Invoke-Checked 'pgvector build' { nmake /nologo /F Makefile.win }
    Invoke-Checked 'pgvector install' { nmake /nologo /F Makefile.win install }
  } finally { Pop-Location }

  Write-Host '==> vendor runtime DLLs'
  # Found next to the executables: OpenSSL for pgcrypto, and the MSVC runtime
  # (app-local deployment) for machines without the VC++ redistributable.
  Copy-Item "$OpenSsl/bin/libssl-3-x64.dll", "$OpenSsl/bin/libcrypto-3-x64.dll" "$Out/bin/"
  foreach ($dll in 'vcruntime140.dll', 'vcruntime140_1.dll') {
    $path = Join-Path $env:SystemRoot "System32/$dll"
    if (Test-Path $path) { Copy-Item $path "$Out/bin/" }
  }

  Write-Host '==> trim to what the runtime needs'
  $keep = 'postgres', 'initdb', 'pg_ctl', 'pg_dump', 'pg_restore', 'psql', 'pg_isready'
  Get-ChildItem "$Out/bin" -Filter '*.exe' |
    Where-Object { $keep -notcontains $_.BaseName } |
    Remove-Item
  Get-ChildItem "$Out/bin" -Filter '*.pdb' | Remove-Item
  # Embedded-SQL client libraries: unused.
  Get-ChildItem "$Out/bin" -Include 'libecpg*.dll', 'libpgtypes.dll' -Recurse | Remove-Item
  foreach ($dir in 'include', 'share/doc', 'lib/pgxs') {
    if (Test-Path "$Out/$dir") { Remove-Item -Recurse -Force "$Out/$dir" }
  }
  Get-ChildItem "$Out/lib" -Recurse -Include '*.lib', '*.pdb', '*.a' | Remove-Item

  @(
    "postgresql $PgVersion ($PgTag, commit $PgCommit)",
    "pgvector $PgvectorTag",
    'built for windows x86_64'
  ) | Set-Content "$Out/BUNDLE.txt"

  Write-Host '==> smoke test: initdb, start, CREATE EXTENSION vector + pgcrypto'
  $data = Join-Path $Work 'data'
  Invoke-Checked 'initdb' { & "$Out/bin/initdb.exe" -D $data -U gather --auth=trust | Out-Null }
  # Not piped: the server pg_ctl launches inherits its output handles, so a
  # pipe (| Out-Null) would never close and the script would hang.
  Invoke-Checked 'pg_ctl start' {
    & "$Out/bin/pg_ctl.exe" -D $data -o '-p 7699 -c listen_addresses=127.0.0.1' -l "$Work/pg.log" -w start
  }
  try {
    Invoke-Checked 'psql' {
      & "$Out/bin/psql.exe" -h 127.0.0.1 -p 7699 -U gather -d postgres -v ON_ERROR_STOP=1 -qAt `
        -c 'CREATE EXTENSION vector; CREATE EXTENSION pgcrypto;' `
        -c "SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector, length(gen_random_uuid()::text)"
    }
  } finally {
    & "$Out/bin/pg_ctl.exe" -D $data -m fast -w stop | Out-Null
  }

  "{0:N0} MB" -f ((Get-ChildItem $Out -Recurse | Measure-Object Length -Sum).Sum / 1MB)
  Write-Host "bundled PostgreSQL runtime ready in $Out"
} finally {
  Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
}
