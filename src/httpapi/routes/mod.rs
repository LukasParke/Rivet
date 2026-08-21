//! HTTP route modules. Each module exports exactly `pub fn router() ->
//! axum::Router<AppState>` containing only its method+path routes; the parent
//! (`crate::httpapi`) nests them all under `/api/v1` and applies the global
//! layer order: setup_gate -> load_session -> csrf.

pub mod acquisitions;
pub mod diagnostics;
pub mod health;
pub mod metrics;
pub mod targets_search;
pub mod ws;

// Auth + setup wizard (ApiAuthSetup).
pub mod auth;
pub mod setup;
pub mod setup_clients;
pub mod setup_config;
pub mod setup_finish;
pub mod setup_indexers;
pub mod setup_metadata;
pub mod setup_paths;
pub mod setup_policies;
