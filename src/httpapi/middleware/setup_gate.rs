//! Setup-mode gate.
//!
//! While fewer than one user exists, the API only serves `/api/v1/setup/*`,
//! `/healthz`, `/readyz` and `/metrics`; everything else gets
//! `403 {"error":"setup_required", ..., "setupMode":true}` so the frontend
//! redirects into the setup wizard.
//!
//! # Reviewed race, fixed here
//!
//! The original refreshed `COUNT(users)==0` on a 2s ticker only. Right after
//! `POST /api/v1/setup/admin` created the admin, the frontend immediately
//! attempted `POST /api/v1/auth/login` — still inside the refresh window — and
//! got `setup_required`, losing the race. [`SetupGate::notify_user_created`]
//! flips the atomic flag synchronously inside the admin-creation handler, so
//! the very next request sees setup mode off (the ticker remains authoritative
//! afterwards and merely re-confirms).
//!
//! The flag starts `true`; an initial refresh runs before traffic is served
//! and the periodic task keeps it honest against out-of-band user deletion.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::future::Future;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::time::MissedTickBehavior;

/// Process-wide shared instance, registered once at boot by [`init_shared`]
/// and read by route handlers via [`shared`]. A runtime-input registry cannot
/// be a compile-time constant, hence `OnceLock`.
static SHARED: std::sync::Mutex<Option<Arc<SetupGate>>> = std::sync::Mutex::new(None);

pub struct SetupGate {
    setup_mode: AtomicBool,
    refresh_every: Duration,
    pool: SqlitePool,
    /// Guards against spawning a second periodic task if `init_shared` is
    /// called again (first registration wins).
    refresher_started: AtomicBool,
}

impl SetupGate {
    /// Creates the gate in setup mode (`refresh_every <= 0` defaults to 2s,
    /// matching the original). Call [`SetupGate::start_refresher`] before
    /// serving traffic.
    pub fn new(pool: SqlitePool, refresh_every: Duration) -> Arc<Self> {
        let refresh_every = if refresh_every.is_zero() {
            Duration::from_secs(2)
        } else {
            refresh_every
        };
        Arc::new(Self {
            setup_mode: AtomicBool::new(true),
            refresher_started: AtomicBool::new(false),
            refresh_every,
            pool,
        })
    }

    /// Runs the initial count-based refresh synchronously, then spawns the
    /// periodic refresher. Counting errors leave the previous value in place
    /// (the original skipped the tick too). Idempotent: only the first call
    /// spawns the task.
    pub async fn start_refresher(self: &Arc<Self>) {
        if self
            .refresher_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.refresh_once().await;
        let gate = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(gate.refresh_every);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                gate.refresh_once().await;
            }
        });
    }

    async fn refresh_once(&self) {
        if let Ok(n) = crate::storage::users::count_users(&self.pool).await {
            self.setup_mode.store(n == 0, Ordering::SeqCst);
        }
    }

    pub fn is_setup_mode(&self) -> bool {
        self.setup_mode.load(Ordering::SeqCst)
    }

    /// THE RACE FIX: flips setup mode off synchronously the moment the first
    /// (admin) user has been created, so the frontend's immediate login cannot
    /// lose a refresh-window race. Idempotent.
    pub fn notify_user_created(&self) {
        self.setup_mode.store(false, Ordering::SeqCst);
    }

    /// Tower/axum layer enforcing the gate. The parent attaches it globally
    /// under `/api/v1`, first in the layer order:
    /// `Router::new()....layer(gate.middleware())`
    pub fn middleware(self: &Arc<Self>) -> SetupGateLayer {
        SetupGateLayer {
            gate: Arc::clone(self),
        }
    }
}

/// [`tower::Layer`] producing [`SetupGateService`]; cheap to clone.
#[derive(Clone)]
pub struct SetupGateLayer {
    gate: Arc<SetupGate>,
}

impl<S> tower::Layer<S> for SetupGateLayer {
    type Service = SetupGateService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SetupGateService {
            inner,
            gate: Arc::clone(&self.gate),
        }
    }
}

#[derive(Clone)]
pub struct SetupGateService<S> {
    inner: S,
    gate: Arc<SetupGate>,
}

impl<S> tower::Service<Request> for SetupGateService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let allowed = !self.gate.is_setup_mode() || path_allowed(req.uri().path());
        // Replace-and-clone so `inner` can be moved into the future while the
        // service itself stays callable.
        let spare = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, spare);
        if allowed {
            Box::pin(inner.call(req))
        } else {
            drop(inner);
            Box::pin(async { Ok(setup_required_response()) })
        }
    }
}

fn path_allowed(path: &str) -> bool {
    // Always allow health endpoints…
    matches!(path, "/healthz" | "/readyz" | "/metrics")
        // …and every setup endpoint. Setup stays active until /finish so the
        // wizard can keep configuring after admin creation; only the
        // synchronous flag flip opens the rest of the app.
        || path.starts_with("/api/v1/setup/")
}

