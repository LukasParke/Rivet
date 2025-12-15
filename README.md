## Media Automation Control Plane (self‑hosted)

A self-hosted, publicly accessible **media automation control plane** that manages discovery, decisioning, acquisition, verification, and import—while delegating actual downloading to mature external clients (qBittorrent/Deluge/Transmission, NZBGet/SABnzbd).

Core goals: **fast**, **explainable**, **secure**, **observable**, great as a **single-container install**, with an easy path to **Postgres + Redis** scaling.

---

## Current implementation status (pick up here)

This repo already contains a working “MVP spine” (API + DB + worker + WS + basic UI). Below is a consolidated snapshot of what is implemented so far and what’s next.

### Implemented (today)

- **Backend service (Go + Fiber)**
  - **Auth + security**: sessions, CSRF for cookie-auth mutations, RBAC (admin/operator/viewer), login rate limiting, Argon2id password hashing, secrets encryption (AES-256-GCM), SSRF defenses for outbound HTTP, audit event recording.
  - **DB**: SQLite via `modernc.org/sqlite` + embedded SQL migrations.
  - **Durable jobs**: DB-backed job queue with leases, retries, exponential backoff + jitter; in-process worker loop.
  - **Observability**: structured JSON logging (`slog`), `/healthz`, `/readyz`, `/metrics`, diagnostics bundle endpoint.
  - **WebSockets**: `/ws` with channel subscriptions; server publishes events for jobs, health, audit, downloads, imports.

- **Setup wizard (API + UI)**
  - Wizard steps exist in the UI and corresponding API endpoints store validated configs in settings:
    - Paths (`setup.paths`): downloads, library roots, quarantine
    - Clients (`setup.clients`): qBittorrent credentials stored encrypted
    - Indexers (`setup.indexers`): Newznab/Torznab configs (stored + validated)
    - Metadata (`setup.metadata`): TMDB/TVDB configs (stored + validated)
    - Policies (`setup.policies`): profile/policy JSON (validated + stored)

- **Discovery → decisioning pipeline**
  - Wanted targets, search runs, release candidates, decisions stored in DB.
  - `search_run` job executes: searches indexers, normalizes candidates, runs decisioning, stores decisions, emits WS events.

- **Acquisition pipeline (qBittorrent MVP)**
  - Unified `acquisitions` record with lifecycle state.
  - `acquisition_enqueue` job:
    - Reads candidate URL (enclosure/link), sends to qBittorrent, extracts `btih` hash from magnet, stores `external_id`, emits `download.enqueued`.
    - Schedules monitoring (`acquisition_monitor`).
  - `acquisition_monitor` job:
    - Polls qBittorrent `/api/v2/torrents/info`, updates progress in DB, emits `download.progress`.
    - On completion: marks acquisition completed, emits `download.completed`, schedules `acquisition_verify`.
    - Stall detection v1: if no progress change within a window, sets `status=stalled`, emits `download.stalled`, schedules `acquisition_recover`.
  - **Verification v0** (`acquisition_verify` job):
    - Picks a media file, applies minimal safety checks (extension allowlist, “sample” detection, minimum size).
    - On failure: quarantines file and marks `status=needs_review`, emits `verify.failed`.
    - On success: marks verified and schedules `acquisition_import`, emits `verify.completed`.
  - **Import v0** (`acquisition_import` job):
    - Hardlink-first import into library root (fallback copy), marks `status=imported`, emits `import.completed`.
  - **Recovery v1** (`acquisition_recover` job, qBittorrent):
    - Attempts: resume → reannounce → recheck, emits `download.recovery_*` events, then resumes monitoring.
    - Escalates to `needs_review` after repeated failures/attempts.

- **API endpoints (implemented)**
  - Auth: `/api/v1/auth/*` (login/logout/me/csrf)
  - Setup: `/api/v1/setup/*`
  - Targets/search: `/api/v1/targets`, `/api/v1/targets/{id}/search`, `/api/v1/search/runs/{id}`
  - Diagnostics: `/api/v1/diagnostics/bundle`
  - Acquisitions:
    - `GET /api/v1/acquisitions`
    - `GET /api/v1/acquisitions/{id}`

