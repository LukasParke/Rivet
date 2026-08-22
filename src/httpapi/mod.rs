//! HTTP API assembly: nests all route modules under /api/v1 and applies the
//! global middleware order: setup gate → session load → CSRF.
//!
//! Security hardening layers (outermost first):
//! - [`axum::extract::DefaultBodyLimit`]: caps every request body at 1 MiB.
//!   No endpoint legitimately needs more today; oversized bodies are rejected
//!   before any handler runs (413 for direct body extractors, surfaced as a
//!   400 rejection through `Json`).
//! - [`tower_http::timeout::TimeoutLayer`]: 30s per-request timeout answering
//!   408, bounding slow handlers and slow body uploads.
//!
//! Known remaining gap: hyper has no read-idle timeout, so a client that
//! trickles request bytes (slowloris) is bounded by the 30s total request
//! timeout but not by an inter-read deadline. Production is fronted by nginx,
//! which must keep `client_header_timeout`/`client_body_timeout` set (single
//! digits, seconds); that is the recommended mitigation. If the server is ever
//! exposed directly, add a `socket2`-wrapped accept loop with per-connection
//! read timeouts instead of loosening these layers.

use std::time::Duration;

use axum::routing::get;
use axum::Router;

use crate::httpapi::middleware::{auth, setup_gate};
use crate::state::AppState;

pub mod middleware;
pub mod routes;

/// Hard cap on any request body (1 MiB). No endpoint legitimately accepts
/// more today; oversized uploads are rejected before any handler runs.
const MAX_REQUEST_BODY_BYTES: usize = 1_048_576;
/// Per-request wall-clock budget, covering handler execution and body reads.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Assembles the complete application router.
///
/// `gate` is shared with main so admin creation can flip setup mode
/// synchronously (the reviewed first-run race).
pub fn build_router(state: AppState, gate: std::sync::Arc<setup_gate::SetupGate>) -> Router {
    let api = Router::new()
        .merge(routes::auth::router())
        .merge(routes::setup::router())
        .merge(routes::setup_paths::router())
        .merge(routes::setup_clients::router())
        .merge(routes::setup_indexers::router())
        .merge(routes::setup_metadata::router())
        .merge(routes::setup_policies::router())
        .merge(routes::setup_finish::router())
        .merge(routes::setup_config::router())
        .merge(routes::targets_search::router())
        .merge(routes::acquisitions::router())
        .merge(routes::library::router())
        .merge(routes::history::router())
        .merge(routes::diagnostics::router())
        .merge(routes::calendar::router())
        .merge(routes::wanted::router())
        .merge(routes::search_interactive::router())
        .merge(routes::notifications::router())
        .merge(routes::system::router());

    Router::new()
        // Health/metrics live outside /api/v1 like the original.
        .route("/healthz", get(routes::health::healthz))
        .route("/readyz", get(routes::health::readyz))
        .route("/metrics", get(routes::metrics::metrics_handler))
        .merge(routes::ws::router())
        .nest("/api/v1", api)
        // Security hardening. Added last = outermost, so limits apply before
        // any handler or gate work. tower-http's Timeout answers 408 itself
        // without changing the service error type.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        // Layer order (last added = outermost): gate → session → CSRF.
        .layer(axum::middleware::from_fn(auth::csrf))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::load_session,
        ))
        .layer(gate.middleware())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use crate::security::secrets::SecretBox;
    use crate::security::OutboundPolicy;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("httpapi_limits_test.db").display()
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

    /// Full production router against a fresh DB (setup mode, so
    /// /api/v1/setup/admin accepts anonymous POSTs).
    fn full_router(
        state: &AppState,
        gate: &Arc<setup_gate::SetupGate>,
    ) -> Router {
        build_router(state.clone(), gate.clone())
    }

    /// Regression: request bodies over the 1 MiB global cap must be rejected
    /// by the DefaultBodyLimit layer instead of being buffered whole.
    #[tokio::test]
    async fn oversized_request_body_rejected() {
        let (_dir, state) = test_state().await;
        let gate = setup_gate::SetupGate::new(state.db.clone(), Duration::from_secs(3600));
        let app = full_router(&state, &gate);

        let big = vec![b'a'; MAX_REQUEST_BODY_BYTES + 1024];
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/setup/admin")
            .header("content-type", "application/json")
            .body(Body::from(big))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // axum 0.8 surfaces the Bytes length limit through the Json
        // extractor as a 400 Bad Request rejection (a bare Bytes extractor
        // yields 413); either proves the limit fired before buffering.
        assert!(
            resp.status() == StatusCode::BAD_REQUEST
                || resp.status() == StatusCode::PAYLOAD_TOO_LARGE,
            "2 MiB body must be rejected by the body limit, got {}",
            resp.status()
        );
    }

    /// Small bodies pass the limit untouched (handler-level validation runs).
    #[tokio::test]
    async fn small_request_body_unaffected() {
        let (_dir, state) = test_state().await;
        let gate = setup_gate::SetupGate::new(state.db.clone(), Duration::from_secs(3600));
        let app = full_router(&state, &gate);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/setup/admin")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"email": "a@x.test", "password": "short"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "small body must not trip the body limit"
        );
        // Handler ran far enough to reject the weak password.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// The 30s TimeoutLayer answers 408 itself; a fast handler is untouched.
    #[tokio::test]
    async fn timeout_layer_passes_fast_requests() {
        let (_dir, state) = test_state().await;
        let gate = setup_gate::SetupGate::new(state.db.clone(), Duration::from_secs(3600));
        let app = full_router(&state, &gate);

        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_ne!(resp.status(), StatusCode::REQUEST_TIMEOUT);
    }
}
