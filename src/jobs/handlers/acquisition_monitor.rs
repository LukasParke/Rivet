//! `acquisition_monitor`: poll the download client until the torrent
//! completes, stalls, or gives up.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_monitor.go`):
//! - Consecutive not-found polls are bounded (`not_found_polls`, give up at
//!   [`NOT_FOUND_POLL_LIMIT`]) instead of re-enqueueing forever.
//! - Paused torrents do not loop forever: slow re-polls every
//!   [`PAUSED_POLL_INTERVAL_SECS`] seconds, at most [`PAUSED_POLL_LIMIT`]
//!   times, then needs_review.
//! - Every continuation enqueue propagates its error.
//! - Long polls keep their lease via the worker's heartbeat task.

use crate::adapters::{is_completed, is_paused};
use crate::jobs::handlers::{load_torrent_client, publish};
use crate::jobs::{MonitorPayload, RecoverPayload, RescoutPayload, VerifyPayload};
use crate::state::AppState;
use crate::storage::acquisitions;
use crate::storage::jobs::{self, Job};

/// Interval between normal progress polls.
const POLL_EVERY_SECS: i64 = 10;
/// No progress-change for this long marks the acquisition stalled.
const STALL_AFTER_MINUTES: i64 = 30;
/// Give up after this many consecutive not-found polls.
pub const NOT_FOUND_POLL_LIMIT: u32 = 30;
/// Re-poll paused torrents slowly.
pub const PAUSED_POLL_INTERVAL_SECS: i64 = 60;
/// ...but at most this many times before handing to review.
pub const PAUSED_POLL_LIMIT: u32 = 60;

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: MonitorPayload = parse_payload(job)?;
    if payload.acquisition_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }

    let a = acquisitions::get(&state.db, &payload.acquisition_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("acquisition_not_found")?;

    let external_id = a
        .external_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing_hash")?;

    // Bounds are checked BEFORE any client call so give-up is deterministic
    // even when the client is unreachable.
    if payload.not_found_polls >= NOT_FOUND_POLL_LIMIT {
        acquisitions::mark_needs_review(&state.db, &a.id, "not_found", None, "torrent_not_found")
            .await
            .map_err(|e| e.to_string())?;
        publish(
            state,
            "download.needs_review",
            serde_json::json!({"acquisitionId": a.id, "reason": "torrent_not_found", "notFoundPolls": payload.not_found_polls}),
        );
        return rescout(state, &a.target_id, &a.id).await;
    }
    if payload.paused_polls >= PAUSED_POLL_LIMIT {
        acquisitions::mark_needs_review(&state.db, &a.id, "paused", None, "paused_too_long")
            .await
            .map_err(|e| e.to_string())?;
        publish(
            state,
            "download.needs_review",
            serde_json::json!({"acquisitionId": a.id, "reason": "paused_too_long", "pausedPolls": payload.paused_polls}),
        );
        return rescout(state, &a.target_id, &a.id).await;
    }

    let cli = load_torrent_client(state, &a.client_kind).await?;
    let info = match cli.get_info(&external_id).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            // Not visible yet: retry later with an incremented bound counter.
            let next = MonitorPayload {
                acquisition_id: a.id.clone(),
                not_found_polls: payload.not_found_polls + 1,
                paused_polls: payload.paused_polls,
            };
            publish(
                state,
                "download.progress",
                serde_json::json!({
                    "acquisitionId": a.id,
                    "candidateId": a.candidate_id,
                    "targetId": a.target_id,
                    "state": "not_found",
                    "notFoundPolls": next.not_found_polls
                }),
            );
            return requeue_monitor(state, &next, POLL_EVERY_SECS).await;
        }
        Err(e) => {
            publish(
                state,
                "download.failed",
                serde_json::json!({
                    "acquisitionId": a.id,
                    "candidateId": a.candidate_id,
                    "targetId": a.target_id,
                    "clientKind": a.client_kind,
                    "clientName": a.client_name,
                    "error": e.to_string()
                }),
            );
            return Err(e.to_string());
        }
    };

    let now = chrono::Utc::now();
    if a.status == "pending" || a.status == "enqueued" {
        acquisitions::mark_downloading(&state.db, &a.id, now, now)
            .await
            .map_err(|e| e.to_string())?;
    }

    let progress = info.progress;
    let old_progress = a.progress.unwrap_or(0.0);

    if is_paused(&info) {
        acquisitions::update_progress(
            &state.db,
            &a.id,
            "paused",
            Some(progress),
            info.content_path.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())?;
        let next = MonitorPayload {
            acquisition_id: a.id.clone(),
            not_found_polls: payload.not_found_polls,
            paused_polls: payload.paused_polls + 1,
        };
        return requeue_monitor(state, &next, PAUSED_POLL_INTERVAL_SECS).await;
    }

    acquisitions::update_progress(
        &state.db,
        &a.id,
        "downloading",
        Some(progress),
        info.content_path.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    if progress > old_progress + 0.001 {
        acquisitions::touch_progress_changed(&state.db, &a.id, now)
            .await
            .map_err(|e| e.to_string())?;
    }
    publish(
        state,
        "download.progress",
        serde_json::json!({
            "acquisitionId": a.id,
            "candidateId": a.candidate_id,
            "targetId": a.target_id,
            "clientKind": a.client_kind,
            "clientName": a.client_name,
            "hash": external_id,
            "state": info.state,
            "progress": progress,
            "contentPath": info.content_path,
            "name": info.name
        }),
    );

    if is_completed(&info) || progress >= 0.999 {
        let mut cp = info.content_path.clone().unwrap_or_default();
        if cp.trim().is_empty() {
            cp = std::path::Path::new(info.name.trim())
                .to_string_lossy()
                .into_owned();
        }
        acquisitions::mark_completed(&state.db, &a.id, Some(cp.trim()))
            .await
            .map_err(|e| e.to_string())?;
        publish(
            state,
            "download.completed",
            serde_json::json!({
                "acquisitionId": a.id,
                "candidateId": a.candidate_id,
                "targetId": a.target_id,
                "clientKind": a.client_kind,
                "clientName": a.client_name,
                "contentPath": cp.trim()
            }),
        );
        jobs::enqueue(
            &state.db,
            "acquisition_verify",
            Some(&VerifyPayload {
                acquisition_id: a.id.clone(),
            }),
            Some(now + chrono::Duration::seconds(1)),
        )
        .await
        .map_err(|e| format!("enqueue_verify_failed: {e}"))?;
        state.metrics.inc_jobs_enqueued();
        return Ok(());
    }

    // Stall detection: no progress change for too long.
    let stalled = a
        .progress_changed_at
        .as_deref()
        .and_then(crate::timeutil::parse)
        .is_some_and(|t| now - t > chrono::Duration::minutes(STALL_AFTER_MINUTES));
    if stalled {
        acquisitions::mark_stalled(&state.db, &a.id)
            .await
            .map_err(|e| e.to_string())?;
        publish(
            state,
            "download.stalled",
            serde_json::json!({
                "acquisitionId": a.id,
                "targetId": a.target_id,
                "progress": progress,
                "state": info.state
            }),
        );
        jobs::enqueue(
            &state.db,
            "acquisition_recover",
            Some(&RecoverPayload {
                acquisition_id: a.id.clone(),
            }),
            Some(now + chrono::Duration::seconds(1)),
        )
        .await
        .map_err(|e| format!("enqueue_recover_failed: {e}"))?;
        state.metrics.inc_jobs_enqueued();
        return Ok(());
    }

    // Not done: schedule the next poll.
    requeue_monitor(
        state,
        &MonitorPayload {
            acquisition_id: a.id.clone(),
            not_found_polls: 0,
            paused_polls: 0,
        },
        POLL_EVERY_SECS,
    )
    .await
}

