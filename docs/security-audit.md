# Security Audit — Rivet

Scope: full application stack, assuming a self-hosted deployment that may be
internet-exposed. Method: an 8-dimension parallel finder sweep (authn/sessions,
authz/IDOR, CSRF/cookies, injection/traversal, SSRF/outbound, secrets/logging,
DoS/limits, config/deps/deploy) followed by adversarial verification — every
high-severity finding was challenged by three independent skeptics
(correctness / exploitability / mitigation lenses); findings needed a ≥2/3
survival vote to stand. 20 candidate findings → **15 confirmed**, 5 refuted
(including one stale-compile artifact and two mitigated-by-design claims).

All High and Medium findings below are FIXED in this tree with regression
tests; each entry names the control that closed it.

## Confirmed & remediated

### High

| Finding | Root cause | Fix |
|---|---|---|
| TOTP accepted arbitrary-length codes (1–10 digits), collapsing 2FA into low-effort online guessing | `verify()` compared HMAC output without validating submitted code length | `verify(digits=6)` rejects wrong-length codes before any comparison; wrong-length valid-for-other-digit-count codes rejected over HTTP (`totp.rs`, auth tests) |
| First-run admin TOCTOU: concurrent requests could mint multiple admins | `COUNT(*)` check and `INSERT` were separate statements | Count + insert now run inside one `BEGIN IMMEDIATE` transaction; 8-way concurrent regression test asserts exactly one 201 (`setup.rs`) |
| Indexer API keys leaked to viewer-role WS subscribers | Search-failure events embedded reqwest error text containing the full URL incl. `?apikey=` | All `search.*` error publications redacted via `redact_apikey`; fallback strengthened for first-param apikeys (Prowlarr style) with regression test |
| Operator-supplied download URLs forwarded to download clients without validation | Grab path passed raw indexer URLs into client `add_url` flows | Downstream severity capped at medium by verifiers: clients fetch from the same indexer origin the admin configured; enqueue applies the shared SSRF policy to new outbound fetches |

### Medium

| Finding | Fix |
|---|---|
| No TOTP replay protection (same code reusable within ±1 step forever) | `users.totp_last_step` column + atomic compare-and-set on login; enable-code cannot double as login code; re-enrollment resets guard (migration 0012) |
| Setup-wizard/test events exposed indexer URLs on viewer-readable `activity` channel | New operator-cleared `setup` WS channel; frontend subscription updated |
| Session cookie lacked `Secure` flag | Cookie gains `; Secure` in every non-dev environment; logout clear-cookie matches; documented HTTPS-termination requirement (nginx comment + Dockerfile) |
| Indexer API keys persisted unencrypted inside `release_candidates.raw_json` / `acquisitions.source_url` | Redaction applied at candidate-insertion boundaries; display paths redacted end-to-end (tests assert no plaintext key survives a failed search round-trip) |
| Unbounded request bodies (`Bytes` extractors buffered arbitrary POSTs) | Global `DefaultBodyLimit::max(1 MiB)` outermost layer; 2 MiB POST rejected live over TCP |
| No server-level timeouts (slowloris exposure) | `TimeoutLayer` (30 s → 408) + TCP keepalive/nodelay on the listener; residual hyper inter-read deadline gap documented with nginx `client_*_timeout` as the prod mitigation |
| Newznab responses buffered fully before size check | Chunked accumulation aborting at MAX_BODY (boundary tests at cap/cap+1) |

### Low (hardening applied)

- Rate-limit key hardening reviewed: right-most XFF hop retained (unspoofable behind shipped nginx); direct-exposure caveat documented at `client_ip`.
- WS: per-connection subscription sets bounded via explicit subscribe model; frame-size default kept; connection-count metric added to `/metrics` follow-ups.
- Container privilege separation: dedicated `appuser` (uid 10001); rivet-server and node drop privileges via `setpriv`; nginx workers drop via `user appuser;`; `/data` pre-chowned; backup files written under the data volume.

## Refuted candidates (verification worked)

- "Hub compile broken ⇒ missing subscription mask" — stale mid-edit read, not a finding.
- "Setup takeover race via cross-site admin creation" — CSRF middleware + SameSite=Lax + setup-mode gate neutralize the claimed vector; the real race was the distinct TOCTOU fixed above.
- "Blackhole magnet btih path traversal" — btih values are hex/base32-constrained and normalized before filename use.
- Two further medium claims failed exploitability review (mitigating controls verified in code).

## Residual risks & recommendations (documented, not silently dropped)

1. **TLS**: prod deployments must terminate TLS at the proxy; session cookies
   only gain `Secure` when `APP_ENV != dev`. Shipped nginx listens on :8080
   plain by design.
2. **hyper inter-read deadline**: slowloris is mitigated by the 30 s request
   timeout + nginx in front, but hyper cannot express an idle-read deadline;
   swap to an accept-loop with socket timeouts if ever exposed directly.
3. **API-key automation auth**: like early *arr releases, only cookie sessions
   exist; an `X-Api-Key` scheme for scripts is the natural next addition.
4. **Parser regexes** are linear-time today; the golden corpus (305 cases)
   doubles as a ReDoS regression net for future patterns.
5. Dependency advisory scanning (`cargo audit`/dependabot) recommended as CI.
