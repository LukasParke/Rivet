//! Background reconciler: rescues acquisitions whose pipeline died mid-step
//! and drives retention pruning.
//!
//! Every [`SCAN_INTERVAL`] it scans for acquisitions stuck in a non-terminal
//! status with no queued job referencing them, and re-enqueues the appropriate
//! next step (or marks them needs_review when no step applies). Every hour it
//! invokes [`crate::storage::retention::run_retention`].
//!
//! This closes the original's silent-death gap: a crash between two handler
//! steps left acquisitions frozen forever because nothing re-derived the next
//! step from status alone.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::jobs::MonitorPayload;
use crate::state::AppState;

/// How often the stuck-acquisition scan runs.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How often retention pruning runs.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// An acquisition untouched this long counts as stuck.
const STALE_AFTER_MINUTES: i64 = 30;
/// Batch size per scan.
const SCAN_LIMIT: i64 = 200;

/// Terminal statuses excluded from reconciliation.
const TERMINAL_STATUSES: &[&str] = &["failed", "needs_review", "removed", "imported"];

/// Spawns the reconciler loop. Shutdown: send `true` on the watch channel.
pub fn spawn(state: AppState, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut scan_tick = tokio::time::interval(SCAN_INTERVAL);
        let mut retention_tick = tokio::time::interval(RETENTION_INTERVAL);
        // Avoid firing both immediately at startup; a small delay is fine.
        scan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        retention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                _ = scan_tick.tick() => {
                    match run_scan(&state).await {
                        Ok(n) if n > 0 => tracing::info!(reconciled = n, "reconciler rescued acquisitions"),
                        Ok(_) => {}
                        Err(e) => tracing::error!(error = %e, "reconciler scan failed"),
                    }
                }
                _ = retention_tick.tick() => {
                    match crate::storage::retention::run_retention(&state.db).await {
                        Ok(counts) => tracing::info!(
                            jobs = counts.jobs,
                            sessions = counts.sessions,
                            audit_events = counts.audit_events,
                            release_candidates = counts.release_candidates,
                            "retention prune complete"
                        ),
                        Err(e) => tracing::error!(error = %e, "retention prune failed"),
                    }
                }
            }
        }
        tracing::info!("reconciler stopped");
    })
}

/// One reconciliation pass over stale non-terminal acquisitions. Returns how
/// many acquisitions were acted upon.
pub async fn run_scan(state: &AppState) -> Result<usize, String> {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(STALE_AFTER_MINUTES);
    let stale = crate::storage::acquisitions::list_stale_nonterminal(&state.db, cutoff, SCAN_LIMIT)
        .await
        .map_err(|e| e.to_string())?;

    let mut acted = 0;
    for a in stale {
        if has_active_job(state, &a.id).await? {
            continue;
        }
        match a.status.as_str() {
            "pending" => {
                enqueue_next(
                    state,
                    "acquisition_enqueue",
                    &serde_json::json!({
                        "acquisitionId": a.id
                    }),
                )
                .await?;
                acted += 1;
            }
            "enqueued" | "downloading" | "paused" => {
                enqueue_next(
                    state,
                    "acquisition_monitor",
                    &serde_json::to_value(MonitorPayload {
                        acquisition_id: a.id.clone(),
                        not_found_polls: 0,
                        paused_polls: 0,
                    })
                    .map_err(|e| e.to_string())?,
                )
                .await?;
                acted += 1;
            }
            "stalled" => {
                enqueue_next(
                    state,
                    "acquisition_recover",
                    &serde_json::json!({
                        "acquisitionId": a.id
                    }),
                )
                .await?;
                acted += 1;
            }
            "completed" | "verifying" => {
                enqueue_next(
                    state,
                    "acquisition_verify",
                    &serde_json::json!({
                        "acquisitionId": a.id
                    }),
                )
                .await?;
                acted += 1;
            }
            "verified" => {
                enqueue_next(
                    state,
                    "acquisition_import",
                    &serde_json::json!({
                        "acquisitionId": a.id
                    }),
                )
                .await?;
                acted += 1;
            }
            other if !TERMINAL_STATUSES.contains(&other) => {
                // Unknown non-terminal status: hand to a human instead of
                // guessing a continuation.
                crate::storage::acquisitions::mark_needs_review(
                    &state.db,
                    &a.id,
                    "reconciled",
                    None,
                    &format!("unhandled_status:{other}"),
                )
                .await
                .map_err(|e| e.to_string())?;
                acted += 1;
            }
            _ => {}
        }
    }
    Ok(acted)
}

