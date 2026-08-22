//! Durable job worker: claims jobs from the queue and runs their handlers on
//! panic-isolated tokio tasks.
//!
//! Fixes over the Go original (`internal/jobs/worker.go`):
//! - Handlers run inside `tokio::spawn`; a panic becomes a job failure with
//!   the panic message as `last_error`, never a process crash.
//! - A heartbeat renews the lease every 10s while a handler runs, so long
//!   jobs are not stolen mid-flight; losing ownership aborts the zombie.
//! - Attempt capping goes through [`crate::storage::jobs::fail_and_retry`],
//!   which fails finally at [`MAX_ATTEMPTS`].
//! - Expired leases are released periodically so crashed-worker jobs recover.
//! - Graceful stop via a `watch::Receiver<bool>` shutdown token.

use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio::task::JoinError;
use tokio::task::JoinHandle;

use crate::state::AppState;
use crate::storage::jobs::{self, ClaimOpts, Job};

/// How often the claim loop looks for runnable jobs.
const CLAIM_INTERVAL: Duration = Duration::from_millis(750);
/// Lease duration granted at claim time and renewed by heartbeats.
const LEASE_DURATION: Duration = Duration::from_secs(30);
/// Heartbeat period for lease renewal while a handler runs.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// How often expired leases are swept.
const LEASE_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Spawns the worker loop. Shutdown: send `true` on the watch channel; the
/// loop stops claiming and waits for in-flight handlers to finish.
pub fn spawn(state: AppState, shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    let owner = uuid::Uuid::new_v4().to_string();
    tokio::spawn(async move {
        run(state, owner, shutdown).await;
    })
}

async fn run(state: AppState, owner: String, mut shutdown: watch::Receiver<bool>) {
    let mut claim_tick = tokio::time::interval(CLAIM_INTERVAL);
    let mut sweep_tick = tokio::time::interval(LEASE_SWEEP_INTERVAL);
    sweep_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut in_flight: Vec<JoinHandle<()>> = Vec::new();

    tracing::info!(owner = %owner, "worker started");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = claim_tick.tick() => {
                match jobs::claim_one(&state.db, &ClaimOpts { lease_owner: owner.clone(), lease_duration: chrono::Duration::from_std(LEASE_DURATION).unwrap_or_else(|_| chrono::Duration::seconds(30)) }).await {
                    Ok(job) => {
                        state.metrics.inc_jobs_claimed();
                        crate::jobs::handlers::publish(
                            &state,
                            "job.claimed",
                            serde_json::json!({"jobId": job.id, "kind": job.kind, "attempt": job.attempt}),
                        );
                        let task = spawn_job_task(state.clone(), owner.clone(), job);
                        in_flight.retain(|h| !h.is_finished());
                        in_flight.push(task);
                    }
                    Err(jobs::QueueError::NoJob) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "claim job failed");
                    }
                }
            }
            _ = sweep_tick.tick() => {
                match jobs::release_expired_leases(&state.db).await {
                    Ok(n) if n > 0 => {
                        tracing::info!(released = n, "released expired leases");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "lease sweep failed"),
                }
            }
        }
    }

    // Graceful stop: wait for in-flight handlers.
    for handle in in_flight.drain(..) {
        let _ = handle.await;
    }
    tracing::info!(owner = %owner, "worker stopped");
}

fn spawn_job_task(state: AppState, owner: String, job: Job) -> JoinHandle<()> {
    tokio::spawn(async move {
        let task_state = state.clone();
        let task_job = job.clone();
        let dispatch_task =
            tokio::spawn(
                async move { crate::jobs::handlers::dispatch(&task_state, &task_job).await },
            );
        let heartbeat = heartbeat_loop(state.db.clone(), job.id.clone(), owner.clone());

        tokio::pin!(dispatch_task);
        tokio::select! {
            joined = &mut dispatch_task => {
                let result = join_result(joined);
                settle(&state, &owner, &job, result).await;
            }
            _ = heartbeat => {
                // Ownership was lost to another worker: abort the zombie so
                // it stops doing work it can no longer record.
                dispatch_task.abort();
                tracing::warn!(job_id = %job.id, "lease lost; aborting handler");
            }
        }
    })
}

/// Maps a finished dispatch task onto the handler result; panics become
/// `handler_panic: …` errors instead of process crashes.
fn join_result(joined: Result<Result<(), String>, JoinError>) -> Result<(), String> {
    match joined {
        Ok(result) => result,
        Err(join_err) => {
            let detail = panic_message(join_err);
            tracing::error!(panic = %detail, "handler panicked");
            Err(format!("handler_panic: {detail}"))
        }
    }
}

/// Extracts the human-readable payload from a panicked task's JoinError.
fn panic_message(join_err: JoinError) -> String {
    if !join_err.is_panic() {
        return join_err.to_string();
    }
    let payload = join_err.into_panic();
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

async fn heartbeat_loop(pool: SqlitePool, job_id: String, owner: String) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    tick.tick().await; // first tick fires immediately; skip it
    loop {
        tick.tick().await;
        match jobs::renew_lease(
            &pool,
            &job_id,
            &owner,
            chrono::Duration::from_std(LEASE_DURATION)
                .unwrap_or_else(|_| chrono::Duration::seconds(30)),
        )
        .await
        {
            Ok(()) => {}
            Err(jobs::QueueError::NotOwner) => return,
            Err(e) => {
                tracing::error!(job_id = %job_id, error = %e, "heartbeat renew failed");
                return;
            }
        }
    }
}

