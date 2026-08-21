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

pub mod adapters;
pub mod config;
pub mod error;
pub mod httpapi;
pub mod jobs;
pub mod logging;
pub mod metrics;
pub mod pipeline;
pub mod security;
pub mod state;
pub mod storage;
pub mod timeutil;
pub mod ws;