async fn has_active_job(state: &AppState, acquisition_id: &str) -> Result<bool, String> {
    let pattern = format!("%{acquisition_id}%");
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE state = 'queued' AND payload_json LIKE ?")
            .bind(&pattern)
            .fetch_one(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    Ok(row.0 > 0)
}

async fn enqueue_next(
    state: &AppState,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), String> {
    crate::storage::jobs::enqueue(&state.db, kind, Some(payload), None)
        .await
        .map_err(|e| format!("reconciler_enqueue_failed({kind}): {e}"))?;
    state.metrics.inc_jobs_enqueued();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;

    /// Seeds target/run/candidate scaffolding plus one acquisition with the
    /// given status; returns the acquisition id.
    async fn seed_acquisition(state: &AppState, id: &str, status: &str, age_minutes: i64) {
        // Scaffolding ids are derived from the acquisition id so several
        // acquisitions can coexist (candidate_id is UNIQUE).
        let target_id = format!("t-{id}");
        let run_id = format!("r-{id}");
        let cand_id = format!("c-{id}");
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, created_at, updated_at)
             VALUES (?, 'movie', 'tmdb', '550', 'wanted', ?, ?)",
        )
        .bind(&target_id)
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_runs (id, target_id, status) VALUES (?, ?, 'running')",
        )
        .bind(&run_id)
        .bind(&target_id)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO release_candidates (id, search_run_id, indexer_name, raw_json, created_at)
             VALUES (?, ?, 'ix', '{}', ?)",
        )
        .bind(&cand_id)
        .bind(&run_id)
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        let old = crate::timeutil::fmt(chrono::Utc::now() - chrono::Duration::minutes(age_minutes));
        sqlx::query(
            "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, created_at, updated_at)
             VALUES (?, ?, ?, 'torrent', ?, 'qbittorrent', 'default', ?, ?)",
        )
        .bind(id)
        .bind(&cand_id)
        .bind(&target_id)
        .bind(status)
        .bind(&old)
        .bind(&old)
        .execute(&state.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stuck_pending_acquisition_gets_re_enqueued() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, "a1", "pending", 60).await;

        let n = run_scan(&state).await.unwrap();
        assert_eq!(n, 1);

        let kinds: Vec<(String,)> = sqlx::query_as("SELECT kind FROM jobs WHERE state = 'queued'")
            .fetch_all(&state.db)
            .await
            .unwrap();
        assert_eq!(kinds, vec![("acquisition_enqueue".to_string(),)]);
    }

    #[tokio::test]
    async fn each_status_maps_to_its_next_step() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, "a-mon", "downloading", 60).await;
        seed_acquisition(&state, "a-stall", "stalled", 60).await;
        seed_acquisition(&state, "a-ver", "completed", 60).await;
        seed_acquisition(&state, "a-imp", "verified", 60).await;

        let n = run_scan(&state).await.unwrap();
        assert_eq!(n, 4);

        for kind in [
            "acquisition_monitor",
            "acquisition_recover",
            "acquisition_verify",
            "acquisition_import",
        ] {
            let c: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE kind = ?")
                .bind(kind)
                .fetch_one(&state.db)
                .await
                .unwrap();
            assert_eq!(c.0, 1, "{kind}");
        }
    }

    #[tokio::test]
    async fn terminal_and_fresh_acquisitions_are_untouched() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, "a-done", "imported", 600).await;
        seed_acquisition(&state, "a-review", "needs_review", 600).await;
        seed_acquisition(&state, "a-fresh", "downloading", 1).await;

        let n = run_scan(&state).await.unwrap();
        assert_eq!(n, 0);

        let c: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM jobs")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(c.0, 0);
    }

    #[tokio::test]
    async fn acquisitions_with_active_jobs_are_skipped() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, "a1", "downloading", 600).await;

        // A live monitor job already references this acquisition.
        crate::storage::jobs::enqueue(
            &state.db,
            "acquisition_monitor",
            Some(&MonitorPayload {
                acquisition_id: "a1".into(),
                not_found_polls: 0,
                paused_polls: 0,
            }),
            None,
        )
        .await
        .unwrap();

        let n = run_scan(&state).await.unwrap();
        assert_eq!(n, 0);

        // Only the pre-existing job is present.
        let c: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM jobs")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(c.0, 1);
    }

    #[tokio::test]
    async fn unknown_status_goes_to_needs_review() {
        let (_dir, state) = testutil::test_state().await;
        seed_acquisition(&state, "a1", "mystery_status", 600).await;

        let n = run_scan(&state).await.unwrap();
        assert_eq!(n, 1);

        let a = crate::storage::acquisitions::get(&state.db, "a1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.status, "needs_review");
    }

    #[tokio::test]
    async fn retention_entry_point_succeeds() {
        let (_dir, state) = testutil::test_state().await;
        crate::storage::jobs::enqueue::<serde_json::Value>(&state.db, "search_run", None, None)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET state = 'failed' WHERE kind = 'search_run'")
            .execute(&state.db)
            .await
            .unwrap();

        // Same entry point the hourly loop tick uses; must succeed and report
        // counts without error.
        let _counts = crate::storage::retention::run_retention(&state.db)
            .await
            .unwrap();
    }
}
