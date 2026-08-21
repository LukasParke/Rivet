//! Session authentication, RBAC and CSRF for the HTTP API.
//!
//! # Pattern (all route modules MUST follow this)
//!
//! Cross-cutting layers are attached exactly ONCE by `crate::httpapi` under
//! `/api/v1`, in this order: `setup_gate` -> `load_session` -> `csrf`.
//! Route modules therefore export bare routers (`pub fn router() ->
//! Router<AppState>`) and protect handlers with EXTRACTORS only:
//!
//! - [`AuthenticatedUser`] as a handler argument requires a valid session
//!   (missing/expired/revoked => 401 `{"error":"unauthorized"}`).
//! - [`Operator`] / [`Admin`] wrapper extractors additionally enforce a
//!   minimum role rank (insufficient or unknown role => 403
//!   `{"error":"forbidden"}`). Rank order: viewer < operator < admin.
//!
//! The `from_fn`-style handlers below ([`load_session`], [`csrf`],
//! [`require_auth`], [`require_role`]) exist for the global stack owned by
//! `crate::httpapi` and for callers that prefer layer composition; individual
//! route modules must NOT attach them per-route.

use std::sync::LazyLock;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::security::ratelimit::RateLimiter;
use crate::state::AppState;
use crate::storage::sessions::Session;
use crate::storage::users::User;
use crate::timeutil;

/// Session context attached to every request whose cookie resolved to a live
/// session. Handlers should prefer the [`AuthenticatedUser`] extractor.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user: User,
    pub session: Session,
}

/// Extractor for handlers that require a valid, unexpired, unrevoked session.
///
/// Rejection is [`AppError::Unauthorized`] (401, machine code `unauthorized`),
/// matching the original wire shape.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub user: User,
    pub session: Session,
}

impl AuthenticatedUser {
    /// Parsed role of the authenticated user. `None` means the stored role
    /// string is unknown, which denies every role-gated action.
    pub fn role(&self) -> Option<crate::state::Role> {
        crate::state::Role::parse(&self.user.role)
    }

    /// Role rank; unknown roles rank below viewer (0).
    pub fn rank(&self) -> u8 {
        self.role().map(|r| r.rank()).unwrap_or(0)
    }
}

impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .map(|ctx| AuthenticatedUser { user: ctx.user, session: ctx.session })
            .ok_or(AppError::Unauthorized)
    }
}

macro_rules! role_extractor {
    ($name:ident) => {
        #[doc = concat!("Extractor requiring at least the `", stringify!($name), "` role (rank >= `", stringify!($name), "`). Unknown stored roles are denied.")]
        #[derive(Debug, Clone)]
        pub struct $name(pub AuthenticatedUser);

        impl FromRequestParts<AppState> for $name {
            type Rejection = AppError;

            async fn from_request_parts(
                parts: &mut axum::http::request::Parts,
                state: &AppState,
            ) -> Result<Self, Self::Rejection> {
                let au = AuthenticatedUser::from_request_parts(parts, state).await?;
                if au.rank() >= crate::state::Role::$name.rank() {
                    Ok($name(au))
                } else {
                    Err(AppError::Forbidden)
                }
            }
        }
    };
}

role_extractor!(Viewer);
role_extractor!(Operator);
role_extractor!(Admin);

/// Reads the first value of a named cookie from the `Cookie` header(s).
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for raw in headers.get_all(axum::http::header::COOKIE) {
        let raw = raw.to_str().ok()?;
        for pair in raw.split(';') {
            let mut it = pair.trim().splitn(2, '=');
            match it.next() {
                Some(k) if k.trim() == name => {
                    return it.next().map(|v| v.trim().to_string());
                }
                _ => continue,
            }
        }
    }
    None
}