- **Web UI (SvelteKit)**
  - Setup wizard pages.
  - `/app` demo page: create target → start search → poll results + live WS feed.
  - Acquisitions view on `/app`: refreshes automatically when `download.*` / `import.*` / `verify.*` events arrive.

### WebSocket event types (implemented)

- **Jobs**: `job.claimed`, `job.completed`, `job.failed`, `job.retry_scheduled`
- **Downloads** (`downloads` channel): `download.enqueued`, `download.progress`, `download.completed`, `download.failed`, `download.stalled`, `download.recovery_attempt`, `download.recovery_scheduled`, `download.recovery_failed`, `download.needs_review`
- **Verification/import** (`imports` channel): `verify.started`, `verify.completed`, `verify.failed`, `import.started`, `import.completed`, `import.failed`
- **Audit** (`audit` channel): `audit.*`
- **Health** (`health` channel): `health.snapshot`

### Job kinds (implemented)

- `search_run`
- `acquisition_enqueue`
- `acquisition_monitor`
- `acquisition_verify`
- `acquisition_import`
- `acquisition_recover`

### Migrations added so far (high-level)

- Core auth/session/audit/settings/jobs tables
- Core domain tables (targets/search runs/candidates/decisions)
- Acquisition tables and follow-on fields:
  - `0004_acquisitions.sql` (base)
  - `0005_acquisitions_fields.sql` (source/content/import/progress fields)
  - `0006_acquisition_verify_stall.sql` (verify/quarantine + stall/recovery fields)

### How to run

- **Go tests**:
  - `go test ./...`
- **Web build**:
  - `npm -C apps/web run build`

---

## Architecture overview

### Components

- **Web UI**: SvelteKit
  - Server-rendered initial data (native server loads)
  - WebSockets for realtime updates
- **API/Engine**: Go + Fiber
  - REST (OpenAPI-first)
  - WebSockets
  - Background workers/controllers
- **Storage profiles**
  - **Default**: SQLite (WAL) on a mounted volume
  - **Scale mode**: Postgres (source of truth)
- **Redis** (optional → recommended in scale mode)
  - Provider call caching
  - Rate-limit state
  - Distributed job leases
  - WebSocket fanout pub/sub
- **External reverse proxy**: handled outside the app (no TLS concerns inside app)

### Core design principles

- **Declarative desired state** (what you want) + **reconciliation loops** (controllers) that continuously drive reality toward that state.
- **Durable job orchestration** (DB-backed queue) with leases, retries, jittered backoff, and explainable outcomes.
- **Client-agnostic acquisition model** that unifies torrents + usenet under one state machine.
- **Security-first** for internet-exposed installs: RBAC, audit logging, SSRF defenses, rate limiting, secret encryption.
- **Observability-first**: OpenTelemetry traces/metrics/logs, diagnostics bundle, health dashboards.

---

## First-run registration + guided setup wizard (required)

### Setup Mode gate

If **no users exist**, the site enters **Setup Mode**:

- Only wizard routes are accessible
- The first created user becomes **Admin**
- Registration **closes by default** after setup (admin can enable invites later)

### Wizard steps (with real validations)

1. **Create Admin**
   - Email/password (Argon2id)
   - Optional TOTP
2. **Paths**
   - Downloads completed path
   - Library roots
   - Quarantine path
   - Validate permissions + hardlink viability (same filesystem)
3. **Download clients**
   - Configure at least one torrent client: qBittorrent (recommended) or Deluge/Transmission
   - Usenet: NZBGet (required) + SABnzbd (recommended)
   - Test connectivity; store secrets encrypted
4. **Indexers**
   - Torznab (torrents), Newznab (usenet)
   - Test search + parsing
5. **Metadata providers**
   - TMDB/TVDB etc.
   - Test and set request budgets
