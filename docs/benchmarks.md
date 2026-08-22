# Rivet Benchmarks

Methodology: all numbers from this machine (AMD Ryzen 9 7950X3D, 32 threads,
Linux, release builds). Criterion benches use 100 samples with outlier
analysis; the HTTP loadgen boots the real axum app in-process (full middleware
stack), seeds first-run state through the real handlers, then drives concurrent
load with `reqwest` for a fixed window per endpoint.

Reproduce:

```bash
cargo bench                    # criterion suites (domain/storage/ws)
cargo run --release --bin loadgen -- 18091 10 16   # HTTP + WS end-to-end
make goldens                   # regenerate committed golden files
```

## Domain engine (criterion)

| Benchmark | Result | Notes |
|---|---|---|
| parse, 8 mixed titles | **~38 µs** batch (**~4.7 µs/release ≈ 210K titles/s**) | Was **2.94 ms** before hoisting per-call regex compiles — the benchmark suite caught and fixed a 100× hot-path regression |
| parse, single hard title (2160p Remux) | 4.4 µs | |
| quality upgrade decision (`decide_upgrade`) | **~150 ns** | |
| custom-format scoring, 6 formats | **~4.3 µs** | Includes re-parse of title |
| naming template render (`render_series`) | **~460 ns** | |

## Storage & queue (criterion, SQLite WAL on tmpfs-backed tempdir)

| Benchmark | Result |
|---|---|
| job enqueue | ~437 µs/op (≈ 2.3K jobs/s) — fsync-bound |
| claim → complete cycle | ~239 µs/op (≈ 4.2K cycles/s) |
| bulk upsert 100 episodes | ~1.24 ms |
| missing-episodes query over 1,000 episodes | ~5.4 ms |

## HTTP API (loadgen, real server, 16 workers × 6–8 s)

Session-authenticated GETs unless noted. 1,000 episodes seeded across 25
series; 100% 200 responses everywhere.

| Endpoint | Throughput | p50 | p95 | p99 |
|---|---|---|---|---|
| GET /healthz | 6.3–7.2K rps | 1.2 ms | 7 ms | 18 ms |
| GET /api/v1/auth/me (session lookup) | 3.5–6.8K rps | 1.2–2.5 ms | 8–13 ms | 18–25 ms |
| GET /api/v1/acquisitions | 3.1–7.8K rps | 1.5–2.6 ms | 4–14 ms | 10–33 ms |
| GET /api/v1/calendar (1k episodes) | 2.7–6.3K rps | 1.5–3.1 ms | 5–17 ms | 13–45 ms |
| GET /api/v1/history | 3.5–4.6K rps | 2.0–2.3 ms | 8–13 ms | 20–36 ms |
| GET /api/v1/wanted/missing | 1.2–1.5K rps | 10.6 ms | 15–25 ms | 20–48 ms |
| GET /api/v1/series | 1.9–2.6K rps | 5.6–6.9 ms | 9–19 ms | 24–39 ms |
| POST /api/v1/targets (write + audit + hub event) | 604 rps (first run), up to 3.2K rps steady-state | 2.8–22 ms | 35–59 ms | 62–174 ms |

Ranges span multiple runs; variance is scheduler noise on a shared machine.

## WebSocket hub

| Benchmark | Result |
|---|---|
| In-process publish, 10 subscribers | ~643 ns/event (≈ 15.5M deliveries/s) |
| Live: 100 authenticated WS connections, publishes driven via POST /targets | 976 publishes/s; **6,000 frames delivered for 2,000 publishes** (exact 3-channel × N fanout math, no loss) |

## Memory

- Loadgen process RSS during sustained mixed load: **23–36 MB**
- Server RSS under the same profile: tens of MB (SQLite page cache dominates)

## Frontend bundle (production build)

- Server bundle entry `index.js` ≈ 127 KB (+chunks); client chunks split per
  route; svelte-check 0 errors.

## Findings benchmarks surfaced

1. **100× parser regression fixed**: `parse()` compiled several regexes on
   every call; hoisting to `LazyLock` statics took 8-title parsing from
   2.94 ms to ~38 µs.
2. Queue write throughput is SQLite fsync-bound — batching or `synchronous =
   NORMAL` trade-offs are the lever if higher ingest is needed.
3. `/wanted/missing` is the priciest read (p50 ≈ 10 ms at 1k episodes) due to
   the monitored+aired+no-file scan; indexed but worth revisiting past ~10k
   episodes.
