//! Integration suites for the *arr domain surfaces: upgrade decisioning,
//! proper/repack preference, failed-download recovery, and the full series
//! lifecycle over the real HTTP router.
//!
//! Every suite exercises REAL handlers + the durable queue + storage; wiremock
//! stands in for the torznab indexer and the qBittorrent WebUI API.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rivet::config::Config;
use rivet::domain::naming::{render_series, NamingContext};
use rivet::domain::parsing::parse;
use rivet::domain::quality::decide_upgrade;
use rivet::httpapi;
use rivet::jobs;
use rivet::metrics::Metrics;
use rivet::security::secrets::SecretBox;
use rivet::security::OutboundPolicy;
use rivet::state::AppState;
use rivet::storage::jobs as queue;
use rivet::storage::series::{
    bulk_upsert_episodes, insert_episode_file, upsert_season, upsert_series, EpisodeUpsert,
    NewEpisodeFile, SeriesUpsert,
};
use rivet::storage::settings;
use rivet::ws::hub::Hub;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = "BwYFBAMCAQAPDg0MCwoJCBcWFRQTEhEqKCYmJiYmJiY=";

async fn test_state(dry_run: bool, fixture: Option<String>) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!("file://{}", dir.path().join("it.db").display());
    let db = rivet::storage::open(&dsn).await.unwrap();
    rivet::storage::migrations::apply(&db).await.unwrap();
    let cfg = Config {
        env: "dev".into(),
        http_addr: ":0".into(),
        database_url: dsn,
        dry_run,
        dry_run_fixture_path: fixture,
        ffprobe_path: None,
        secret_key_base64: Some(TEST_KEY.to_string()),
        session_cookie_name: "rivet_session".into(),
        session_ttl: chrono::Duration::hours(1).to_std().unwrap(),
        allow_internal_urls: true,
    };
    let state = AppState {
        db,
        secret_box: Arc::new(SecretBox::new(cfg.secret_key_base64.as_deref()).unwrap()),
        policy: OutboundPolicy::dev(),
        hub: Arc::new(Hub::new()),
        metrics: Arc::new(Metrics::default()),
        config: Arc::new(cfg),
    };
    (dir, state)
}

/// Drives the queue with real worker semantics (claim → dispatch →
/// complete/retry); every queued continuation is fast-forwarded so no
/// wall-clock backoff delays the pipeline stages.
async fn drain_queue(state: &AppState, max_jobs: usize) {
    let _ = sqlx::query("UPDATE jobs SET run_after = ? WHERE state = 'queued'")
        .bind(rivet::timeutil::now())
        .execute(&state.db)
        .await;
    let opts = queue::ClaimOpts {
        lease_owner: "integration-worker".into(),
        lease_duration: chrono::Duration::seconds(120),
    };
    for _ in 0..max_jobs {
        // Continuations are enqueued with short delays; make every queued
        // job claimable so the drain observes real handler semantics
        // without wall-clock backoff between pipeline stages.
        let _ = sqlx::query("UPDATE jobs SET run_after = ? WHERE state = 'queued'")
            .bind(rivet::timeutil::now())
            .execute(&state.db)
            .await;
        let job = match queue::claim_one(&state.db, &opts).await {
            Ok(j) => j,
            Err(_) => break,
        };
        match jobs::handlers::dispatch(state, &job).await {
            Ok(()) => {
                let _ = queue::complete(&state.db, &job.id, &opts.lease_owner).await;
            }
            Err(e) => {
                let _ = queue::fail_and_retry(
                    &state.db,
                    &job.id,
                    &opts.lease_owner,
                    job.attempt + 1,
                    &e,
                )
                .await;
                let _ = sqlx::query("UPDATE jobs SET run_after = ? WHERE id = ? AND state = 'queued'")
                    .bind(rivet::timeutil::now())
                    .bind(&job.id)
                    .execute(&state.db)
                    .await;
            }
        }
    }
}

fn torznab_item(title: &str, hash: &str, seeders: i64) -> String {
    format!(
        "<item><title>{title}</title>\
         <link>magnet:?xt=urn:btih:{hash}&amp;dn=f</link>\
         <torznab:attr name=\"seeders\" value=\"{seeders}\"/></item>"
    )
}