/// Best-effort client IP, forwarded-proxy aware: the RIGHT-MOST entry of
/// `X-Forwarded-For` (the hop appended by our own reverse proxy), else
/// `X-Real-IP`, else `"unknown"`. The shipped nginx config appends the real
/// client address, so the last entry cannot be spoofed by the client, unlike
/// the left-most entry. Used for audit rows and rate-limit keys.
pub fn client_ip(headers: &HeaderMap) -> String {
    for header in ["x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(header).and_then(|v| v.to_str().ok()) {
            let candidate = v.rsplit(',').next().unwrap_or("").trim();
            if !candidate.is_empty() {
                return candidate.to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Global layer handler: resolves the session cookie and attaches
/// [`AuthContext`] (user + session) to the request extensions. Missing,
/// unknown, expired or revoked sessions leave the request anonymous — lookup
/// failures must never fail closed here, matching the original behaviour.
///
/// Attach with `axum::middleware::from_fn_with_state(state.clone(), load_session)`.
pub async fn load_session(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(sid) = cookie_value(req.headers(), &state.config.session_cookie_name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return next.run(req).await;
    };

    // Unknown sessions are anonymous.
    let Ok(Some(session)) = crate::storage::sessions::get(&state.db, &sid).await else {
        return next.run(req).await;
    };
    // Expired or revoked sessions are anonymous too; an unparseable expiry
    // counts as expired (fail closed on the timestamp only).
    let expired = timeutil::parse(&session.expires_at)
        .map(|exp| chrono::Utc::now() > exp)
        .unwrap_or(true);
    if session.revoked_at.is_some() || expired {
        return next.run(req).await;
    }

    let Ok(user) = crate::storage::users::get_by_id(&state.db, &session.user_id).await else {
        return next.run(req).await;
    };

    // Sliding "last seen" bookkeeping; failure must not break the request.
    let _ = crate::storage::sessions::touch(&state.db, &session.id).await;

    req.extensions_mut().insert(AuthContext { user, session });
    next.run(req).await
}

/// Global layer handler: double-submit CSRF enforcement for unsafe methods
/// (POST/PUT/PATCH/DELETE) whenever a valid session is present. Requests
/// without a session pass through (e.g. login itself).
pub async fn csrf(req: Request, next: Next) -> Response {
    if !matches!(req.method().as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        return next.run(req).await;
    }
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return next.run(req).await;
    };
    let header_token = req
        .headers()
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    if header_token.is_empty()
        || ctx.session.csrf_token.is_empty()
        || header_token != ctx.session.csrf_token
    {
        return AppError::CsrfRequired.into_response();
    }
    next.run(req).await
}

/// Layer-style guard: any authenticated user.
pub async fn require_auth(req: Request, next: Next) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return AppError::Unauthorized.into_response();
    }
    next.run(req).await
}

/// Layer-style guard requiring at least `min_role` ("viewer"/"operator"/
/// "admin"). Unknown roles rank below viewer and are denied. Usage:
///
/// ```ignore
/// use axum::middleware;
/// .route_layer(middleware::from_fn(move |req, next| async move {
///     require_role("admin", req, next).await
/// }))
/// ```
pub async fn require_role(min_role: &'static str, req: Request, next: Next) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return AppError::Unauthorized.into_response();
    };
    let min_rank = crate::state::Role::parse(min_role).map(|r| r.rank()).unwrap_or(0);
    let rank = crate::state::Role::parse(&ctx.user.role).map(|r| r.rank()).unwrap_or(0);
    if rank < min_rank {
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}

static LOGIN_LIMITER: LazyLock<RateLimiter> =
    LazyLock::new(|| RateLimiter::new(10, std::time::Duration::from_secs(60)));

pub fn login_limiter() -> &'static RateLimiter {
    &LOGIN_LIMITER
}

/// A precomputed argon2id hash of a throwaway password. Verifying against it
/// on unknown emails equalizes response timing between "unknown email" and
/// "wrong password", killing the enumeration side-channel the Go version had.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    crate::security::password::hash_password("rivet-timing-equalizer")
        .expect("static dummy password is valid")
});

pub fn dummy_password_hash() -> &'static str {
    &DUMMY_HASH
}


