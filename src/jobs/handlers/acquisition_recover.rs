//! `acquisition_recover`: run the recovery ladder (resume → reannounce →
//! recheck) on a stalled download.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_recover.go`):
//! - Transient infrastructure errors (network/auth while building or calling
//!   the client) do NOT burn ladder steps: they return an error so the job
//!   retries without incrementing `recovery_attempt`. Only genuine client-op
//!   rejections (unsupported op, invalid config, parse failures) advance the
//!   ladder.
//! - The rescout continuation at escalation propagates its enqueue error.

use crate::adapters::AdapterError;
use crate::jobs::handlers::{load_torrent_client, publish};
use crate::jobs::RecoverPayload;
use crate::jobs::{MonitorPayload, RescoutPayload};
use crate::state::AppState;
use crate::storage::acquisitions;
use crate::storage::jobs::{self, Job};

/// Ladder attempts before escalating to needs_review + rescout.
const ESCALATE_AFTER_ATTEMPTS: i64 = 4;

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: RecoverPayload = parse_payload(job)?;
    if payload.acquisition_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }

    let a = acquisitions::get(&state.db, &payload.acquisition_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("acquisition_not_found")?;

    if a.client_kind != "qbittorrent" && a.client_kind != "dryrun" {
        return Err("unsupported_client_kind".to_string());
    }
    let external_id = a
        .external_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing_hash")?;

    // Building the client is infrastructure: failure here must NOT advance
    // the ladder — the job retry will rebuild it.
    let cli = load_torrent_client(state).await?;

    let ladder_step = a.recovery_attempt + 1;
    let op_result = match ladder_step {
        1 => cli.resume(&external_id).await,
        2 => cli.reannounce(&external_id).await,
        _ => cli.recheck(&external_id).await,
    };

    if let Err(op_err) = &op_result {
        match classify(op_err) {
            OpErrorKind::Transient => {
                // Network/auth trouble: retry the job, keep the ladder step.
                publish(
                    state,
                    "download.recovery_transient",
                    serde_json::json!({
                        "acquisitionId": a.id,
                        "attempt": ladder_step,
                        "error": op_err.to_string()
                    }),
                );
                return Err(op_err.to_string());
            }
            OpErrorKind::Rejection => {
                // The client refused the operation: burn a ladder step.
                acquisitions::increment_recovery_attempt(&state.db, &a.id)
                    .await
                    .map_err(|e| e.to_string())?;
                acquisitions::mark_failed(&state.db, &a.id, &op_err.to_string())
                    .await
                    .map_err(|e| e.to_string())?;
                publish(
                    state,
                    "download.recovery_failed",
                    serde_json::json!({
                        "acquisitionId": a.id,
                        "attempt": ladder_step,
                        "error": op_err.to_string()
                    }),
                );
                return Err(op_err.to_string());
            }
        }
    }

    let attempt = acquisitions::increment_recovery_attempt(&state.db, &a.id)
        .await
        .map_err(|e| e.to_string())?;
    publish(
        state,
        "download.recovery_attempt",
        serde_json::json!({"acquisitionId": a.id, "attempt": attempt}),
    );

    // After a few attempts, escalate to review and try an alternative.
    if attempt >= ESCALATE_AFTER_ATTEMPTS {
        acquisitions::mark_needs_review(
            &state.db,
            &a.id,
            "stalled",
            a.quarantined_path.as_deref(),
            "stalled",
        )
        .await
        .map_err(|e| e.to_string())?;
        publish(
            state,
            "download.needs_review",
            serde_json::json!({"acquisitionId": a.id, "reason": "stalled"}),
        );
        jobs::enqueue(
            &state.db,
            "acquisition_rescout",
            Some(&RescoutPayload {
                target_id: a.target_id.clone(),
                failed_acquisition_id: Some(a.id.clone()),
            }),
            Some(chrono::Utc::now() + chrono::Duration::seconds(1)),
        )
        .await
        .map_err(|e| format!("enqueue_rescout_failed: {e}"))?;
        state.metrics.inc_jobs_enqueued();
        return Ok(());
    }

    // Resume monitoring quickly.
    acquisitions::touch_progress_changed(&state.db, &a.id, chrono::Utc::now())
        .await
        .map_err(|e| e.to_string())?;
    jobs::enqueue(
        &state.db,
        "acquisition_monitor",
        Some(&MonitorPayload {
            acquisition_id: a.id.clone(),
            not_found_polls: 0,
            paused_polls: 0,
        }),
        Some(chrono::Utc::now() + chrono::Duration::seconds(5)),
    )
    .await
    .map_err(|e| format!("enqueue_monitor_failed: {e}"))?;
    state.metrics.inc_jobs_enqueued();
    publish(
        state,
        "download.recovery_scheduled",
        serde_json::json!({"acquisitionId": a.id, "attempt": attempt}),
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpErrorKind {
    /// Infrastructure trouble: transient, does not consume a ladder step.
    Transient,
    /// The client understood and rejected the operation.
    Rejection,
}

fn classify(err: &AdapterError) -> OpErrorKind {
    match err {
        AdapterError::Network(_) | AdapterError::Auth => OpErrorKind::Transient,
        AdapterError::InvalidConfig(_) | AdapterError::Unsupported(_) | AdapterError::Parse(_) => {
            OpErrorKind::Rejection
        }
    }
}

fn parse_payload(job: &Job) -> Result<crate::jobs::RecoverPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_splits_transient_from_rejection() {
        assert_eq!(
            classify(&AdapterError::Network("timeout".into())),
            OpErrorKind::Transient
        );
        assert_eq!(classify(&AdapterError::Auth), OpErrorKind::Transient);
        assert_eq!(
            classify(&AdapterError::Unsupported("reannounce".into())),
            OpErrorKind::Rejection
        );
        assert_eq!(
            classify(&AdapterError::InvalidConfig("bad".into())),
            OpErrorKind::Rejection
        );
        assert_eq!(
            classify(&AdapterError::Parse("junk".into())),
            OpErrorKind::Rejection
        );
    }
}
