# Releasing Gather

Pushing a `v*` tag builds the installers for Windows, macOS and Linux and publishes them, with
`SHA256SUMS`, as a GitHub release (`.github/workflows/ci.yml`, jobs `build-desktop` and
`release`).

## Cutting a release

1. Bump the version in `apps/desktop/src-tauri/tauri.conf.json`,
   `apps/desktop/src-tauri/Cargo.toml` and `apps/desktop/package.json`. The release job
   refuses a tag that doesn't match `tauri.conf.json`, because the updater compares these
   versions.
2. Merge to `main`, then tag and push: `git tag v0.2.0 && git push origin v0.2.0`.

## What an installer contains

| Part | Source |
|---|---|
| Desktop app | `apps/desktop` (Tauri) |
| `gather-daemon` sidecar | `daemon/`, built for the runner's target |
| PostgreSQL 16 + pgvector + pgcrypto | `scripts/bundle-postgres.sh` / `.ps1`: compiled from the official source tarball (checksum pinned in the script) and pgvector at a pinned tag, with OpenSSL (for pgcrypto) vendored in |

The bundling inputs are only merged in for packaging (`--config src-tauri/tauri.bundle.conf.json`),
so `npm run tauri -- dev` keeps working without them. The Postgres build is cached per OS and
script version, so changing a pinned version in the scripts rebuilds it.

## Signing (optional, off by default)

Releases start **unsigned**; [INSTALL.md](INSTALL.md#first-launch) walks users through the
one-time OS warning. Each signing hook in CI is inactive until its secrets exist, so turning one
on is a matter of adding secrets, with no workflow edits.

| What | Configure | Effect |
|---|---|---|
| **In-app updates** | Secrets `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`; repository **variable** `TAURI_UPDATER_PUBKEY` | Builds carry the public key and sign updater bundles; the release gets `latest.json`, and **Settings → Check now** can install updates. Free. |
| **macOS** | Secrets `APPLE_CERTIFICATE` (base64 .p12), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific), `APPLE_TEAM_ID` | Developer ID signing + notarization: no Gatekeeper warning. Needs the Apple Developer Program. |
| **Windows** | Secrets `WINDOWS_CERTIFICATE` (base64 .pfx), `WINDOWS_CERTIFICATE_PASSWORD` | Authenticode-signed installers: SmartScreen warnings fade as the certificate builds reputation (immediately with an EV certificate). |

### Updater key

Generate once, keep the private key safe (losing it means existing installs can't verify
future updates):

```bash
cd apps/desktop && npm run tauri signer generate -- -w ~/.tauri/gather.key
```

Put the private key file's contents in `TAURI_SIGNING_PRIVATE_KEY`, its password in
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the printed public key in the
`TAURI_UPDATER_PUBKEY` variable. Only builds made after that can self-update. Earlier installs
update once by hand.

### Known gap: notarizing the bundled database

Without a certificate, macOS builds are ad-hoc signed, which Apple silicon requires and is
enough to run. With Developer ID signing, Tauri signs the app and the daemon, but notarization
also requires every executable in `Resources/postgres` to be signed with hardened runtime. That
step isn't wired up yet. Add a `codesign --options runtime` pass over `resources/postgres`
after the certificate is imported and before `tauri build` when turning macOS signing on.

## Verifying a build locally

```bash
scripts/bundle-postgres.sh                       # Linux/macOS (not as root: initdb refuses)
cargo build --release --manifest-path daemon/Cargo.toml
mkdir -p apps/desktop/src-tauri/binaries
cp daemon/target/release/gather-daemon \
  "apps/desktop/src-tauri/binaries/gather-daemon-$(rustc -vV | sed -n 's/^host: //p')"
cd apps/desktop && npm ci && npm run tauri -- build --config src-tauri/tauri.bundle.conf.json
```
