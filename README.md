# Rivet — Media Automation Control Plane

A self-hosted, publicly-accessible **media automation control plane** that manages
discovery, decisioning, acquisition, verification, and import — delegating actual
downloading to mature external clients (qBittorrent, Deluge, Transmission,
NZBGet, SABnzbd).

Core goals: **fast**, **explainable**, **secure**, **observable**, great as a
**single-container install**.

## Implementation

| Layer      | Technology                                                        |
|------------|-------------------------------------------------------------------|
| API/Engine | **Rust** — axum + tokio + SQLx (SQLite), role-gated WebSocket hub |
| Storage    | SQLite (WAL, enforced foreign keys), transactional migrations     |
| Web UI     | SvelteKit SSR (`apps/web`)                                        |
| Deploy     | Single container: nginx `:8080` → Rust API `:8081` + node `:3000` |

### Backend layout (`src/`)

- `security/` — Argon2id passwords, AES-256-GCM secret box, bounded rate
  limiter, and the **SSRF-safe outbound layer**: every outbound request is
  validated once and its resolved IPs *pinned* into the client's DNS resolver;
  redirects are followed manually with per-hop revalidation. No adapter may
  construct a raw HTTP client.
- `storage/` — connection pool forces `foreign_keys`/WAL/`busy_timeout`
  regardless of DSN; migrations run one file per `BEGIN IMMEDIATE`
  transaction, keyed by filename, with drift detection; durable job queue with
  leases, renewal, owner-checked terminal writes, and attempt caps.
- `ws/` — hub enforces channel clearance at delivery: `audit` is admin-only,
  `downloads`/`imports` operator+, `activity`/`jobs`/`health` any
  authenticated user. Unsubscribe is explicit.
- `jobs/` — worker panic-isolates handlers (a poisoned job fails, never the
  process); heartbeat renews leases; every pipeline continuation propagates
  enqueue errors; a reconciler rescues stale non-terminal acquisitions; a
  retention janitor prunes sessions/jobs/audit/candidates.
- `adapters/` — Newznab + Torznab indexers (Prowlarr aggregation, API-key
  redaction), qBittorrent (category-scoped, v5 state names), Deluge,
  Transmission, NZBGet, SABnzbd, dry-run simulator, TMDB/TVDB metadata.
- `httpapi/` — routes mirroring `packages/contracts/openapi.yaml`; CSRF
  double-submit on cookie-auth mutations; RBAC viewer < operator < admin.

### Pipeline semantics

- A search run records all candidates + decisions and creates **one**
  acquisition for the best-scoring accepted candidate (score desc, seeders
  desc, title asc); the rest remain as rescout fallbacks.
- Enqueue identifies torrents exactly: magnet btih or a client-returned id
  under the `rivet` category. No fuzzy matching against unrelated downloads.
- Monitor bounds ghost/paused polling (30 not-found / 60 paused polls →
  needs_review). Verify quarantines failures and records evidence; rescout
  fires once per verify failure budget and is depth-capped per target.
- Dry-run mode simulates download clients and never writes to the filesystem.

## Running

```bash
# Tests (Rust)
cargo test

# Web tests + build
npm -C apps/web install && npm -C apps/web test && npm -C apps/web run build

# Dev: backend :8080 + web :5173 (persists .rivet-secret-key and rivet-dev.db)
make dev

# Dry-run backend (simulated download clients, real indexer calls)
make backend-dry-run        # or: DRY_RUN=1 make backend
```

Environment: `APP_ENV`, `HTTP_ADDR`, `DATABASE_URL`, `APP_SECRET_KEY`
(base64, 32 bytes; required outside dev), `SESSION_TTL`, `ALLOW_INTERNAL_URLS`
(defaults true in dev), `DRY_RUN`, `DRY_RUN_FIXTURE_PATH`, `FFPROBE_PATH`.

## First-run wizard

With no users, only `/api/v1/setup/*` is reachable:

1. **Create admin** — first user becomes admin; the gate flips synchronously
   so the immediate login cannot race it. Optional TOTP two-factor
   (`POST /auth/totp/setup` → `enable`; enabled accounts require a 6-digit code
   at login).
2. **Paths** — downloads/quarantine/library roots validated (stat-only in dry-run).
3. **Download clients** — qBittorrent/Deluge/Transmission + NZBGet/SABnzbd;
   connectivity tested under the SSRF policy; secrets encrypted at rest.
4. **Indexers** — Torznab/Newznab or Prowlarr; test search + parsing.
5. **Metadata** — TMDB/TVDB validation.
6. **Policies** — quality profiles, banned tags, language.
7. **Finish** — writes the setup report and opens the app.

## Docker

```bash
docker build -t rivet:dev .
docker run --rm -p 8080:8080 -v "$PWD/data:/data" \
  -e APP_ENV=prod \
  -e APP_SECRET_KEY="$(python3 -c 'import base64,os;print(base64.b64encode(os.urandom(32)).decode())')" \
  rivet:dev
```

The entrypoint forwards SIGTERM/SIGINT to all three processes and tears the
rest down if any child dies.

## API contract

`packages/contracts/openapi.yaml` documents every endpoint including status
codes. The WebSocket protocol (`/ws`) uses `{v, type, ts, payload}` envelopes;
clients subscribe explicitly (`{"type":"subscribe","channels":[...]}`).

## Security posture

- Argon2id (m=64 MiB, t=3) password hashing; constant-shape login errors with
  dummy verification against timing enumeration.
- AES-256-GCM encryption for all stored credentials via ONE shared SecretBox.
- Per-IP login rate limiting (10/min fixed window).
- Outbound SSRF policy blocks loopback/private/link-local/CGNAT/multicast/
  NAT64 targets unless `ALLOW_INTERNAL_URLS` is set; pinned-IP dialing defeats
  DNS rebinding; redirect hops revalidated.
- Audit events for auth/setup/admin actions; WS event payloads gated by role.