async fn requeue_monitor(
    state: &AppState,
    payload: &MonitorPayload,
    delay_secs: i64,
) -> Result<(), String> {
    jobs::enqueue(
        &state.db,
        "acquisition_monitor",
        Some(payload),
        Some(chrono::Utc::now() + chrono::Duration::seconds(delay_secs)),
    )
    .await
    .map_err(|e| format!("enqueue_monitor_failed: {e}"))?;
    state.metrics.inc_jobs_enqueued();
    Ok(())
}

async fn rescout(state: &AppState, target_id: &str, failed_id: &str) -> Result<(), String> {
    jobs::enqueue(
        &state.db,
        "acquisition_rescout",
        Some(&RescoutPayload {
            target_id: target_id.to_string(),
            failed_acquisition_id: Some(failed_id.to_string()),
        }),
        Some(chrono::Utc::now() + chrono::Duration::seconds(2)),
    )
    .await
    .map_err(|e| format!("enqueue_rescout_failed: {e}"))?;
    state.metrics.inc_jobs_enqueued();
    Ok(())
}

fn parse_payload(job: &Job) -> Result<MonitorPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;

    async fn seed_acquisition(state: &AppState, external_id: Option<&str>) -> String {
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, created_at, updated_at)
             VALUES ('t1', 'movie', 'tmdb', '550', 'wanted', ?, ?)",
        )
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_runs (id, target_id, status) VALUES ('r1', 't1', 'running')",
        )
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO release_candidates (id, search_run_id, indexer_name, raw_json, created_at)
             VALUES ('c1', 'r1', 'ix', '{}', ?)",
        )
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, external_id, created_at, updated_at)
             VALUES ('a1', 'c1', 't1', 'torrent', 'enqueued', 'qbittorrent', 'default', ?, ?, ?)",
        )
        .bind(external_id)
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        "a1".to_string()
    }

    async fn monitor_job(state: &AppState, payload: &MonitorPayload) -> Job {
        jobs::enqueue(&state.db, "acquisition_monitor", Some(payload), None)
            .await
            .unwrap()
    }

    #[test]
    fn bounds_are_constant_and_sane() {
        assert_eq!(NOT_FOUND_POLL_LIMIT, 30);
        assert_eq!(PAUSED_POLL_LIMIT, 60);
        assert_eq!(PAUSED_POLL_INTERVAL_SECS, 60);
    }

    #[tokio::test]
    async fn not_found_gives_up_at_limit() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, Some("hash")).await;

        // At the limit the handler gives up without touching any client.
        let payload = MonitorPayload {
            acquisition_id: "a1".into(),
            not_found_polls: NOT_FOUND_POLL_LIMIT,
            paused_polls: 0,
        };
        let job = monitor_job(&state, &payload).await;
        handle(&state, &job).await.unwrap();

        let a = crate::storage::acquisitions::get(&state.db, "a1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.status, "needs_review");
        // Rescout continuation was enqueued.
        let n: (i64,) =
            sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_rescout'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(n.0, 1);
    }

    #[tokio::test]
    async fn paused_gives_up_at_limit() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, Some("hash")).await;

        let payload = MonitorPayload {
            acquisition_id: "a1".into(),
            not_found_polls: 0,
            paused_polls: PAUSED_POLL_LIMIT,
        };
        let job = monitor_job(&state, &payload).await;
        handle(&state, &job).await.unwrap();

        let a = crate::storage::acquisitions::get(&state.db, "a1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.status, "needs_review");
    }
    #[tokio::test]
    async fn missing_hash_fails_without_client() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, None).await;
        let payload = MonitorPayload {
            acquisition_id: "a1".into(),
            not_found_polls: 0,
            paused_polls: 0,
        };
        let job = monitor_job(&state, &payload).await;
        assert_eq!(handle(&state, &job).await.unwrap_err(), "missing_hash");
    }
}
