//! `/setup/status` and `/setup/admin`: setup wizard entry points.
//!
//! `POST /setup/admin` creates the first (admin) user and synchronously flips
//! the setup gate ([`SetupGate::notify_user_created`]) so the frontend's
//! immediate login cannot lose the periodic-refresh race.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::httpapi::middleware::setup_gate;
use crate::security::password;
use crate::state::AppState;
use crate::storage::audit::{insert as insert_audit, AuditEvent};
use crate::ws::hub::PublishExt;

#[derive(Debug, serde::Serialize)]
struct SetupStatusResponse {
    setupMode: bool,
    setupComplete: bool,
    dryRun: bool,
    dryRunFixturePathSet: bool,
}

#[derive(Debug, Deserialize)]
struct CreateAdminRequest {
    email: String,
    password: String,
}

#[derive(Debug, serde::Serialize)]
struct CreateAdminResponse {
    userId: String,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/setup/status", axum::routing::get(status))
        .route("/setup/admin", axum::routing::post(create_admin))
}

async fn setup_complete(state: &AppState) -> bool {
    crate::storage::settings::get(&state.db, "setup.report.latest")
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn status(State(state): State<AppState>) -> Response {
    let setup_mode = match setup_gate::shared() {
        Some(gate) => gate.is_setup_mode(),
        // Standalone construction (tests): derive from the authoritative count.
        None => crate::storage::users::count_users(&state.db)
            .await
            .map(|n| n == 0)
            .unwrap_or(true),
    };
    let fixture = state.config.dry_run_fixture_path.as_deref().map(str::trim);
    axum::Json(SetupStatusResponse {
        setupMode: setup_mode,
        setupComplete: setup_complete(&state).await,
        dryRun: state.config.dry_run,
        dryRunFixturePathSet: fixture.map(|f| !f.is_empty()).unwrap_or(false),
    })
    .into_response()
}

fn setup_conflict() -> Response {
    (
        StatusCode::CONFLICT,
        axum::Json(json!({
            "error": "setup_complete",
            "message": "setup has already been completed",
        })),
    )
        .into_response()
}

async fn create_admin(
    State(state): State<AppState>,
    body: Result<axum::Json<CreateAdminRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // Gate fast-path; the user count below stays authoritative either way.
    if setup_gate::shared().is_some_and(|g| !g.is_setup_mode()) {
        return setup_conflict();
    }

    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let email = req.email.trim().to_string();
    let pw = req.password.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        return AppError::BadRequest("invalid_email".into()).into_response();
    }

    // Argon2id is CPU-heavy: hash off the async runtime. Also enforces the
    // 12+ character minimum (invalid_password).
    let hash = match tokio::task::spawn_blocking(move || password::hash_password(&pw)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "invalid_password", "message": e.to_string()})),
            )
                .into_response()
        }
        Err(_) => return AppError::Internal("hash task failed".into()).into_response(),
    };

    // Authoritative check + insert in ONE `BEGIN IMMEDIATE` transaction.
    // A plain SELECT-then-INSERT has a TOCTOU window: two concurrent
    // first-run requests with different emails can both observe zero users
    // and mint two admins. BEGIN IMMEDIATE takes the write lock up front,
    // serializing contenders before the count is read.
    let mut tx = match state.db.begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "db_error"})),
            )
                .into_response()
        }
    };
    let n = match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(&mut *tx)
        .await
    {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "db_error"})),
            )
                .into_response()
        }
    };
    if n != 0 {
        // Dropping `tx` rolls back and releases the write lock.
        return setup_conflict();
    }

    // Inlined from storage::users::create so the INSERT joins the same
    // transaction (`create` takes a pool handle and cannot run on `tx`).
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::timeutil::fmt(chrono::Utc::now());
    match sqlx::query(
        "INSERT INTO users (id, email, password_hash, role, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&email)
    .bind(&hash)
    .bind("admin")
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await
    {
        Ok(_) => {}
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "create_user_failed"})),
            )
                .into_response()
        }
    };
    if tx.commit().await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "db_error"})),
        )
            .into_response();
    }

    // RACE FIX: flip the gate synchronously BEFORE responding, so the
    // frontend's immediate POST /auth/login cannot hit setup_required.
    if let Some(gate) = setup_gate::shared() {
        gate.notify_user_created();
    }

    let _ = insert_audit(
        &state.db,
        AuditEvent {
            actor_user_id: Some(id.clone()),
            action: "setup.admin_created".into(),
            ..Default::default()
        },
    )
    .await;
    state.hub.publish_event(
        "audit.event",
        json!({"action": "setup.admin_created", "actorUserId": id}),
    );

    (
        StatusCode::CREATED,
        axum::Json(CreateAdminResponse { userId: id }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::metrics::Metrics;
    use crate::security::secrets::SecretBox;
    use crate::security::OutboundPolicy;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn app_state(dry_run: bool) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("setup_routes_test.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&db).await.unwrap();
        let config = Arc::new(crate::config::Config {
            env: "test".into(),
            http_addr: String::new(),
            database_url: String::new(),
            dry_run,
            dry_run_fixture_path: Some("/tmp/fixture.json".into()),
            ffprobe_path: None,
            secret_key_base64: None,
            session_cookie_name: "rivet_session".into(),
            session_ttl: std::time::Duration::from_secs(30 * 24 * 3600),
            allow_internal_urls: true,
        });
        (
            dir,
            AppState {
                db,
                secret_box: Arc::new(SecretBox::new(None).unwrap()),
                policy: OutboundPolicy::dev(),
                hub: Arc::new(Hub::new()),
                metrics: Arc::new(Metrics::default()),
                config,
            },
        )
    }

    fn full_router(state: AppState) -> axum::Router {
        router()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state)
    }

    async fn req_json(
        state: AppState,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let resp = full_router(state)
            .oneshot(
                b.body(Body::from(body.unwrap_or_default().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn status_reports_flags() {
        let (_dir, state) = app_state(true).await;

        let (_, v) = req_json(state.clone(), "GET", "/api/v1/setup/status", None).await;
        // No gate registered in this process => fallback derives from count.
        assert_eq!(v["dryRun"], true);
        assert_eq!(v["dryRunFixturePathSet"], true);
        assert_eq!(v["setupComplete"], false);

        // Writing the finish report flips setupComplete.
        crate::storage::settings::upsert(&state.db, "setup.report.latest", "{}")
            .await
            .unwrap();
        let (_, v) = req_json(state, "GET", "/api/v1/setup/status", None).await;
        assert_eq!(v["setupComplete"], true);
    }

    #[tokio::test]
    async fn create_admin_happy_path_and_race_fix() {
        // The gate registry is process-wide: isolate this test from siblings.
        crate::httpapi::middleware::setup_gate::reset_shared_for_tests();
        let (_dir, state) = app_state(true).await;
        if let Some(g) = crate::httpapi::middleware::setup_gate::shared() {
            g.notify_user_created(); // neutralize any stale gate from another test
            crate::httpapi::middleware::setup_gate::reset_shared_for_tests();
        }

        // Weak password rejected before any user exists.
        let (s, v) = req_json(
            state.clone(),
            "POST",
            "/api/v1/setup/admin",
            Some(r#"{"email":"a@x.test","password":"short"}"#),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "invalid_password");

        let (s, v) = req_json(
            state.clone(),
            "POST",
            "/api/v1/setup/admin",
            Some(r#"{"email":"a@x.test","password":"long-enough-password"}"#),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        assert!(v["userId"].is_string());

        // First user is admin with a real argon2id hash.
        let (role, hash): (String, String) =
            sqlx::query_as("SELECT role, password_hash FROM users WHERE email = 'a@x.test'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(role, "admin");
        assert!(hash.starts_with("argon2id$") || hash.starts_with("$argon2id$"));

        // Audit row recorded.
        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "setup.admin_created"));

        // Second creation is a 409 (setup complete).
        let (s, v) = req_json(
            state.clone(),
            "POST",
            "/api/v1/setup/admin",
            Some(r#"{"email":"b@x.test","password":"long-enough-password"}"#),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(v["error"], "setup_complete");

        // Status now derives setupMode=false from the user count fallback.
        let (_, v) = req_json(state, "GET", "/api/v1/setup/status", None).await;
        assert_eq!(v["setupMode"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_first_run_mints_exactly_one_admin() {
        // Regression for the TOCTOU fix: 8 parallel first-run requests with
        // distinct emails must yield exactly one 201 and seven conflicts —
        // never two admins.
        crate::httpapi::middleware::setup_gate::reset_shared_for_tests();
        let (_dir, state) = app_state(true).await;
        if let Some(g) = crate::httpapi::middleware::setup_gate::shared() {
            g.notify_user_created(); // neutralize any stale gate from another test
            crate::httpapi::middleware::setup_gate::reset_shared_for_tests();
        }

        let mut handles = Vec::new();
        for i in 0..8 {
            let st = state.clone();
            handles.push(tokio::spawn(async move {
                let body = format!(
                    r#"{{"email":"race{i}@x.test","password":"long-enough-password"}}"#
                );
                req_json(st, "POST", "/api/v1/setup/admin", Some(&body)).await
            }));
        }

        let (mut created, mut rejected) = (0usize, 0usize);
        for h in handles {
            let (status, _) = h.await.unwrap();
            match status {
                StatusCode::CREATED => created += 1,
                StatusCode::CONFLICT | StatusCode::BAD_REQUEST => rejected += 1,
                other => panic!("unexpected status {other}"),
            }
        }
        assert_eq!(created, 1, "exactly one request may mint the admin");
        assert_eq!(rejected, 7);

        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&state.db).await.unwrap();
        assert_eq!(n, 1);
    }
}
