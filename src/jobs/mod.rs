//! Durable job pipeline: queue, worker, and acquisition handlers.
//!
//! Job kinds: `search_run`, `acquisition_enqueue`, `acquisition_monitor`,
//! `acquisition_verify`, `acquisition_import`, `acquisition_recover`,
//! `acquisition_rescout`.
//!
//! Handler contract (fixes the original chain-liveness defect):
//! - Handlers MUST propagate continuation-enqueue failures as handler errors
//!   so the worker retries and re-drives the chain. Never `let _ = enqueue`.
//! - Handlers are panic-isolated by the worker; a panic fails the job.
//! - Long handlers renew their lease via [`queue::renew_lease`].

pub mod handlers;
pub mod reconciler;
pub mod worker;

use serde::{Deserialize, Serialize};

/// Payload for `search_run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRunPayload {
    pub run_id: String,
    pub target_id: String,
}

/// Payload for `acquisition_enqueue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueuePayload {
    pub acquisition_id: String,
}

/// Payload for `acquisition_monitor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorPayload {
    pub acquisition_id: String,
    /// Consecutive polls where the torrent was not found in the client.
    #[serde(default)]
    pub not_found_polls: u32,
    /// Consecutive slow re-polls while the torrent sat paused.
    #[serde(default)]
    pub paused_polls: u32,
}

/// Payload for `acquisition_verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyPayload {
    pub acquisition_id: String,
}

/// Payload for `acquisition_import`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPayload {
    pub acquisition_id: String,
}

/// Payload for `acquisition_recover`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverPayload {
    pub acquisition_id: String,
}

/// Payload for `acquisition_rescout`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RescoutPayload {
    pub target_id: String,
    /// Acquisition id whose failure triggered this rescout.
    pub failed_acquisition_id: Option<String>,
}

/// Payload for `enrich_series`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichSeriesPayload {
    pub target_id: String,
}
