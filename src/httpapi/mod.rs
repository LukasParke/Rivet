//! HTTP API assembly: nests all route modules under /api/v1 and applies the
//! global middleware order: setup gate → session load → CSRF.

use axum::routing::get;
use axum::Router;

use crate::httpapi::middleware::{auth, setup_gate};
use crate::state::AppState;

pub mod middleware;
pub mod routes;

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
        // Layer order (last added = outermost): gate → session → CSRF.
        .layer(axum::middleware::from_fn(auth::csrf))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::load_session))
        .layer(gate.middleware())
        .with_state(state)
}