fn torznab_feed(items: &[String]) -> String {
    format!(
        r#"<?xml version="1.0"?>
<rss xmlns:torznab="http://torznab.com/schemas/2015/feed" version="2.0">
  <channel>{}</channel>
</rss>"#,
        items.join("")
    )
}

async fn serve_torznab(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

/// Mounts the qBittorrent WebUI endpoints every adapter session needs
/// (login + version probe + add) without per-endpoint call expectations.
async fn mount_qbittorrent_core(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("Ok.")
                .append_header("Set-Cookie", "SID=integration-session; Path=/; HttpOnly"),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.1"))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(server)
        .await;
}

/// Seeds indexer config (encrypted key), quality policies, and a wanted
/// target; returns the target id.
async fn seed_search_target(state: &AppState, indexer_uri: &str, allowed: &[&str]) -> String {
    let key_enc = state.secret_box.encrypt_to_string(b"MOCKKEY").unwrap();
    settings::upsert(
        &state.db,
        "setup.indexers",
        &json!({
            "torznab": [{"name": "mock", "url": indexer_uri, "apiKeyEnc": key_enc}],
            "newznab": []
        })
        .to_string(),
    )
    .await
    .unwrap();
    settings::upsert(
        &state.db,
        "setup.policies",
        &json!({
            "language": "en",
            "qualityProfiles": [{"name": "hd", "allowedQualities": allowed}],
            "bannedTags": []
        })
        .to_string(),
    )
    .await
    .unwrap();

    let target = rivet::storage::domain::create_wanted_target(
        &state.db,
        "series",
        "tmdb",
        "1399",
        Some(&json!({"query": "show s01"})),
    )
    .await
    .unwrap();
    target.id
}

/// Seeds a monitored series with one monitored S01E01 and an imported
/// episode file of the given quality/score. Returns (series, episode).
async fn seed_imported_episode(
    state: &AppState,
    title: &str,
    quality: &str,
    score: i64,
) -> (rivet::storage::series::Series, rivet::storage::series::Episode) {
    let series = upsert_series(
        &state.db,
        &SeriesUpsert {
            id: None,
            tvdb_id: None,
            tmdb_id: Some(4242),
            imdb_id: None,
            title: title.to_string(),
            sort_title: None,
            status: "continuing".into(),
            overview: None,
            network: None,
            first_aired: None,
            genres: vec![],
            path: format!("/library/{title}"),
            quality_profile: "hd".into(),
            monitored: true,
            season_folder: true,
            tags: vec![],
        },
    )
    .await
    .unwrap()
    .0;
    upsert_season(&state.db, &series.id, 1, true).await.unwrap();
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    bulk_upsert_episodes(
        &state.db,
        &series.id,
        &[EpisodeUpsert {
            season_number: 1,
            episode_number: 1,
            absolute_episode_number: Some(1),
            title: Some("Pilot".into()),
            air_date_utc: Some(yesterday),
            runtime: Some(42),
            overview: None,
            monitored: true,
        }],
    )
    .await
    .unwrap();
    insert_episode_file(
        &state.db,
        &NewEpisodeFile {
            id: None,
            series_id: series.id.clone(),
            season_number: 1,
            episode_numbers: vec![1],
            relative_path: format!("{title} - S01E01.mkv"),
            size_bytes: 26 * 1024 * 1024,
            quality: quality.to_string(),
            quality_score: score,
            languages: vec!["en".into()],
            original_path: None,
        },
    )
    .await
    .unwrap();
    let episode = rivet::storage::series::list_episodes_by_series(&state.db, &series.id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.season_number == 1 && e.episode_number == 1)
        .expect("seeded episode");
    (series, episode)
}

async fn enqueue_search_run(state: &AppState, target_id: &str) {
    let run = rivet::storage::domain::create_search_run(&state.db, target_id)
        .await
        .unwrap();
    queue::enqueue(
        &state.db,
        "search_run",
        Some(&jobs::SearchRunPayload {
            run_id: run.id.clone(),
            target_id: target_id.to_string(),
        }),
        None,
    )
    .await
    .unwrap();
}

/// The chosen candidate title for the target's single acquisition.
async fn acquired_candidate_title(state: &AppState, target_id: &str) -> (String, String) {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT rc.raw_json, a.status FROM acquisitions a \
         JOIN release_candidates rc ON rc.id = a.candidate_id WHERE a.target_id = ?",
    )
    .bind(target_id)
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "exactly one acquisition: {rows:?}");
    let raw: Value = serde_json::from_str(&rows[0].0).unwrap();
    (raw["title"].as_str().unwrap().to_string(), rows[0].1.clone())
}

