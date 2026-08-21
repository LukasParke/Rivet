//! Shared application state handed to every route and job handler.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::metrics::Metrics;
use crate::security::secrets::SecretBox;
use crate::ws::hub::Hub;

/// User roles, ordered viewer < operator < admin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }

    pub fn rank(&self) -> u8 {
        match self {
            Role::Admin => 3,
            Role::Operator => 2,
            Role::Viewer => 1,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    /// Single shared SecretBox — the original created independent instances for
    /// the API and the worker, which in dev produced different ephemeral keys
    /// and made wizard-saved credentials undecryptable by jobs.
    pub secret_box: Arc<SecretBox>,
    pub policy: crate::security::OutboundPolicy,
    pub hub: Arc<Hub>,
    pub metrics: Arc<Metrics>,
    pub config: Arc<Config>,
}
