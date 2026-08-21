//! `/auth/*`: login, logout, session introspection and CSRF token delivery.
//!
//! Wire shapes match the original fiber implementation. Auth protection
//! follows the documented pattern in `crate::httpapi::middleware::auth`
//! (global layers attached by the parent; per-route auth via extractors).

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::httpapi::middleware::auth::{
    client_ip, cookie_value, dummy_password_hash, login_limiter, AuthenticatedUser,
};
use crate::security::password;
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};
use crate::timeutil;

#[derive(Debug, Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
    /// TOTP code; required only when the account has two-factor enabled.
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct MeResponse {
    userId: String,
    email: String,
    role: String,
}

#[derive(Debug, serde::Serialize)]
struct CsrfResponse {
    token: String,
}

/// Exports ONLY method+path routes; the parent nests under /api/v1.
pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/auth/login", axum::routing::post(login))
        .route("/auth/logout", axum::routing::post(logout))
        .route("/auth/me", axum::routing::get(me))
        .route("/auth/csrf", axum::routing::get(deliver_csrf))
        .route("/auth/totp/setup", axum::routing::post(totp_setup))
        .route("/auth/totp/enable", axum::routing::post(totp_enable))
        .route("/auth/totp/disable", axum::routing::post(totp_disable))
}

/// JSON error envelope with an explicit machine-readable code, for codes not
/// covered by [`AppError`] variants (`rate_limited`, `session_create_failed`).
fn coded_error(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(json!({ "error": code }))).into_response()
}

fn http_date(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

async fn verify_in_blocking(pw: String, encoded: &str) -> bool {
    let encoded = encoded.to_string();
    tokio::task::spawn_blocking(move || password::verify_password(&pw, &encoded))
        .await
        .ok()
        .map(|r| r.unwrap_or(false))
        .unwrap_or(false)
}

/// Constant-shape 401 for both unknown emails and wrong passwords. The dummy
/// argon2 verification on the unknown-email path equalizes timing so response
/// latency cannot enumerate accounts.
async fn invalid_credentials(pw: String) -> Response {
    let _ = verify_in_blocking(pw, dummy_password_hash()).await;
    coded_error(StatusCode::UNAUTHORIZED, "invalid_credentials")
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<axum::Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // Forwarded-aware client IP keys both rate limiting and audit rows.
    let ip = client_ip(&headers);
    if !login_limiter().allow(&format!("login:{ip}"), std::time::Instant::now()) {
        return coded_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }

    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let email = req.email.trim().to_ascii_lowercase();
    let pw = req.password.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        return AppError::BadRequest("invalid_email".into()).into_response();
    }

    // Argon2 is CPU-heavy: never verify on the async runtime threads.
    let user = match crate::storage::users::get_by_email(&state.db, &email).await {
        Ok(u) => u,
        Err(_) => return invalid_credentials(pw).await,
    };
    if !verify_in_blocking(pw.clone(), &user.password_hash).await {
        return invalid_credentials(pw).await;
    }

    // Two-factor gate: enabled accounts MUST present a valid TOTP code.
    // Accounts without TOTP are unaffected (backward compatible).
    if user.totp_enabled {
        let Some(code) = req.code.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
            return coded_error(StatusCode::UNAUTHORIZED, "totp_required");
        };
        let Some(enc) = user.totp_secret_enc.as_deref() else {
            return coded_error(StatusCode::UNAUTHORIZED, "invalid_code");
        };
        let Ok(secret) = state.secret_box.decrypt_string(enc) else {
            return coded_error(StatusCode::UNAUTHORIZED, "invalid_code");
        };
        let Ok(secret_b32) = std::str::from_utf8(&secret) else {
            return coded_error(StatusCode::UNAUTHORIZED, "invalid_code");
        };
        match crate::security::totp::verify(secret_b32, code, 1) {
            Ok(true) => {}
            _ => return coded_error(StatusCode::UNAUTHORIZED, "invalid_code"),
        }
    }

    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);
    let ip_opt = (!ip.is_empty() && ip != "unknown").then_some(ip.clone());

    let ttl =
        chrono::Duration::from_std(state.config.session_ttl).unwrap_or(chrono::Duration::days(30));
    let sess = match crate::storage::sessions::create(
        &state.db,
        &user.id,
        ttl,
        ip_opt.as_deref(),
        ua.as_deref(),
    )
    .await
    {
        Ok(s) => s,
        Err(_) => return coded_error(StatusCode::INTERNAL_SERVER_ERROR, "session_create_failed"),
    };

    let _ = insert_audit(
        &state.db,
        AuditEvent {
            actor_user_id: Some(user.id.clone()),
            action: "auth.login".into(),
            ip: ip_opt.clone(),
            user_agent: ua.clone(),
            ..Default::default()
        },
    )
    .await;
    state.hub.publish_event(
        "audit.event",
        json!({"action": "auth.login", "actorUserId": user.id, "ip": ip, "userAgent": ua}),
    );

    // HttpOnly cookie; Secure belongs to reverse-proxy/TLS termination, as before.
    let expires = timeutil::parse(&sess.expires_at).unwrap_or_else(chrono::Utc::now);
    let mut resp = (
        StatusCode::OK,
        axum::Json(CsrfResponse { token: sess.csrf_token.clone() }),
    )
        .into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&format!(
            "{}={}; Path=/; HttpOnly; SameSite=Lax; Expires={}",
            state.config.session_cookie_name,
            sess.id,
            http_date(expires)
        ))
        .expect("cookie header is valid"),
    );
    // The CSRF token rides the JSON body for the client to attach as
    // X-CSRF-Token on mutations.
    resp
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // No session => idempotent no-op.
    let Some(sid) = cookie_value(&headers, &state.config.session_cookie_name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return StatusCode::NO_CONTENT.into_response();
    };

    if let Ok(Some(s)) = crate::storage::sessions::get(&state.db, &sid).await {
        let _ = crate::storage::sessions::revoke(&state.db, &sid).await;
        let ip = client_ip(&headers);
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToString::to_string);
        let _ = insert_audit(
            &state.db,
            AuditEvent {
                actor_user_id: Some(s.user_id.clone()),
                action: "auth.logout".into(),
                ip: Some(ip.clone()).filter(|v| v != "unknown"),
                user_agent: ua.clone(),
                ..Default::default()
            },
        )
        .await;
        state.hub.publish_event(
            "audit.event",
            json!({"action": "auth.logout", "actorUserId": s.user_id, "ip": ip, "userAgent": ua}),
        );
    }

    // Clear the cookie unconditionally.
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = header::HeaderValue::from_str(&format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        state.config.session_cookie_name
    )) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

