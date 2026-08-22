//! HTTP route modules. Each module exports exactly `pub fn router() ->
//! axum::Router<AppState>` containing only its method+path routes; the parent
//! (`crate::httpapi`) merges them under `/api/v1` and applies the global
//! layer order: setup_gate -> load_session -> csrf.
//!
//! Registry owned by the parent session — modules are added here, never
//! edited by individual feature agents.

pub mod acquisitions;
pub mod auth;
pub mod calendar;
pub mod diagnostics;
pub mod health;
pub mod history;
pub mod library;
pub mod metrics;
pub mod notifications;
pub mod search_interactive;
pub mod setup;
pub mod setup_clients;
pub mod setup_config;
pub mod setup_finish;
pub mod setup_indexers;
pub mod setup_metadata;
pub mod setup_paths;
pub mod setup_policies;
pub mod system;
pub mod targets_search;
pub mod wanted;
pub mod ws;
