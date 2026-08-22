//! Rivet — media automation control plane (Rust implementation).
//!
//! Module layout mirrors the original Go service:
//! - `config`    environment-driven configuration
//! - `error`     shared error type with stable machine-readable codes
//! - `security`  argon2id passwords, AES-256-GCM secret box, rate limiter, SSRF-safe outbound
//! - `storage`   SQLite pool, transactional migrations, domain queries
//! - `ws`        role-gated WebSocket hub
//! - `jobs`      durable queue + worker + acquisition pipeline handlers
//! - `adapters`  indexers / downloaders / usenet clients / metadata providers
//! - `httpapi`   axum routes and middleware (contract-compatible with openapi.yaml)

// Wire-format structs intentionally mirror the Go API's JSON field names
// (ID, CreatedAt, apiKey, ...) so the SvelteKit client and openapi.yaml stay
// byte-compatible across the port.
#![allow(non_snake_case)]
// Adapter constructors return Arc<dyn Trait> factories rather than Self by
// design (callers depend on the erased interface).
#![allow(clippy::new_ret_no_self)]
// Tests assert error variants on Result<Arc<dyn Trait>, _>: expect_err would
// require Debug on the erased trait object.
#![cfg_attr(test, allow(clippy::err_expect))]

pub mod adapters;
pub mod config;
pub mod domain;
pub mod error;
pub mod httpapi;
pub mod jobs;
pub mod logging;
pub mod metrics;
pub mod notify;
pub mod pipeline;
pub mod security;
pub mod state;
pub mod storage;
pub mod timeutil;
pub mod ws;
