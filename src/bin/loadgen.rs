//! End-to-end HTTP load generator for the real Rivet router.
//!
//! Boots the actual axum app (full middleware stack) on a local port, seeds a
//! first-run admin + session, then drives concurrent load against key
//! endpoints and reports latency percentiles + throughput.
//!
//! Run: cargo run --release --bin loadgen [-- port duration_secs concurrency]

use axum::body::Body;
use axum::http::Request;
use rivet::config::Config;
use rivet::httpapi;
use rivet::metrics::Metrics;
use rivet::security::secrets::SecretBox;
use rivet::security::OutboundPolicy;
use rivet::state::AppState;
use rivet::ws::hub::Hub;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const TEST_KEY: &str = "BwYFBAMCAQAPDg0MCwoJCBcWFRQTEhEqKCYmJiYmJiY=";

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(18090);
    let duration_secs: u64 = std::env::args()
        .nth(2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(10);
    let concurrency: usize = std::env::args()
        .nth(3)
        .and_then(|p| p.parse().ok())
        .unwrap_or(16);

    // --- Boot the real app ---------------------------------------------------
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!("file://{}", dir.path().join("load.db").display());
    let db = rivet::storage::open(&dsn).await.unwrap();
    rivet::storage::migrations::apply(&db).await.unwrap();
    let cfg = Config {
        env: "bench".into(),
        http_addr: format!("127.0.0.1:{port}"),
        database_url: dsn,
        dry_run: true,
        dry_run_fixture_path: None,
        ffprobe_path: None,
        secret_key_base64: Some(TEST_KEY.to_string()),
        session_cookie_name: "rivet_session".into(),
        session_ttl: chrono::Duration::hours(1).to_std().unwrap(),
        allow_internal_urls: true,
    };
    let state = AppState {
        db: db.clone(),
        secret_box: Arc::new(SecretBox::new(cfg.secret_key_base64.as_deref()).unwrap()),
        policy: OutboundPolicy::dev(),
        hub: Arc::new(Hub::new()),
        metrics: Arc::new(Metrics::default()),
        config: Arc::new(cfg.clone()),
    };
    let gate = rivet::httpapi::middleware::setup_gate::init_shared(
        db.clone(),
        std::time::Duration::from_secs(2),
    )
    .await;
    let app = httpapi::build_router(state.clone(), Arc::clone(&gate));

    // Serve for real so the load driver exercises TCP + HTTP stack.
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind");
    {
        let serve_app = app.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, serve_app.into_make_service()).await;
        });
    }

    // --- Seed first-run state through the real handlers ----------------------
    async fn seed(
        app: axum::Router,
        method: &'static str,
        uri: String,
        headers: Vec<(&'static str, String)>,
        body: Option<String>,
    ) -> axum::response::Response {
        let mut b = Request::builder().method(method).uri(uri);
        for (k, v) in &headers {
            b = b.header(*k, v);
        }
        let req = b.body(Body::from(body.unwrap_or_default())).unwrap();
        app.oneshot(req).await.unwrap()
    }
    let res = seed(app.clone(),
        "POST",
        "/api/v1/setup/admin".into(),
        vec![("content-type", "application/json".into())],
        Some(json!({"email":"admin@bench.test","password":"correct-horse-battery"}).to_string()),
    )
    .await;
    assert_eq!(res.status(), 201);
    let res = seed(app.clone(),
        "POST",
        "/api/v1/auth/login".into(),
        vec![("content-type", "application/json".into())],
        Some(json!({"email":"admin@bench.test","password":"correct-horse-battery"}).to_string()),
    )
    .await;
    let set_cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let cookie = set_cookie
        .split(';')
        .next()
        .unwrap_or_default()
        .to_string();
    let res = seed(app.clone(), "GET", "/api/v1/auth/csrf".into(), vec![("cookie", cookie.clone())], None).await;
    let csrf_bytes = axum::body::to_bytes(res.into_body(), 8192).await.unwrap();
    let csrf: String = serde_json::from_slice::<serde_json::Value>(&csrf_bytes)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let res = seed(app.clone(),
        "POST",
        "/api/v1/setup/paths/save".into(),
        vec![
            ("content-type", "application/json".into()),
            ("cookie", cookie.clone()),
            ("x-csrf-token", csrf.clone()),
        ],
        Some(
            json!({"downloadsCompletedPath":"/tmp/dl","quarantinePath":"/tmp/q","libraryRoots":["/tmp/lib"]})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(res.status(), 200);
    let res = seed(app.clone(),
        "POST",
        "/api/v1/setup/finish".into(),
        vec![("cookie", cookie.clone()), ("x-csrf-token", csrf.clone())],
        None,
    )
    .await;
    assert_eq!(res.status(), 200);

    // Seed library data: 25 series × 40 episodes = 1000 rows.
    for i in 0..25u32 {
        let (series, _) = rivet::storage::series::upsert_series(
            &db,
            &rivet::storage::series::SeriesUpsert {
                id: None,
                tvdb_id: Some(i as i64),
                tmdb_id: None,
                imdb_id: None,
                title: format!("Bench Series {i}"),
                sort_title: None,
                status: "continuing".into(),
                overview: Some("".into()),
                network: None,
                first_aired: None,
                genres: vec![],
                path: format!("/tv/bench{i}"),
                quality_profile: "default".into(),
                monitored: true,
                season_folder: true,
                tags: vec![],
            },
        )
        .await
        .unwrap();
        for season in 1..=2u32 {
            let batch: Vec<rivet::storage::series::EpisodeUpsert> = (1..=20u32)
                .map(|e| rivet::storage::series::EpisodeUpsert {
                    season_number: season as i64,
                    episode_number: e as i64,
                    absolute_episode_number: Some(((season - 1) * 20 + e) as i64),
                    title: Some(format!("Episode {e}")),
                    air_date_utc: Some("2026-01-01T00:00:00Z".into()),
                    runtime: Some(42),
                    overview: None,
                    monitored: true,
                })
                .collect();
            rivet::storage::series::bulk_upsert_episodes(&db, &series.id, &batch)
                .await
                .unwrap();
        }
    }

    // --- Load driver ---------------------------------------------------------
    let endpoints: Vec<(&str, String)> = vec![
        ("GET /healthz", "/healthz".into()),
        ("GET /api/v1/auth/me", "/api/v1/auth/me".into()),
        ("GET /api/v1/series", "/api/v1/series".into()),
        ("GET /api/v1/acquisitions", "/api/v1/acquisitions".into()),
        ("GET /api/v1/calendar", "/api/v1/calendar".into()),
        ("GET /api/v1/wanted/missing", "/api/v1/wanted/missing?pageSize=50".into()),
        ("GET /api/v1/history", "/api/v1/history?pageSize=50".into()),
    ];

    println!(
        "rivet loadgen: {concurrency} workers × {duration_secs}s per endpoint, session-authenticated\n"
    );
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "endpoint", "reqs", "rps", "p50(ms)", "p95(ms)", "p99(ms)"
    );

    let client: Arc<reqwest::Client> = Arc::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");

    for (label, path) in &endpoints {
        let url = format!("{base}{path}");
        let mut latencies = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(duration_secs);
        let mut handles = Vec::new();
        for _ in 0..concurrency {
            let client = Arc::clone(&client);
            let url = url.clone();
            let cookie = cookie.clone();
            handles.push(tokio::spawn(async move {
                let mut lat = Vec::new();
                while Instant::now() < deadline {
                    let t0 = Instant::now();
                    let resp = client
                        .get(&url)
                        .header("cookie", &cookie)
                        .send()
                        .await
                        .expect("request");
                    let status = resp.status().as_u16();
                    let _ = resp.bytes().await;
                    lat.push((t0.elapsed().as_micros() as u64, status));
                }
                lat
            }));
        }
        for h in handles {
            latencies.extend(h.await.unwrap());
        }
        latencies.sort();
        let n = latencies.len();
        if n == 0 {
            println!("{label:<28} no requests");
            continue;
        }
        let total_ms = duration_secs as f64;
        let rps = n as f64 / total_ms;
        let pct = |p: f64| latencies[((n as f64) * p).min((n - 1) as f64) as usize].0 as f64 / 1000.0;
        let ok = latencies.iter().filter(|(_, s)| *s == 200).count();
        println!(
            "{label:<28} {n:>10} {rps:>10.0} {:>10.2} {:>10.2} {:>10.2}   ({}% 200)",
            pct(0.50),
            pct(0.95),
            pct(0.99),
            100 * ok / n
        );
    }

    // Authenticated POST (full CSRF + session middleware path).
    let url = format!("{base}/api/v1/targets");
    let mut latencies = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut handles = Vec::new();
    for w in 0..concurrency {
        let client = Arc::clone(&client);
        let cookie = cookie.clone();
        let csrf = csrf.clone();
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            let mut i = 0u64;
            while Instant::now() < deadline {
                i += 1;
                let t0 = Instant::now();
                let resp = client
                    .post(&url)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .json(&json!({
                        "kind": "series",
                        "externalSource": "tmdb",
                        "externalId": format!("bench-{w}-{i}"),
                        "query": format!("bench {w} {i}"),
                    }))
                    .send()
                    .await
                    .expect("request");
                let status = resp.status().as_u16();
                let _ = resp.bytes().await;
                lat.push((t0.elapsed().as_micros() as u64, status));
            }
            lat
        }));
    }
    for h in handles {
        latencies.extend(h.await.unwrap());
    }
    latencies.sort();
    let n = latencies.len();
    let rps = n as f64 / duration_secs as f64;
    let pct = |p: f64| latencies[((n as f64) * p).min((n - 1) as f64) as usize].0 as f64 / 1000.0;
    println!(
        "{:<28} {n:>10} {rps:>10.0} {:>10.2} {:>10.2} {:>10.2}",
        "POST /api/v1/targets",
        pct(0.50),
        pct(0.95),
        pct(0.99)
    );

    // --- Live WebSocket fanout ----------------------------------------------
    {
        use futures_util::StreamExt;
        let ws_clients: usize = 100;
        let events_to_send: usize = 2000;
        let mut conns = Vec::new();
        for _ in 0..ws_clients {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut req = format!("ws://127.0.0.1:{port}/ws")
                .into_client_request()
                .expect("ws request");
            req.headers_mut().insert(
                "cookie",
                axum::http::HeaderValue::from_str(&cookie).unwrap(),
            );
            let (ws, _) = tokio_tungstenite::connect_async(req)
                .await
                .expect("ws connect");
            conns.push(ws);
        }
        // All connections auto-subscribed to activity/jobs/health defaults.
        let t0 = Instant::now();
        for i in 0..events_to_send {
            // Publish via a tiny HTTP-free path: reuse the hub directly is not
            // possible cross-process, so drive through an endpoint that
            // publishes: creating targets publishes test.target.created.
            let _ = client
                .post(format!("{base}/api/v1/targets"))
                .header("cookie", &cookie)
                .header("x-csrf-token", &csrf)
                .json(&json!({
                    "kind": "series",
                    "externalSource": "tmdb",
                    "externalId": format!("wsfan-{i}"),
                    "query": format!("ws fan {i}"),
                }))
                .send()
                .await
                .unwrap();
        }
        let publish_elapsed = t0.elapsed();
        // Drain: count delivered frames across all connections for 2s.
        let mut delivered = 0usize;
        let deadline = Instant::now() + Duration::from_secs(2);
        'drain: while Instant::now() < deadline {
            for ws in conns.iter_mut() {
                if let Ok(Some(Ok(_msg))) =
                    tokio::time::timeout(Duration::from_millis(5), ws.next()).await
                {
                    delivered += 1;
                }
                if delivered >= events_to_send * 3 {
                    break 'drain;
                }
            }
        }
        let fanout_rps = events_to_send as f64 / publish_elapsed.as_secs_f64();
        println!(
            "WS fanout: {ws_clients} connections, {events_to_send} publishes in {:.2}s ({fanout_rps:.0} publishes/s), {delivered} frames delivered during drain",
            publish_elapsed.as_secs_f64()
        );
    }

    // RSS capture.
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let rss: String = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())
        .unwrap_or_else(|| "?".into());
    println!("\nloadgen process RSS: {rss} kB");
}