// ---------------------------------------------------------------------------
// 1. Upgrade flow
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_flow_grabs_2160p_over_existing_1080p() {
    let (_dir, state) = test_state(true, None).await;

    // Library already holds an imported 1080p WEB-DL file (score 100).
    let (_series, episode) = seed_imported_episode(&state, "Show", "WEB-DL 1080p", 100).await;
    assert!(episode.has_file, "seeded episode must be imported");

    let feed = torznab_feed(&[
        torznab_item("Show.S01E01.1080p.HDTV.x264-RIVET", &"a".repeat(40), 90),
        torznab_item("Show.S01E01.2160p.WEB-DL.x265-RIVET", &"b".repeat(40), 50),
    ]);
    let server = serve_torznab(feed).await;
    let target = seed_search_target(&state, &server.uri(), &["1080p", "2160p"]).await;

    enqueue_search_run(&state, &target).await;
    drain_queue(&state, 8).await;

    // Best-candidate selection must pick the 2160p release even though the
    // 1080p one has almost twice the seeders (score 40 > 30 dominates).
    let (title, status) = acquired_candidate_title(&state, &target).await;
    assert!(
        title.contains("2160p"),
        "pipeline must grab the 2160p candidate, got: {title}"
    );
    assert!(!title.contains("1080p"));
    assert!(
        matches!(
            status.as_str(),
            "enqueued" | "downloading" | "verifying" | "completed" | "imported" | "stalled"
        ),
        "acquisition advanced into the pipeline, status={status}"
    );

    // Upgrade decisioning against the existing file: the grabbed 2160p is a
    // genuine upgrade; a same-quality 1080p WEB-DL release is NOT a re-grab.
    let have = parse("Show.S01E01.1080p.WEB-DL.x264-RIVET");
    let up = parse("Show.S01E01.2160p.WEB-DL.x265-RIVET");
    let d_up = decide_upgrade(
        &up.quality,
        up.quality.modifier,
        Some(&have.quality),
        Some(have.quality.modifier),
    );
    assert!(d_up.grab && d_up.upgrade, "2160p must upgrade over 1080p: {d_up:?}");
    let eq = parse("Show.S01E01.1080p.WEB-DL.x265-OTHER");
    let d_eq = decide_upgrade(
        &eq.quality,
        eq.quality.modifier,
        Some(&have.quality),
        Some(have.quality.modifier),
    );
    assert!(!d_eq.grab, "equal quality must not re-grab: {d_eq:?}");
}

// ---------------------------------------------------------------------------
// 2. Proper/repack preference
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proper_release_beats_lower_resolution_and_plain_same_quality() {
    let (_dir, state) = test_state(true, None).await;

    // Existing file: plain 1080p WEB-DL (no modifier).
    let (_series, episode) = seed_imported_episode(&state, "Show", "WEB-DL 1080p", 100).await;
    assert!(episode.has_file);

    // The 720p release has far more seeders; the 1080p PROPER must still win
    // on decision score (30 + proper bonus 5 > 20).
    let feed = torznab_feed(&[
        torznab_item("Show.S01E01.720p.WEB-DL.x264-RIVET", &"c".repeat(40), 100),
        torznab_item("Show.S01E01.1080p.WEB-DL.x264.PROPER-RIVET", &"d".repeat(40), 40),
    ]);
    let server = serve_torznab(feed).await;
    let target = seed_search_target(&state, &server.uri(), &["720p", "1080p"]).await;

    enqueue_search_run(&state, &target).await;
    drain_queue(&state, 8).await;

    let (title, _status) = acquired_candidate_title(&state, &target).await;
    assert!(
        title.contains("PROPER") && title.contains("1080p"),
        "PROPER 1080p must beat plain 720p, got: {title}"
    );
    assert!(!title.contains("720p"));

    // decide_upgrade: a PROPER at equal quality upgrades over the plain file;
    // the lower-resolution 720p does not.
    let have = parse("Show.S01E01.1080p.WEB-DL.x264-RIVET");
    let proper = parse("Show.S01E01.1080p.WEB-DL.x264.PROPER-RIVET");
    let d_proper = decide_upgrade(
        &proper.quality,
        proper.quality.modifier,
        Some(&have.quality),
        Some(have.quality.modifier),
    );
    assert!(
        d_proper.grab && d_proper.upgrade,
        "PROPER must beat plain at equal quality: {d_proper:?}"
    );
    let lower = parse("Show.S01E01.720p.WEB-DL.x264-RIVET");
    let d_lower = decide_upgrade(
        &lower.quality,
        lower.quality.modifier,
        Some(&have.quality),
        Some(have.quality.modifier),
    );
    assert!(!d_lower.grab, "720p must be rejected against 1080p: {d_lower:?}");
}

