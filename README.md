# Gather

**Local-first AI data sovereignty daemon.** Ingest your AI chat exports (ChatGPT, Claude, …),
agent/session logs, and manually uploaded documents & photos into a versioned, deduplicated
PostgreSQL + pgvector knowledge store; extract timestamped facts/claims/decisions with full
provenance; organize them into a personal knowledge graph; and continuously surface
contradictions across sources, models, and time — all offline by default.

- **Stack**: Rust (Axum) daemon · PostgreSQL 16 + pgvector · Tauri v2 + React/TypeScript
- **Privacy**: loopback-only APIs, zero outbound traffic by default; encrypted VPS backup is
  strictly opt-in (~€5.25/month on Hetzner)
- **Spec**: the full technical write-up — architecture, complete DDL, REST+gRPC contract,
  extraction pipeline, contradiction algorithm, security model — lives in
  [`docs/TECHNICAL-WRITEUP.md`](docs/TECHNICAL-WRITEUP.md)

## Quickstart

```bash
cp .env.example .env   # set POSTGRES_PASSWORD (e.g. openssl rand -hex 24)
docker compose up --build -d
curl -s http://127.0.0.1:7601/readyz
```

Then follow **[§10 “What to run first”](docs/TECHNICAL-WRITEUP.md#10-what-to-run-first-quickstart)**
in the write-up for Phase-0 validation, the desktop app, dashboards, and the optional backup VPS.

## Repository layout

| Path | Contents |
|---|---|
| `daemon/` | Axum daemon: ingestion (chat/agent/file), extraction worker (PDF text, image EXIF/OCR, rule-based + opt-in Ollama atomic units), query & search, export/import, contradiction scanner + review; schema in `daemon/migrations/` |
| `apps/desktop/` | Tauri v2 + React shell with drag-and-drop / native-picker upload |
| `proto/gather/v1/` | gRPC contract mirroring the REST API — served by the daemon on `127.0.0.1:7602` (tonic; `daemon/src/grpc/`) |
| `docker/`, `docker-compose.yml` | Local stack (daemon + Postgres/pgvector, optional Prometheus/Grafana profile) |
| `infra/terraform/` | **Optional, opt-in** Hetzner backup VM (firewall, LUKS volume, hardening) |
| `scripts/` | OS-level scheduled-backup scripts + per-OS installers, and the two-tier restore-drill tooling (`docs/BACKUP-RUNBOOK.md`) — the daemon itself never schedules or triggers any of this |
| `observability/` | Prometheus scrape config + provisioned Grafana dashboard |
| `.github/workflows/ci.yml` | Single CI workflow: lint, tests vs pgvector, daemon binary, Tauri bundles (Linux/Windows/macOS), releases |
| `docs/TECHNICAL-WRITEUP.md` | The full specification |

## Development

```bash
cd daemon
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                    # DB integration tests run when DATABASE_URL is set
cd ../apps/desktop && npm install && npm run tauri -- dev
```