6. **Policies**
   - Quality profiles, language, upgrade rules, safety gates
7. **Finish**
   - Health summary
   - Store “setup report”
   - Unlock full UI

Validations are not cosmetic: connectivity, permissions, basic search/parse, plus a sample end-to-end **dry run**.

---

## Integrations and adapters (v1 set)

### Indexers (avoid one-off integrations)

- **Torznab adapter** (torrent searching)
- **Newznab adapter** (usenet searching)

This supports Prowlarr-style setups without tightly coupling to Prowlarr.

### Download clients (native support)

All integrations implement a shared **Downloader** interface plus optional capability interfaces (e.g., torrent reannounce/recheck; usenet retry/postprocess).

- **Torrents**: qBittorrent (default), Deluge, Transmission
- **Usenet**: NZBGet (first-class), SABnzbd (strongly recommended)

---

## Core domain model

### Key objects

- **Library**: kind (movies/tv), root paths, import mode, quarantine path
- **Profile/Policy**: hard filters + scoring weights + upgrade policy
- **Wanted Target**: desired item (movie, episode, season pack)
- **Release Candidate**: indexer result (raw + normalized parse)
- **Decision**: accepted/rejected with score breakdown and reasons
- **Acquisition**: unified record (torrent or usenet) + lifecycle
- **Verification**: post-download correctness + safety outcomes
- **Asset**: imported file on disk + hash + media info
- **Jobs**: durable queue items powering controllers/workflows
- **Audit Events**: privileged actions + sensitive system actions

---

## System workflows (end-to-end)

### Standard flow

1. Target created (user adds wanted item, or via list import)
2. Search via Torznab/Newznab
3. Normalize titles into structured metadata (resolution/source/codec/etc.)
4. Filter + score using profile rules
5. Decision recorded (explainable report stored)
6. Queue download in chosen client (torrent or usenet)
7. Monitor progress (polling clients) → realtime UI via WS
8. Stall detection runs continuously
9. On completion → verify (safety + correctness)
10. Import (hardlink/copy/move) + rename + folder layout
11. Update target state (imported) + notify
12. Periodic reconciliation (repair/missing/moved)

Everything is **idempotent** and **resumable**.

---

## Torrent/NZB selection and validation

### Pre-grab filtering (cheap/fast)

- Parse + normalize release title
- Hard rejects
  - Too few seeders (torrent)
  - Size outside expected band
  - Banned tags (cam/ts/scr/etc.)
  - Target mismatch (wrong season/episode/year)
- Soft scoring
  - Quality tier, source, codec/audio prefs
  - Group reputation (locally learned)
  - Age/freshness, size closeness

Always store **decision reasons** + **score breakdown**.

### Post-download verification (deep/trustworthy)

- Media-only import policy (default)
  - Only allowlisted media extensions are importable
  - Everything else quarantined/ignored (configurable)
- Inspect streams (ffprobe/mediainfo)
  - Duration sanity bounds
  - Resolution/codec/audio checks
  - Detect samples and ignore
- Identity match
  - TV: SxxEyy/date mapping; season-pack completeness rules
  - Movies: title similarity + year tolerance
- Integrity
  - Hash (blake3 recommended) for dedupe + avoiding loops
- Optional malware scanning
  - ClamAV sidecar scanning for suspicious non-media payloads

Failures go to a **Review Queue** with full evidence and override options (admin-only, audited).

---

## Native stalled-download handling + rescout

### Unified acquisition state machine

Queued → Downloading → Completed → Verifying → Imported

With side states: Degraded, Stalled, Failed, NeedsReview, Blacklisted.

### Stall detection signals (composite)

- No progress delta in bytes for a configured window
- Download rate ~ 0
- Torrent: peers/seeders collapse, tracker errors
- Usenet: missing articles/server failures/postprocess stuck

### Recovery ladder (policy-driven, rate-limited)

- Torrents
  1. Resume
  2. Reannounce (if supported)
  3. Force recheck (if supported)
  4. Rescout alternatives
  5. Fail → NeedsReview (with logs + reasons)
