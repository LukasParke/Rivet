//! End-to-end first-run flow: setup gate → admin creation → immediate login
//! (the reviewed race) → CSRF enforcement → wizard steps → finish → app APIs.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rivet::config::Config;
use rivet::httpapi;
use rivet::metrics::Metrics;
use rivet::security::secrets::SecretBox;
use rivet::security::OutboundPolicy;
use rivet::state::AppState;
use rivet::ws::hub::Hub;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn test_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!("file://{}", dir.path().join("e2e.db").display());
    let db = rivet::storage::open(&dsn).await.unwrap();
    rivet::storage::migrations::apply(&db).await.unwrap();
    let cfg = Config {
        env: "dev".into(),
        http_addr: ":0".into(),
        database_url: dsn,
        dry_run: true,
        dry_run_fixture_path: None,
        secret_key_base64: Some("BwYFBAMCAQAPDg0MCwoJCBcWFRQTEhEqKCYmJiYmJiY=".to_string()),
        ffprobe_path: None,
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

async fn app(state: AppState) -> axum::Router {
    // Routes read the process-shared gate registry; register it like main does.
    let gate = rivet::httpapi::middleware::setup_gate::init_shared(
        state.db.clone(),
        std::time::Duration::from_secs(3600),
    )
    .await;
    httpapi::build_router(state, gate)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn cookie_of(res: &axum::response::Response) -> Option<String> {
    res.headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            s.strip_prefix("rivet_session=")
                .map(|rest| rest.split(';').next().unwrap_or(rest).to_string())
        })
}

#[tokio::test]
async fn first_run_full_wizard_flow() {
    let (_d, state) = test_state().await;
    let app = app(state).await;

    // 1. Setup status is public and reports setup mode.
    let res = app
        .clone()
        .oneshot(
            Request::get("/api/v1/setup/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let status = body_json(res).await;
    assert_eq!(status["setupMode"], json!(true));
    assert_eq!(status["setupComplete"], json!(false));

    // 2. Non-setup APIs are gated while no users exist.
    let res = app
        .clone()
        .oneshot(
            Request::get("/api/v1/acquisitions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // 3. Create the admin (first user becomes admin).
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
    assert_eq!(res.status(), StatusCode::CREATED, "admin creation");

    // 4. THE REVIEWED RACE: login must work IMMEDIATELY after admin creation
    //    (the gate flips synchronously; no 2s refresher window to lose).
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
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "immediate post-admin login must not race the gate"
    );
    let session_cookie = cookie_of(&res).expect("session cookie set");
    let login = body_json(res).await;
    let csrf = login["token"]
        .as_str()
        .expect("csrf token in login response")
        .to_string();

    // 5. Second admin creation is a conflict.
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/admin")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": "x@example.com", "password": "long-enough-password"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // 6. /auth/me identifies the admin.
    let res = app
        .clone()
        .oneshot(
            Request::get("/api/v1/auth/me")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let me = body_json(res).await;
    assert_eq!(me["role"], json!("admin"));

    // 7. Mutations without CSRF are rejected; with the header they pass.
    let paths_body = json!({
        "downloadsCompletedPath": "/tmp/rivet-dl",
        "quarantinePath": "/tmp/rivet-quarantine",
        "libraryRoots": ["/tmp/rivet-lib"],
    })
    .to_string();
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/paths/save")
                .header("content-type", "application/json")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .body(Body::from(paths_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "missing CSRF header");

    // 8. Paths save in dry-run must not require the directories to exist
    //    (dry-run validation is stat-only and never writes probe files).
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/paths/validate")
                .header("content-type", "application/json")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from(paths_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "dry-run path validation");
    // No probe files may exist anywhere (dry-run write guarantee).
    let probe = std::fs::read_dir("/tmp")
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".rivet_write_test")
        });
    assert!(!probe, "dry-run must not write probe files");

    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/paths/save")
                .header("content-type", "application/json")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from(paths_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "paths save");

    // 9. Finish the wizard.
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/setup/finish")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let finish = body_json(res).await;
    assert_eq!(finish["ok"], json!(true));

    // 10. App APIs now reachable; acquisitions list is empty.
    let res = app
        .clone()
        .oneshot(
            Request::get("/api/v1/acquisitions")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let acquisitions = body_json(res).await;
    assert!(
        acquisitions.is_null()
            || acquisitions
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
    );

    // 11. Logout revokes the session.
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/logout")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = app
        .oneshot(
            Request::get("/api/v1/auth/me")
                .header("cookie", &format!("rivet_session={session_cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "revoked session rejected"
    );
}

#[tokio::test]
async fn weak_admin_password_rejected() {
    let (_d, state) = test_state().await;
    let app = app(state).await;
    let res = app
        .oneshot(
            Request::post("/api/v1/setup/admin")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": "a@b.c", "password": "short"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = body_json(res).await;
    assert_eq!(body["error"], json!("invalid_password"));
}

#[tokio::test]
async fn viewer_role_cannot_create_targets() {
    let (_d, state) = test_state().await;
    let app = app(state.clone()).await;

    // Create admin + login.
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

    // Promote a viewer directly in storage (no registration endpoint by design).
    let viewer = rivet::storage::users::create(
        &state.db,
        "v@example.com",
        "argon2id$placeholder-hash-value",
        "viewer",
    )
    .await
    .unwrap();

    // Forge a session for the viewer.
    let sess = rivet::storage::sessions::create(
        &state.db,
        &viewer.id,
        chrono::Duration::hours(1),
        None,
        None,
    )
    .await
    .unwrap();

    let res = app
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .header("cookie", &format!("rivet_session={}", sess.id))
                .header("x-csrf-token", &sess.csrf_token)
                .body(Body::from(
                    json!({"kind": "series", "externalSource": "tmdb", "externalId": "1", "query": "q"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "viewer must not create targets"
    );
}