#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::extract::Request;
    use axum::http::StatusCode;
    use crate::security::secrets::SecretBox;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    async fn app_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("auth_test.db").display()
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
        let state = AppState {
            db,
            secret_box: Arc::new(SecretBox::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config,
        };
        (dir, state)
    }

    /// Creates a `role` user + a live session, returning the session id.
    async fn seed_user_with_session(state: &AppState, email: &str, role: &str) -> String {
        let hash = crate::security::password::hash_password("correct-horse-battery").unwrap();
        let user =
            crate::storage::users::create(&state.db, email, &hash, "viewer").await.unwrap();
        let stored_role = if role == "weird" { "superuser" } else { role };
        sqlx::query("UPDATE users SET role = ? WHERE id = ?")
            .bind(stored_role)
            .bind(&user.id)
            .execute(&state.db)
            .await
            .unwrap();
        let sess = crate::storage::sessions::create(
            &state.db,
            &user.id,
            chrono::Duration::hours(1),
            None,
            None,
        )
        .await
        .unwrap();
        sess.id
    }

    async fn csrf_of(state: &AppState, sid: &str) -> String {
        crate::storage::sessions::get(&state.db, sid)
            .await
            .unwrap()
            .unwrap()
            .csrf_token
    }

    fn authed_req(uri: &str, sid: &str, csrf: &str) -> Request {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", format!("rivet_session={sid}"));
        if !csrf.is_empty() {
            b = b.header("x-csrf-token", csrf);
        }
        b.body(Body::empty()).unwrap()
    }

    /// Probe router with the same global stack the parent attaches:
    /// load_session -> csrf, plus one POST /probe guarded by `min_role`.
    fn probe_router(state: AppState, min_role: &'static str) -> Router {
        Router::new()
            .route(
                "/probe",
                post(|| async { "ok" }).route_layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| async move {
                        require_role(min_role, req, next).await
                    },
                )),
            )
            .layer(axum::middleware::from_fn(csrf))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                load_session,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn rbac_matrix() {
        let (_dir, state) = app_state().await;

        struct Row {
            role: &'static str,
            min: &'static str,
            expected: StatusCode,
        }
        let matrix = [
            Row { role: "viewer", min: "viewer", expected: StatusCode::OK },
            Row { role: "operator", min: "viewer", expected: StatusCode::OK },
            Row { role: "admin", min: "viewer", expected: StatusCode::OK },
            Row { role: "viewer", min: "operator", expected: StatusCode::FORBIDDEN },
            Row { role: "operator", min: "operator", expected: StatusCode::OK },
            Row { role: "admin", min: "operator", expected: StatusCode::OK },
            Row { role: "viewer", min: "admin", expected: StatusCode::FORBIDDEN },
            Row { role: "operator", min: "admin", expected: StatusCode::FORBIDDEN },
            Row { role: "admin", min: "admin", expected: StatusCode::OK },
            // Unknown stored role: denied even for the lowest requirement.
            Row { role: "weird", min: "viewer", expected: StatusCode::FORBIDDEN },
        ];
        for (i, row) in matrix.iter().enumerate() {
            let sid = seed_user_with_session(
                &state,
                &format!("{}{i}@x.test", row.role),
                row.role,
            )
            .await;
            let token = csrf_of(&state, &sid).await;
            let resp = probe_router(state.clone(), row.min)
                .oneshot(authed_req("/probe", &sid, &token))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                row.expected,
                "role={} min={}",
                row.role,
                row.min
            );
        }
    }

    #[tokio::test]
    async fn unauthenticated_is_unauthorized() {
        let (_dir, state) = app_state().await;
        let resp = probe_router(state, "viewer")
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"],
            "unauthorized"
        );
    }

    #[tokio::test]
    async fn expired_and_revoked_sessions_are_anonymous() {
        let (_dir, state) = app_state().await;

        let sid = seed_user_with_session(&state, "expired@x.test", "viewer").await;
        sqlx::query(
            "UPDATE sessions SET expires_at = '2000-01-01T00:00:00.000000000Z' WHERE id = ?",
        )
        .bind(&sid)
        .execute(&state.db)
        .await
        .unwrap();
        let resp = probe_router(state.clone(), "viewer")
            .oneshot(authed_req("/probe", &sid, ""))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "expired session must be anonymous");

        let sid2 = seed_user_with_session(&state, "revoked@x.test", "viewer").await;
        crate::storage::sessions::revoke(&state.db, &sid2).await.unwrap();
        let resp = probe_router(state.clone(), "viewer")
            .oneshot(authed_req("/probe", &sid2, ""))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "revoked session must be anonymous");
    }

    #[tokio::test]
    async fn csrf_enforced_on_mutations_only() {
        let (_dir, state) = app_state().await;
        let sid = seed_user_with_session(&state, "csrf@x.test", "viewer").await;
        let token = csrf_of(&state, &sid).await;
        let app = probe_router(state, "viewer");

        // Missing token on mutation -> 403 csrf_required.
        let resp = app.clone().oneshot(authed_req("/probe", &sid, "")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"],
            "csrf_required"
        );

        // Wrong token -> 403 as well.
        let resp = app.clone().oneshot(authed_req("/probe", &sid, "nope")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Correct token -> through to the handler.
        let resp = app.clone().oneshot(authed_req("/probe", &sid, &token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Safe methods are exempt from the CSRF gate entirely.
        let get = Request::builder()
            .method("GET")
            .uri("/probe")
            .header("cookie", format!("rivet_session={sid}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(get).await.unwrap();
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn requests_without_session_skip_csrf_but_fail_auth() {
        let (_dir, state) = app_state().await;
        let resp = probe_router(state, "viewer")
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "require_role fires, not csrf");
    }

    #[test]
    fn cookie_value_parses_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "a=1; rivet_session=tok%20en ; b=2".parse().unwrap(),
        );
        assert_eq!(cookie_value(&h, "a"), Some("1".into()));
        // Session ids are base64 tokens; the parser is deliberately
        // value-transparent (no percent-decoding), matching Go's cookie reader
        // closely enough for opaque tokens.
        assert_eq!(cookie_value(&h, "rivet_session"), Some("tok%20en".into()));
        assert_eq!(cookie_value(&h, "missing"), None);
    }

    #[test]
    fn client_ip_prefers_forwarded_chain() {
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h), "unknown");
        h.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&h), "10.0.0.1");
        h.remove("x-forwarded-for");
        h.insert("x-real-ip", "198.51.100.2".parse().unwrap());
        assert_eq!(client_ip(&h), "198.51.100.2");
    }

    #[test]
    fn dummy_hash_verifies_and_rejects() {
        assert!(crate::security::password::verify_password("rivet-timing-equalizer", dummy_password_hash()).unwrap());
        assert!(!crate::security::password::verify_password("something-else-long-enough", dummy_password_hash()).unwrap());
    }
}