- Usenet
  1. Resume/retry
  2. Dependency unhealthy → stop hammering, alert
  3. Postprocess loops → quarantine/NeedsReview
  4. Rescout alternative
  5. Fail

Every action is recorded (recovery history), emitted via WS, and audited.

---

## API + WebSockets

### REST (OpenAPI-first)

- Auth/session/token management
- Wizard validation endpoints
- Libraries/targets/profiles
- Search runs + decisions + blacklists
- Acquisition controls (pause/resume/remove/retry)
- Verification/import/review queue actions
- Health/diagnostics

### WebSockets (`/ws`)

- Authenticated
- Channels: activity, jobs, downloads, imports, health, audit
- Versioned event envelope: `{ v, type, ts, payload }`
- Scale mode: Redis pub/sub for fanout

---

## Observability (required)

### OpenTelemetry

- **Traces**: REST + WS messages + provider calls + job runs (single trace across pipeline)
- **Metrics**: queue depth, job latency, provider error rates, import throughput, WS connections
- **Logs**: structured JSON with trace/span IDs

### Endpoints

- `/healthz`
- `/readyz`
- `/metrics`

### Diagnostics bundle

Sanitized settings + last N errors + health snapshot + top reject reasons.

---

## Test strategy (required)

### Unit + integration

- **Unit tests**: pure logic (parsing/decisioning/scoring/state machine), storage helpers, SSRF guards, crypto helpers.
- **Integration tests** (real external providers): run against public APIs when credentials are provided via env; otherwise **auto-skip**.
  - TMDB: set `TMDB_API_KEY`
  - TVDB: set `TVDB_API_KEY`
  - (extend similarly for other providers as they’re added)

These tests validate the real request/response contract, rate-limit handling, and auth failures without requiring a full deployment.

---

## Security (publicly accessible by design)

- **RBAC**: admin/operator/viewer
- **Audit log**: privileged + sensitive operations
- **Secrets encrypted at rest**: integration credentials
- **SSRF defenses**: block internal ranges unless explicitly allowed
- **Strict CORS allowlist**
- **CSRF protection**: for cookie-auth mutations
- **Rate limiting**: login, WS connect, search/rescout triggers
- **Secure session cookies**: HTTPOnly; SameSite chosen for deployment model
- **Safe-by-default mounts**
  - Recommend host-mounted SMB/NFS bind-mounted into container
  - Optional privileged mount helper sidecar later (not in core)

---

## Repo layout (target)

- `apps/web`: SvelteKit UI
- `cmd/server`: API + WS + workers (default)
- `cmd/worker`: optional scale mode worker
- `cmd/migrate`: migrations + sqlite→pg tool
- `internal/adapters`: downloaders/indexers/metadata
- `internal/jobs`: queue + runners (search/decide/monitor/recover/rescout/verify/import)
- `internal/import`: verifier/organizer/scanner
- `internal/security`: auth/RBAC/audit/secrets/SSRF/rate-limit
- `internal/observability`: OTel, logging, metrics, diagnostics
- `packages/contracts`: OpenAPI + WS events codegen

---

## Delivery milestones

### MVP

Wizard + qBittorrent + Torznab + one domain + verify/import + WS feed + stall recovery v1.

### Next (recommended order)

- **Review queue UI + admin actions** (needs_review acquisitions: view evidence, re-run verify/import, manual override; audited)
- **Acquisition controls API** (pause/resume/remove/retry) and wire to qBittorrent controls
- **Better verification** (ffprobe/mediainfo, sample detection beyond filename, hash/dedupe)
- **Rename + folder layout** (movie vs TV rules, year tolerance, season/episode formatting)
- **Rescout v1** (when stalled/failed: search alternatives and re-enqueue)

### v1

Add Deluge/Transmission + NZBGet/SABnzbd + Newznab + quarantine/review + rescout + Redis scale mode.

### vNext

Multi-domain, repair/reconcile, richer decision tuning, more metadata/list providers.