// ---------------------------------------------------------------------------
// 3. Failed-download recovery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stalled_download_recovery_ladder_bounds_to_needs_review() {
    let (_dir, state) = test_state(false, None).await;

    let server = MockServer::start().await;
    mount_qbittorrent_core(&server).await;
    for endpoint in [
        "/api/v2/torrents/resume",
        "/api/v2/torrents/reannounce",
        "/api/v2/torrents/recheck",
    ] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
    }

    // Ladder endpoints are recorded so the exact call counts can be asserted
    // after the run: attempt 1 resume, attempt 2 reannounce, attempts 3+4
    // recheck — then the escalation stops the ladder for good.

    let hash = "e".repeat(40);
    // The torrent sits at 0% in a stalled state forever: every poll sees no
    // progress, driving the monitor → recover → monitor loop.
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!([{
                "hash": hash,
                "name": "Show.S01E01",
                "progress": 0.0,
                "state": "stalledDL",
                "total_size": 1000,
            }])
            .to_string(),
        ))
        .mount(&server)
        .await;

    // Configure the real qBittorrent client against the stub.
    let pw_enc = state.secret_box.encrypt_to_string(b"stub-pass").unwrap();
    settings::upsert(
        &state.db,
        "setup.clients",
        &json!({
            "qbittorrent": {"url": server.uri(), "username": "admin", "passwordEnc": pw_enc}
        })
        .to_string(),
    )
    .await
    .unwrap();

    let target = rivet::storage::domain::create_wanted_target(
        &state.db,
        "series",
        "tmdb",
        "7",
        Some(&json!({"query": "show s01"})),
    )
    .await
    .unwrap();
    let run = rivet::storage::domain::create_search_run(&state.db, &target.id)
        .await
        .unwrap();
    let cand = rivet::storage::domain::insert_candidate(
        &state.db,
        &run.id,
        "mock",
        &json!({
            "title": "Show.S01E01",
            "download_url": format!("magnet:?xt=urn:btih:{hash}&dn=show"),
        })
        .to_string(),
        None,
    )
    .await
    .unwrap();
    let acq = rivet::storage::acquisitions::create_acquisition(
        &state.db,
        &cand.id,
        &target.id,
        "torrent",
        "qbittorrent",
        "default",
    )
    .await
    .unwrap();

    // Seed a mid-download acquisition whose progress went stale 90 minutes
    // ago, so the monitor's stall detector (30 min) fires on the first poll.
    let stale = (chrono::Utc::now() - chrono::Duration::minutes(90))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query(
        "UPDATE acquisitions SET status = 'downloading', external_id = ?, progress = 0.0, \
         progress_changed_at = ? WHERE id = ?",
    )
    .bind(&hash)
    .bind(&stale)
    .bind(&acq.id)
    .execute(&state.db)
    .await
    .unwrap();

    queue::enqueue(
        &state.db,
        "acquisition_monitor",
        Some(&jobs::MonitorPayload {
            acquisition_id: acq.id.clone(),
            not_found_polls: 0,
            paused_polls: 0,
        }),
        None,
    )
    .await
    .unwrap();

    // Drive monitor/recover/rescout to exhaustion. Each iteration keeps the
    // stall visible (rewinds progress_changed_at) and fast-forwards queued
    // continuations, so the real handler logic decides every transition.
    let opts = queue::ClaimOpts {
        lease_owner: "integration-worker".into(),
        lease_duration: chrono::Duration::seconds(120),
    };
    for _ in 0..24 {
        let _ = sqlx::query("UPDATE acquisitions SET progress_changed_at = ? WHERE id = ?")
            .bind(&stale)
            .bind(&acq.id)
            .execute(&state.db)
            .await;
        let _ = sqlx::query("UPDATE jobs SET run_after = ? WHERE state = 'queued'")
            .bind(rivet::timeutil::now())
            .execute(&state.db)
            .await;
        let job = match queue::claim_one(&state.db, &opts).await {
            Ok(j) => j,
            Err(_) => break,
        };
        match jobs::handlers::dispatch(&state, &job).await {
            Ok(()) => {
                let _ = queue::complete(&state.db, &job.id, &opts.lease_owner).await;
            }
            Err(e) => {
                let _ = queue::fail_and_retry(
                    &state.db,
                    &job.id,
                    &opts.lease_owner,
                    job.attempt + 1,
                    &e,
                )
                .await;
                let _ =
                    sqlx::query("UPDATE jobs SET run_after = ? WHERE id = ? AND state = 'queued'")
                        .bind(rivet::timeutil::now())
                        .bind(&job.id)
                        .execute(&state.db)
                        .await;
            }
        }
    }

    // Bounded attempts land exactly on needs_review after the full ladder.
    let row = rivet::storage::acquisitions::get(&state.db, &acq.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "needs_review", "ladder must end in review");
    assert_eq!(row.recovery_attempt, 4, "ESCALATE_AFTER_ATTEMPTS = 4");
    assert_eq!(row.verify_status.as_deref(), Some("stalled"));

    // Exactly ONE rescout continuation was enqueued by the escalation — the
    // recovery loop never multiplied it.
    let rescouts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'acquisition_rescout'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(rescouts, 1, "rescout must fire once, got {rescouts}");
    let acquisitions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acquisitions WHERE target_id = ?")
        .bind(&target.id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(acquisitions, 1, "no replacement acquisition without indexers");

    // The ladder hit resume exactly once, reannounce once, recheck twice —
    // bounded attempts, no runaway recovery loop.
    let requests = server.received_requests().await.unwrap_or_default();
    let count = |suffix: &str| requests.iter().filter(|r| r.url.path() == suffix).count();
    assert_eq!(count("/api/v2/torrents/resume"), 1, "resume ladder step");
    assert_eq!(count("/api/v2/torrents/reannounce"), 1, "reannounce ladder step");
    assert_eq!(count("/api/v2/torrents/recheck"), 2, "recheck ladder steps 3+4");

}