async fn me(user: AuthenticatedUser) -> axum::Json<MeResponse> {
    axum::Json(MeResponse {
        userId: user.user.id,
        email: user.user.email,
        role: user.user.role,
    })
}

async fn deliver_csrf(user: AuthenticatedUser) -> Result<axum::Json<CsrfResponse>, AppError> {
    if user.session.csrf_token.is_empty() {
        return Err(AppError::Unauthorized);
    }
    Ok(axum::Json(CsrfResponse { token: user.session.csrf_token }))
}

// ---------------------------------------------------------------------------
// TOTP two-factor self-service.

#[derive(Debug, Deserialize)]
struct TotpCodeRequest {
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct TotpSetupResponse {
    otpauthUri: String,
}

/// Generates a fresh secret, stores it ENCRYPTED but disabled until confirmed
/// via /auth/totp/enable. Re-running setup replaces any pending secret.
async fn totp_setup(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<axum::Json<TotpSetupResponse>, AppError> {
    let secret_b32 = crate::security::totp::generate_secret();
    let enc = state
        .secret_box
        .encrypt_to_string(secret_b32.as_bytes())
        .map_err(|e| AppError::Internal(e.to_string()))?;
    crate::storage::users::set_totp_secret(&state.db, &user.user.id, Some(&enc), false)
        .await
        .map_err(AppError::from)?;
    insert_audit(
        &state.db,
        AuditEvent {
            actor_user_id: Some(user.user.id.clone()),
            action: "auth.totp.setup".into(),
            ..Default::default()
        },
    )
    .await
    .ok();
    Ok(axum::Json(TotpSetupResponse {
        otpauthUri: crate::security::totp::otpauth_uri(&user.user.email, &secret_b32),
    }))
}

/// Confirms enrollment: verifies one live code against the pending secret,
/// then flips totp_enabled=1.
async fn totp_enable(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    body: Result<axum::Json<TotpCodeRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    handle_totp_toggle(state, user, body, true).await
}

/// Turns two-factor off after verifying a live code; clears the stored secret.
async fn totp_disable(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    body: Result<axum::Json<TotpCodeRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    handle_totp_toggle(state, user, body, false).await
}

async fn handle_totp_toggle(
    state: AppState,
    user: AuthenticatedUser,
    body: Result<axum::Json<TotpCodeRequest>, axum::extract::rejection::JsonRejection>,
    enable: bool,
) -> Response {
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let Some(code) = req.code.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
        return coded_error(StatusCode::BAD_REQUEST, "code_required");
    };
    let Some(enc) = user.user.totp_secret_enc.as_deref() else {
        return coded_error(StatusCode::BAD_REQUEST, "totp_not_setup");
    };
    let secret = match state.secret_box.decrypt_string(enc)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
    {
        Some(s) => s,
        None => return coded_error(StatusCode::INTERNAL_SERVER_ERROR, "totp_secret_unreadable"),
    };
    match crate::security::totp::verify(&secret, code, 1) {
        Ok(true) => {}
        Ok(false) => return coded_error(StatusCode::UNAUTHORIZED, "invalid_code"),
        Err(_) => return coded_error(StatusCode::INTERNAL_SERVER_ERROR, "totp_secret_unreadable"),
    }
    let new_enc = if enable {
        Some(enc.to_string())
    } else {
        None
    };
    if crate::storage::users::set_totp_secret(&state.db, &user.user.id, new_enc.as_deref(), enable)
        .await
        .is_err()
    {
        return coded_error(StatusCode::INTERNAL_SERVER_ERROR, "totp_update_failed");
    }
    let action = if enable { "auth.totp.enable" } else { "auth.totp.disable" };
    let _ = insert_audit(
        &state.db,
        AuditEvent {
            actor_user_id: Some(user.user.id.clone()),
            action: action.into(),
            ..Default::default()
        },
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------

/// Creates a user + live session directly (tests avoid extra argon2 rounds).
#[cfg(test)]
async fn seed_session(state: &AppState, email: &str, role: &str) -> crate::storage::sessions::Session {
    let hash = password::hash_password("long-enough-password").unwrap();
    let u = crate::storage::users::create(&state.db, email, &hash, role).await.unwrap();
    crate::storage::sessions::create(&state.db, &u.id, chrono::Duration::hours(1), None, None)
        .await
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::metrics::Metrics;
    use crate::security::secrets::SecretBox;
    use crate::security::OutboundPolicy;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::middleware;
    use tower::ServiceExt;

    async fn app_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("auth_routes_test.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&db).await.unwrap();
        let config = Arc::new(crate::config::Config {
            env: "test".into(),
            http_addr: String::new(),
            database_url: String::new(),
            dry_run: false,
            dry_run_fixture_path: None,
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

    /// Router with the same global stack the parent attaches under /api/v1.
    fn full_router(state: AppState) -> axum::Router {
        router()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state)
    }
    fn post_json(uri: &str, body: &str, xff: Option<&str>) -> Request {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(ip) = xff {
            b = b.header("x-forwarded-for", ip);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn login_success_sets_cookie_and_returns_csrf_token() {
        let (_dir, state) = app_state().await;
        seed_session(&state, "admin@x.test", "admin").await;

        let resp = full_router(state.clone())
            .oneshot(post_json(
                "/api/v1/auth/login",
                r#"{"email":"Admin@X.test","password":"long-enough-password"}"#,
                Some("203.0.113.9"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.starts_with("rivet_session="));
        assert!(set_cookie.contains("HttpOnly"), "cookie must be HttpOnly");
        assert!(set_cookie.contains("SameSite=Lax"));
        let sid = set_cookie.split(';').next().unwrap().split_once('=').unwrap().1;

        let stored = crate::storage::sessions::get(&state.db, sid).await.unwrap().unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(v["token"], stored.csrf_token);

        // Audit trail got a login row.
        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "auth.login"));

        // The session works through the extractor-protected endpoint.
        let me_req = Request::builder()
            .uri("/api/v1/auth/me")
            .header("cookie", format!("rivet_session={sid}"))
            .body(Body::empty())
            .unwrap();
        let resp = full_router(state).oneshot(me_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(v["role"], "admin");
        assert_eq!(v["email"], "admin@x.test");
        assert_eq!(v["userId"], stored.user_id);
    }

    #[tokio::test]
    async fn invalid_credentials_are_constant_shape() {
        let (_dir, state) = app_state().await;
        seed_session(&state, "known@x.test", "admin").await;

        let attempt = |payload: &'static str| {
            let state = state.clone();
            async move {
                let resp = full_router(state)
                    .oneshot(post_json("/api/v1/auth/login", payload, Some("198.51.100.1")))
                    .await
                    .unwrap();
                let status = resp.status();
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<serde_json::Value>(&body).unwrap())
            }
        };

        let unknown_email =
            attempt(r#"{"email":"ghost@nowhere.test","password":"whatever-long"}"#).await;
        let wrong_password =
            attempt(r#"{"email":"known@x.test","password":"wrong-password-long"}"#).await;
        let malformed_email = attempt(r#"{"email":"not-an-email","password":"x"}"#).await;
        let malformed_json = attempt(r#"not json at all"#).await;

        assert_eq!(unknown_email.0, StatusCode::UNAUTHORIZED);
        assert_eq!(unknown_email.1, serde_json::json!({"error": "invalid_credentials"}));
        assert_eq!(unknown_email.1, wrong_password.1, "shape must not leak which half failed");
        assert_eq!(malformed_email.0, StatusCode::BAD_REQUEST);
        assert_eq!(malformed_email.1["error"], "invalid_email");
        assert_eq!(malformed_json.1["error"], "invalid_json");
    }

    #[tokio::test]
    async fn login_is_rate_limited_per_client_ip() {
        let (_dir, state) = app_state().await;
        let mut last = None;
        for i in 0..12 {
            let payload = format!(r#"{{"email":"rl{i}@x.test","password":"whatever-long"}}"#);
            let resp = full_router(state.clone())
                .oneshot(post_json("/api/v1/auth/login", &payload, Some("192.0.2.77")))
                .await
                .unwrap();
            last = Some(resp.status());
        }
        assert_eq!(
            last,
            Some(StatusCode::TOO_MANY_REQUESTS),
            "11th+ attempt from one IP within the window must be blocked"
        );

        // A different IP is unaffected by the exhausted key above.
        let resp = full_router(state)
            .oneshot(post_json(
                "/api/v1/auth/login",
                r#"{"email":"other@x.test","password":"whatever-long"}"#,
                Some("192.0.2.78"),
            ))
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn logout_is_idempotent_and_revokes() {
        let (_dir, state) = app_state().await;
        let sess = seed_session(&state, "bye@x.test", "admin").await;

        // Logout with the session cookie revokes server-side and clears the cookie.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/logout")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::empty())
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let clear = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(clear.contains("Expires=Thu, 01 Jan 1970"));
        let revoked = crate::storage::sessions::get(&state.db, &sess.id)
            .await
            .unwrap()
            .unwrap();
        assert!(revoked.revoked_at.is_some(), "logout must revoke server-side");
        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "auth.logout"));

        // Logout without any cookie is still 204 (idempotent).
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/logout")
            .body(Body::empty())
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Logout with an UNKNOWN session id also stays 204.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/logout")
            .header("cookie", "rivet_session=bogus")
            .body(Body::empty())
            .unwrap();
        let resp = full_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn csrf_endpoint_returns_session_token() {
        let (_dir, state) = app_state().await;
        let sess = seed_session(&state, "csrf@x.test", "admin").await;

        let req = Request::builder()
            .uri("/api/v1/auth/csrf")
            .header("cookie", format!("rivet_session={}", sess.id))
            .body(Body::empty())
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(v["token"], sess.csrf_token);

        // Anonymous requests are rejected by the extractor.
        let req = Request::builder().uri("/api/v1/auth/csrf").body(Body::empty()).unwrap();
        let resp = full_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -- TOTP two-factor -----------------------------------------------------

    /// Runs setup for `email`'s user (fresh login each time) and returns
    /// (session, otpauth URI).
    async fn do_setup(
        state: &AppState,
        email: &str,
    ) -> (crate::storage::sessions::Session, String) {
        let sess = seed_session(state, email, "admin").await;
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/totp/setup")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::empty())
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        let uri = v["otpauthUri"].as_str().unwrap().to_string();
        assert!(uri.starts_with("otpauth://totp/Rivet:"));
        (sess, uri)
    }

    fn uri_secret(uri: &str) -> String {
        uri.split("secret=").nth(1).unwrap().split('&').next().unwrap().to_string()
    }

    fn current_code(secret: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        crate::security::totp::totp_at(secret, 6, 30, 0, now).unwrap()
    }

    async fn login_body(state: AppState, payload: String) -> (StatusCode, serde_json::Value) {
        let resp = full_router(state)
            .oneshot(post_json("/api/v1/auth/login", &payload, Some("203.0.113.50")))
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn totp_setup_enable_login_flow() {
        let (_dir, state) = app_state().await;
        let (sess, uri) = do_setup(&state, "totp@x.test").await;
        let secret = uri_secret(&uri);

        // Enrollment is NOT active until enable: login without code still works.
        let (status, v) = login_body(
            state.clone(),
            r#"{"email":"totp@x.test","password":"long-enough-password"}"#.into(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pending setup must not gate login: {v}");

        // Enable with a WRONG code is rejected and does not activate.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/totp/enable")
            .header("content-type", "application/json")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::from(r#"{"code":"000000"}"#.to_string()))
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"], "invalid_code");

        // Enable with a live code flips the flag.
        let code = current_code(&secret);
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/totp/enable")
            .header("content-type", "application/json")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::from(format!(r#"{{"code":"{code}"}}"#)))
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let user = crate::storage::users::get_by_email(&state.db, "totp@x.test")
            .await
            .unwrap();
        assert!(user.totp_enabled);
        assert!(user.totp_secret_enc.is_some());

        // Login WITHOUT code now → totp_required.
        let (status, v) = login_body(
            state.clone(),
            r#"{"email":"totp@x.test","password":"long-enough-password"}"#.into(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(v, serde_json::json!({"error": "totp_required"}));

        // Login with a WRONG code → invalid_code.
        let (status, v) = login_body(
            state.clone(),
            r#"{"email":"totp@x.test","password":"long-enough-password","code":"000000"}"#.into(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(v, serde_json::json!({"error": "invalid_code"}));

        // Login with the live code succeeds end-to-end.
        let code = current_code(&secret);
        let (status, v) = login_body(
            state.clone(),
            format!(r#"{{"email":"totp@x.test","password":"long-enough-password","code":"{code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "valid TOTP must grant login: {v}");
        assert!(v["token"].is_string());

        // Disable with a live code clears secret + flag.
        let code = current_code(&secret);
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/totp/disable")
            .header("content-type", "application/json")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::from(format!(r#"{{"code":"{code}"}}"#)))
            .unwrap();
        let resp = full_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let user = crate::storage::users::get_by_email(&state.db, "totp@x.test")
            .await
            .unwrap();
        assert!(!user.totp_enabled);
        assert!(user.totp_secret_enc.is_none(), "disable must clear the secret");

        // And password-only login works again.
        let (status, _) = login_body(
            state,
            r#"{"email":"totp@x.test","password":"long-enough-password"}"#.into(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn totp_endpoints_require_session() {
        let (_dir, state) = app_state().await;
        for uri in ["/api/v1/auth/totp/setup", "/api/v1/auth/totp/enable", "/api/v1/auth/totp/disable"] {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let resp = full_router(state.clone()).oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri} requires a session");
        }
    }

    #[tokio::test]
    async fn totp_enable_without_setup_fails() {
        let (_dir, state) = app_state().await;
        let sess = seed_session(&state, "nosetup@x.test", "admin").await;
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/totp/enable")
            .header("content-type", "application/json")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::from(r#"{"code":"123456"}"#.to_string()))
            .unwrap();
        let resp = full_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
 }