fn setup_required_response() -> Response {
    (
        axum::http::StatusCode::FORBIDDEN,
        axum::Json(json!({
            "error": "setup_required",
            "message": "setup is required before using the app",
            "setupMode": true,
        })),
    )
        .into_response()
}

/// Creates the process-wide gate, registers it and starts its refresher.
/// Must be called once at boot before the router is built; later calls return
/// the already-registered gate without spawning another refresher.
pub async fn init_shared(pool: SqlitePool, refresh_every: Duration) -> Arc<SetupGate> {
    // Clone any existing gate out under a short lock; never hold the guard
    // across the refresher await.
    let existing = SHARED
        .lock()
        .expect("setup gate registry")
        .as_ref()
        .map(Arc::clone);
    if let Some(gate) = existing {
        gate.start_refresher().await;
        return gate;
    }
    let gate = SetupGate::new(pool, refresh_every);
    gate.start_refresher().await;
    let mut slot = SHARED.lock().expect("setup gate registry");
    match slot.as_ref() {
        // Another task won the registration race; prefer the winner.
        Some(winner) => Arc::clone(winner),
        None => {
            *slot = Some(Arc::clone(&gate));
            gate
        }
    }
}

/// The gate registered at boot, if any. Route handlers treat `None` as
/// standalone/test construction and fall back to direct user counts.
pub fn shared() -> Option<Arc<SetupGate>> {
    SHARED
        .lock()
        .expect("setup gate registry")
        .as_ref()
        .map(Arc::clone)
}

/// Test isolation: drops the registered gate so a fresh test can install one
/// bound to its own database. Never used in production paths.
#[cfg(test)]
pub fn reset_shared_for_tests() {
    *SHARED.lock().expect("setup gate registry") = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use tower::ServiceExt;

    async fn db_pool(dir: &tempfile::TempDir) -> SqlitePool {
        let pool = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("gate_test.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn race_fix_notify_is_immediate() {
        let dir = tempfile::tempdir().unwrap();
        let gate = SetupGate::new(db_pool(&dir).await, Duration::from_secs(3600));
        assert!(gate.is_setup_mode(), "fresh DB => setup mode");

        // No waiting for any refresh window: the flip is synchronous.
        gate.notify_user_created();
        assert!(
            !gate.is_setup_mode(),
            "notify_user_created must take effect immediately"
        );
    }

    #[tokio::test]
    async fn initial_refresh_disables_when_users_exist() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db_pool(&dir).await;
        let hash = crate::security::password::hash_password("some-long-password").unwrap();
        crate::storage::users::create(&pool, "admin@x.test", &hash, "admin")
            .await
            .unwrap();

        let gate = SetupGate::new(pool, Duration::from_secs(3600));
        assert!(gate.is_setup_mode(), "starts optimistic");
        gate.start_refresher().await;
        assert!(
            !gate.is_setup_mode(),
            "initial refresh sees the existing user"
        );
    }

    fn gated_router(gate: &Arc<SetupGate>) -> Router {
        Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .route(
                "/api/v1/setup/status",
                axum::routing::get(|| async { "ok" }),
            )
            .route("/api/v1/auth/me", axum::routing::get(|| async { "ok" }))
            .layer(gate.middleware())
    }

    async fn status_of(router: Router, uri: &str) -> StatusCode {
        router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn middleware_allows_health_and_setup_only() {
        let dir = tempfile::tempdir().unwrap();
        let gate = SetupGate::new(db_pool(&dir).await, Duration::from_secs(3600));

        let app = gated_router(&gate);
        assert_eq!(status_of(app.clone(), "/healthz").await, StatusCode::OK);
        assert_ne!(
            status_of(app.clone(), "/metrics").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_of(app.clone(), "/api/v1/setup/status").await,
            StatusCode::OK
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(v["error"], "setup_required");
        assert_eq!(v["setupMode"], true);
    }

    #[tokio::test]
    async fn after_admin_creation_app_is_reachable_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let gate = SetupGate::new(db_pool(&dir).await, Duration::from_secs(3600));

        // Simulates exactly what routes/setup.rs POST /admin does on success.
        gate.notify_user_created();

        let resp = gated_router(&gate)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "login must not lose the refresh race right after admin creation"
        );
    }

    #[tokio::test]
    async fn shared_registry_roundtrip_and_double_init_safety() {
        reset_shared_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let pool = db_pool(&dir).await;
        let first = init_shared(pool.clone(), Duration::from_secs(3600)).await;
        // Second init with a DIFFERENT pool returns the same gate and does not
        // spawn a second refresher.
        let second = init_shared(pool, Duration::from_secs(3600)).await;
        assert!(Arc::ptr_eq(&first, &second));

        let gate = shared().expect("init_shared registers the shared instance");
        assert!(gate.is_setup_mode());
        gate.notify_user_created();
        assert!(!shared().unwrap().is_setup_mode());
        reset_shared_for_tests();
    }
}