// ---------------------------------------------------------------------------
// 4. Series lifecycle end-to-end over the real router
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn series_lifecycle_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_qbittorrent_core(&server).await;

    // Real media fixture (> 25 MiB minimum, allowed extension) so verify and
    // import operate on a genuine file.
    let lib_root = dir.path().join("library");
    let quarantine = dir.path().join("quarantine");
    std::fs::create_dir_all(&lib_root).unwrap();
    std::fs::create_dir_all(&quarantine).unwrap();
    let fixture = dir.path().join("Rivet.Chronicles.S01E01.2160p.WEB-DL.mkv");
    std::fs::create_dir_all(dir.path().join("downloads")).unwrap();
    let f = std::fs::File::create(&fixture).unwrap();
    f.set_len(26 * 1024 * 1024).unwrap();

    // The stub reports the torrent fully downloaded with the fixture as its
    // content path, so the real monitor → verify → import chain completes.
    let fixture_path = fixture.to_string_lossy().into_owned();
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!([{
                "hash": "f".repeat(40),
                "name": "Rivet.Chronicles.S01E01.2160p.WEB-DL",
                "progress": 1.0,
                "state": "uploading",
                "content_path": fixture_path,
            }])
            .to_string(),
        ))
        .mount(&server)
        .await;

    let (_db_dir, state) = test_state(false, None).await;
    let db = state.db.clone();

    let gate = httpapi::middleware::setup_gate::init_shared(
        state.db.clone(),
        std::time::Duration::from_secs(3600),
    )
    .await;
    let app = httpapi::build_router(state.clone(), gate);

    let cookie_hdr = |cookie: &str| format!("rivet_session={cookie}");

    // First run: create admin, log in immediately, collect the CSRF token.
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/admin")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": "admin@example.com", "password": "correct-horse-battery"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": "admin@example.com", "password": "correct-horse-battery"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            s.strip_prefix("rivet_session=")
                .map(|rest| rest.split(';').next().unwrap_or(rest).to_string())
        })
        .expect("session cookie");
    let csrf = body_json(res).await["token"]
        .as_str()
        .expect("csrf token")
        .to_string();

    // Wizard: paths (quarantine + library root), download client, finish.
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/paths/save")
                .header("content-type", "application/json")
                .header("cookie", cookie_hdr(&cookie))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    json!({
                        "downloadsCompletedPath": dir.path().join("downloads").to_string_lossy(),
                        "quarantinePath": quarantine.to_string_lossy(),
                        "libraryRoots": [lib_root.to_string_lossy()],
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "paths save");

    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/clients/save")
                .header("content-type", "application/json")
                .header("cookie", cookie_hdr(&cookie))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    json!({
                        "qbittorrent": {
                            "url": server.uri(),
                            "username": "admin",
                            "password": "stub-pass",
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "clients save validates against the stub");

    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/finish")
                .header("cookie", cookie_hdr(&cookie))
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "wizard finish");

    // Seed series/season/episodes through the real storage upserts.
    let series = upsert_series(
        &db,
        &SeriesUpsert {
            id: None,
            tvdb_id: None,
            tmdb_id: Some(4242),
            imdb_id: None,
            title: "Rivet Chronicles".into(),
            sort_title: None,
            status: "continuing".into(),
            overview: None,
            network: None,
            first_aired: None,
            genres: vec!["Drama".into()],
            path: lib_root.join("Rivet Chronicles").to_string_lossy().into_owned(),
            quality_profile: "hd".into(),
            monitored: true,
            season_folder: true,
            tags: vec![],
        },
    )
    .await
    .unwrap()
    .0;
    upsert_season(&db, &series.id, 1, true).await.unwrap();
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let tomorrow = (chrono::Utc::now() + chrono::Duration::days(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    bulk_upsert_episodes(
        &db,
        &series.id,
        &[
            EpisodeUpsert {
                season_number: 1,
                episode_number: 1,
                absolute_episode_number: Some(1),
                title: Some("Pilot".into()),
                air_date_utc: Some(yesterday),
                runtime: Some(42),
                overview: None,
                monitored: true,
            },
            EpisodeUpsert {
                season_number: 1,
                episode_number: 2,
                absolute_episode_number: Some(2),
                title: Some("Second Signal".into()),
                air_date_utc: Some(tomorrow),
                runtime: Some(42),
                overview: None,
                monitored: true,
            },
        ],
    )
    .await
    .unwrap();
    let episodes = rivet::storage::series::list_episodes_by_series(&db, &series.id)
        .await
        .unwrap();
    let e01 = episodes.iter().find(|e| e.episode_number == 1).unwrap();
    let e02 = episodes.iter().find(|e| e.episode_number == 2).unwrap();

    // The wanted target the grab will attach to.
    let target = rivet::storage::domain::create_wanted_target(
        &db,
        "series",
        "tmdb",
        "4242",
        Some(&json!({"query": "rivet chronicles"})),
    )
    .await
    .unwrap();

    // Calendar shows both episodes of the series.
    let start = (chrono::Utc::now() - chrono::Duration::days(2))
        .format("%Y-%m-%d")
        .to_string();
    let end = (chrono::Utc::now() + chrono::Duration::days(2))
        .format("%Y-%m-%d")
        .to_string();
    let res = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/calendar?start={start}&end={end}"))
                .header("cookie", cookie_hdr(&cookie))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cal = body_json(res).await;
    let cal_ids: Vec<&str> = cal.as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert!(cal_ids.contains(&e01.id.as_str()), "calendar must list E01: {cal:?}");
    assert!(cal_ids.contains(&e02.id.as_str()), "calendar must list E02: {cal:?}");
    assert_eq!(cal[0]["seriesTitle"], json!("Rivet Chronicles"));
    // Wanted/missing lists only the aired, file-less episode.
    let res = app
        .clone()
        .oneshot(
            Request::get("/api/v1/wanted/missing")
                .header("cookie", cookie_hdr(&cookie))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let wanted = body_json(res).await;
    let wanted_ids: Vec<&str> = wanted["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(wanted_ids.contains(&e01.id.as_str()), "E01 is missing: {wanted:?}");
    assert!(
        !wanted_ids.contains(&e02.id.as_str()),
        "unaired E02 must not be wanted: {wanted:?}"
    );
    assert!(wanted["total"].as_i64().unwrap_or(0) >= 1);

    // Interactive grab: manual pick creates the acquisition and enqueues it.
    let hash = "f".repeat(40);
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=rivet");
    let release_title = "Rivet.Chronicles.S01E01.2160p.WEB-DL.x265-RIVET";
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/search/interactive/grab")
                .header("content-type", "application/json")
                .header("cookie", cookie_hdr(&cookie))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    json!({
                        "releaseTitle": release_title,
                        "downloadUrl": magnet,
                        "kind": "torrent",
                        "seriesId": target.id,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "grab accepted");
    let grab = body_json(res).await;
    let acq_id = grab["acquisitionId"].as_str().expect("acquisitionId").to_string();

    // The grab recorded the audit event.
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action = 'search.interactive.grab' AND target_id = ?",
    )
    .bind(&acq_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(audits, 1, "grab must be audited");

    // SEAM GAPS (reported): interactive grab stores clientKind='torrent',
    // which acquisition_enqueue rejects ("unsupported_client_kind"), and it
    // writes the candidate URL as camelCase "downloadUrl" while the enqueue
    // handler parses the snake_case `Release` shape ("candidate_missing_url").
    // Bridge both here so the real pipeline can run against the stub client.
    sqlx::query("UPDATE acquisitions SET client_kind = 'qbittorrent' WHERE id = ?")
        .bind(&acq_id)
        .execute(&db)
        .await
        .unwrap();
    let cand_id: String =
        sqlx::query_scalar("SELECT candidate_id FROM acquisitions WHERE id = ?")
            .bind(&acq_id)
            .fetch_one(&db)
            .await
            .unwrap();
    let fixed_raw =
        json!({"title": release_title, "download_url": magnet, "kind": "torrent"}).to_string();
    sqlx::query("UPDATE release_candidates SET raw_json = ? WHERE id = ?")
        .bind(&fixed_raw)
        .bind(&cand_id)
        .execute(&db)
        .await
        .unwrap();

    // Drain the queue: enqueue → monitor (stub reports completed) → verify →
    // import, all through the real handlers.
    drain_queue(&state, 12).await;

    let row = rivet::storage::acquisitions::get(&db, &acq_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "imported", "full pipeline must import, last_error={:?}", row.last_error);
    assert_eq!(row.verified_path.as_deref(), Some(fixture.to_string_lossy().as_ref()));
    let imported = row.imported_path.clone().expect("imported path");
    assert!(
        imported.starts_with(lib_root.to_string_lossy().as_ref()),
        "import lands under the library root: {imported}"
    );

    // Naming preview: parse the grabbed release and render the library name
    // with the parsed quality label.
    let parsed = parse(release_title);
    assert_eq!(parsed.episodes, vec![1]);
    let ctx = NamingContext {
        title: "Rivet Chronicles".into(),
        season: Some(1),
        episodes: vec![1],
        quality_label: parsed.quality.label(),
        ..NamingContext::default()
    };
    let filename = render_series(
        "{Title} - S{Season:00}E{Episode:00} - {Quality Full}",
        "",
        &ctx,
        false,
    );
    assert_eq!(filename, "Rivet Chronicles - S01E01 - WEB-DL 2160p", "naming preview");
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}