/// Terminal update after a handler finishes (or panics).
pub(crate) async fn settle(state: &AppState, owner: &str, job: &Job, result: Result<(), String>) {
    match result {
        Ok(()) => match jobs::complete(&state.db, &job.id, owner).await {
            Ok(()) => {
                crate::jobs::handlers::publish(
                    state,
                    "job.completed",
                    serde_json::json!({"jobId": job.id, "kind": job.kind, "attempt": job.attempt}),
                );
                state.metrics.inc_jobs_completed();
            }
            Err(e) => tracing::error!(job_id = %job.id, error = %e, "complete failed"),
        },
        Err(err_str) => {
            let attempt = job.attempt + 1;
            // fail_and_retry caps internally at MAX_ATTEMPTS → fail_final.
            match jobs::fail_and_retry(&state.db, &job.id, owner, attempt, &err_str).await {
                Ok(()) => {
                    crate::jobs::handlers::publish(
                        state,
                        "job.retry_scheduled",
                        serde_json::json!({
                            "jobId": job.id, "kind": job.kind,
                            "attempt": attempt, "error": err_str
                        }),
                    );
                    state.metrics.inc_jobs_retried();
                }
                Err(jobs::QueueError::NotOwner) => {
                    tracing::warn!(job_id = %job.id, "lost lease before failure recording");
                }
                Err(e) => tracing::error!(job_id = %job.id, error = %e, "fail_and_retry failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn test_state_with_shutdown() -> (tempfile::TempDir, AppState, watch::Sender<bool>) {
        let (dir, state) = testutil::test_state().await;
        let (tx, _rx) = watch::channel(false);
        (dir, state, tx)
    }

    #[tokio::test]
    async fn panic_isolation_fails_job_not_process() {
        let (_dir, state, _tx) = test_state_with_shutdown().await;

        let job = jobs::enqueue::<serde_json::Value>(&state.db, "test_panic", None, None)
            .await
            .unwrap();
        let owner = uuid::Uuid::new_v4().to_string();

        // Claim like the loop would, then drive the per-job pipeline.
        let claimed = jobs::claim_one(
            &state.db,
            &ClaimOpts {
                lease_owner: owner.clone(),
                lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                    .unwrap_or_else(|_| chrono::Duration::seconds(30)),
            },
        )
        .await
        .unwrap();
        assert_eq!(claimed.id, job.id);

        let dispatch_state = state.clone();
        let dispatch_job = claimed.clone();
        let dispatch_task = tokio::spawn(async move {
            crate::jobs::handlers::dispatch(&dispatch_state, &dispatch_job).await
        });
        let result = join_result(dispatch_task.await);
        let err = match &result {
            Err(e) => e.clone(),
            Ok(()) => panic!("expected panic error"),
        };
        assert!(err.contains("handler_panic"), "{err}");
        assert!(err.contains("intentional test panic"), "{err}");

        settle(&state, &owner, &claimed, result).await;

        let finished = jobs::get(&state.db, &job.id).await.unwrap().unwrap();
        assert_eq!(finished.state, "queued", "non-final attempt retries");
        assert!(
            finished
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("handler_panic"),
            "panic recorded as last_error"
        );
        assert_eq!(finished.attempt, 1);
    }

    #[tokio::test]
    async fn attempts_cap_at_max_and_fail_final() {
        let (_dir, state, _tx) = test_state_with_shutdown().await;
        let owner = uuid::Uuid::new_v4().to_string();

        let job = jobs::enqueue::<serde_json::Value>(&state.db, "unknown_kind", None, None)
            .await
            .unwrap();

        // Burn attempts 1..=MAX-1 through the real settle path.
        let mut current = job;
        for expected_attempt in 1..jobs::MAX_ATTEMPTS {
            let claimed = jobs::claim_one(
                &state.db,
                &ClaimOpts {
                    lease_owner: owner.clone(),
                    lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                        .unwrap_or_else(|_| chrono::Duration::seconds(30)),
                },
            )
            .await
            .unwrap();
            assert_eq!(claimed.id, current.id);
            current = claimed;

            let result: Result<(), String> = Err("unknown_job_kind".to_string());
            settle(&state, &owner, &current, result).await;
            // Retry scheduling applies exponential backoff (contract); pull
            // the job due again so the next claim can proceed immediately.
            sqlx::query("UPDATE jobs SET run_after = ? WHERE id = ?")
                .bind(crate::timeutil::now())
                .bind(&current.id)
                .execute(&state.db)
                .await
                .unwrap();

            let row = jobs::get(&state.db, &current.id).await.unwrap().unwrap();
            assert_eq!(row.state, "queued");
            assert_eq!(row.attempt, expected_attempt);
        }

        // Final attempt: state flips to failed, never queued again.
        let claimed = jobs::claim_one(
            &state.db,
            &ClaimOpts {
                lease_owner: owner.clone(),
                lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                    .unwrap_or_else(|_| chrono::Duration::seconds(30)),
            },
        )
        .await
        .unwrap();
        settle(
            &state,
            &owner,
            &claimed,
            Err("unknown_job_kind".to_string()),
        )
        .await;

        let row = jobs::get(&state.db, &current.id).await.unwrap().unwrap();
        assert_eq!(row.state, "failed");
        assert_eq!(row.attempt, jobs::MAX_ATTEMPTS);

        // A failed job is not claimable anymore.
        let next = jobs::claim_one(
            &state.db,
            &ClaimOpts {
                lease_owner: owner.clone(),
                lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                    .unwrap_or_else(|_| chrono::Duration::seconds(30)),
            },
        )
        .await;
        assert!(matches!(next, Err(jobs::QueueError::NoJob)));
    }

    #[tokio::test]
    async fn successful_job_completes() {
        let (_dir, state, _tx) = test_state_with_shutdown().await;
        let owner = uuid::Uuid::new_v4().to_string();

        // search_run with an invalid payload fails fast but deterministically;
        // instead verify completion with a job whose handler succeeds: none of
        // the production kinds succeed without external config, so exercise
        // settle's success path directly.
        let job = jobs::enqueue::<serde_json::Value>(&state.db, "acquisition_import", None, None)
            .await
            .unwrap();
        let claimed = jobs::claim_one(
            &state.db,
            &ClaimOpts {
                lease_owner: owner.clone(),
                lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                    .unwrap_or_else(|_| chrono::Duration::seconds(30)),
            },
        )
        .await
        .unwrap();
        assert_eq!(claimed.id, job.id);

        settle(&state, &owner, &claimed, Ok(())).await;
        let row = jobs::get(&state.db, &job.id).await.unwrap().unwrap();
        assert_eq!(row.state, "completed");
    }

    #[tokio::test]
    async fn shutdown_token_stops_the_loop() {
        let (_dir, state) = testutil::test_state().await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn(state.clone(), shutdown_rx);
        // Give the loop a moment, then signal shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(true).unwrap();

        // The worker task must terminate promptly.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker must not panic on shutdown");
    }

    #[tokio::test]
    async fn expired_leases_are_swept_back_to_claimable() {
        let (_dir, state, _tx) = test_state_with_shutdown().await;
        let stale_owner = uuid::Uuid::new_v4().to_string();

        let job = jobs::enqueue::<serde_json::Value>(&state.db, "unknown_kind", None, None)
            .await
            .unwrap();
        // Simulate a crashed worker: leased, expired, still 'queued'.
        sqlx::query("UPDATE jobs SET lease_owner = ?, lease_expires_at = ? WHERE id = ?")
            .bind(&stale_owner)
            .bind(crate::timeutil::fmt(
                chrono::Utc::now() - chrono::Duration::seconds(60),
            ))
            .bind(&job.id)
            .execute(&state.db)
            .await
            .unwrap();

        // Not claimable while the (expired) lease fields are set...
        let fresh_owner = uuid::Uuid::new_v4().to_string();
        let before = jobs::release_expired_leases(&state.db).await.unwrap();
        assert_eq!(before, 1);
        let claimed = jobs::claim_one(
            &state.db,
            &ClaimOpts {
                lease_owner: fresh_owner,
                lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                    .unwrap_or_else(|_| chrono::Duration::seconds(30)),
            },
        )
        .await
        .unwrap();
        assert_eq!(claimed.id, job.id);
    }

    /// Guards against accidental shared-state regressions: settle must be
    /// usable concurrently (it takes &AppState which is Clone).
    #[tokio::test]
    async fn concurrent_settles_do_not_interfere() {
        let (_dir, state, _tx) = test_state_with_shutdown().await;
        let owner = Arc::new(uuid::Uuid::new_v4().to_string());
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        // Claim up front so this test exercises concurrent *settle*, not the
        // queue's optimistic claim race (losers legitimately get NoJob).
        let mut claimed_jobs = Vec::new();
        for i in 0..4 {
            let job = jobs::enqueue::<serde_json::Value>(&state.db, "unknown_kind", None, None)
                .await
                .unwrap();
            let claimed = jobs::claim_one(
                &state.db,
                &ClaimOpts {
                    lease_owner: (*owner).clone(),
                    lease_duration: chrono::Duration::from_std(LEASE_DURATION)
                        .unwrap_or_else(|_| chrono::Duration::seconds(30)),
                },
            )
            .await
            .unwrap();
            assert_eq!(claimed.id, job.id);
            claimed_jobs.push((claimed, i));
        }
        let mut handles = Vec::new();
        for (claimed, i) in claimed_jobs {
            let st = state.clone();
            let own = owner.clone();
            let seen = Arc::clone(&seen);
            handles.push(tokio::spawn(async move {
                settle(&st, &own, &claimed, Err(format!("boom{i}"))).await;
                seen.lock().await.push(claimed.id);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(seen.lock().await.len(), 4);
    }
}
