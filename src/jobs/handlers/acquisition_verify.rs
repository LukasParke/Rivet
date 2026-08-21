//! `acquisition_verify`: pick + verify the downloaded media, record evidence,
//! quarantine failures, and hand off to import.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_verify.go`):
//! - Rescout is enqueued ONCE, only when the verify job reaches its final
//!   attempt — the original enqueued a rescout on every failed attempt,
//!   multiplying search load by MAX_ATTEMPTS.
//! - Verify evidence JSON is always recorded (success and failure).
//! - ffprobe is never invoked in dry-run; option-like filenames are guarded
//!   in [`crate::pipeline::verify::probe_args`].
//! - Continuation enqueues propagate their errors.

use crate::jobs::handlers::{load_setup_paths, publish};
use crate::jobs::VerifyPayload;
use crate::jobs::{ImportPayload, RescoutPayload};
use crate::pipeline::{import, verify};
use crate::state::AppState;
use crate::storage::acquisitions;
use crate::storage::jobs::{self, Job, MAX_ATTEMPTS};

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: VerifyPayload = parse_payload(job)?;
    if payload.acquisition_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }

    let a = acquisitions::get(&state.db, &payload.acquisition_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("acquisition_not_found")?;

    let mut content_path = a.content_path.clone().unwrap_or_default();
    if state.config.dry_run {
        // Prefer a local fixture for read-only verification/import.
        if let Some(fp) = state
            .config
            .dry_run_fixture_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            content_path = fp.to_string();
        }
        if content_path.is_empty() {
            content_path = "dryrun://content".to_string();
        }
    } else if content_path.trim().is_empty() {
        return Err("missing_content_path".to_string());
    }

    let paths = load_setup_paths(state).await?;
    if paths.quarantine_path.trim().is_empty() {
        return Err("quarantine_path_missing".to_string());
    }

    acquisitions::mark_verifying(&state.db, &a.id)
        .await
        .map_err(|e| e.to_string())?;
    publish(
        state,
        "verify.started",
        serde_json::json!({
            "acquisitionId": a.id,
            "contentPath": content_path,
            "dryRun": state.config.dry_run
        }),
    );

    // No real file/dir to inspect (common in dry run): short-circuit OK.
    if state.config.dry_run && content_path.starts_with("dryrun://") {
        set_evidence(
            state,
            &a.id,
            &serde_json::json!({
                "dryRun": true,
                "picked": content_path,
                "result": {"ok": true, "reason": "dry_run_no_fixture"}
            }),
        )
        .await?;
        acquisitions::mark_verified(&state.db, &a.id, &content_path)
            .await
            .map_err(|e| e.to_string())?;
        publish(
            state,
            "verify.completed",
            serde_json::json!({"acquisitionId": a.id, "verifiedPath": content_path, "dryRun": true}),
        );
        return enqueue_import(state, &a.id).await;
    }

    let final_attempt = job.attempt + 1 >= MAX_ATTEMPTS;

    let picked = match import::pick_best_media_file(&content_path) {
        Ok(p) => p,
        Err(e) => {
            set_evidence(
                state,
                &a.id,
                &serde_json::json!({"dryRun": state.config.dry_run, "contentPath": content_path, "error": e}),
            )
            .await?;
            acquisitions::mark_needs_review(&state.db, &a.id, "failed", None, &e)
                .await
                .map_err(|db| db.to_string())?;
            publish(
                state,
                "verify.failed",
                serde_json::json!({"acquisitionId": a.id, "error": e.to_string(), "dryRun": state.config.dry_run}),
            );
            maybe_rescout(state, final_attempt, &a.target_id, &a.id).await?;
            return Err(e.to_string());
        }
    };

    let mut res = match verify::verify_file(&picked) {
        Ok(r) => r,
        Err(e) => {
            set_evidence(
                state,
                &a.id,
                &serde_json::json!({"dryRun": state.config.dry_run, "picked": picked, "error": e.to_string()}),
            )
            .await?;
            acquisitions::mark_needs_review(&state.db, &a.id, "error", None, &e.to_string())
                .await
                .map_err(|db| db.to_string())?;
            publish(
                state,
                "verify.failed",
                serde_json::json!({"acquisitionId": a.id, "error": e.to_string(), "dryRun": state.config.dry_run}),
            );
            maybe_rescout(state, final_attempt, &a.target_id, &a.id).await?;
            return Err(e.to_string());
        }
    };

    // Optional deeper inspection via ffprobe — never in dry-run.
    let mut probe_value = serde_json::Value::Null;
    if !state.config.dry_run {
        let require_ffprobe = state
            .config
            .ffprobe_path
            .as_deref()
            .map(str::trim)
            .is_some_and(|p| !p.is_empty());
        match verify::probe_ffprobe(state.config.ffprobe_path.as_deref(), &picked).await {
            Ok(Some(sum)) => {
                probe_value = serde_json::to_value(&sum).unwrap_or(serde_json::Value::Null);
                if require_ffprobe
                    && sum.duration_seconds > 0.0
                    && sum.duration_seconds < 5.0 * 60.0
                {
                    res.ok = false;
                    res.reason = "duration_too_short".into();
                }
            }
            Ok(None) => {}
            Err(e) => {
                probe_value = serde_json::json!({"error": e});
                if require_ffprobe {
                    res.ok = false;
                    res.reason = "ffprobe_failed".into();
                }
            }
        }
    }

    set_evidence(
        state,
        &a.id,
        &serde_json::json!({
            "dryRun": state.config.dry_run,
            "picked": picked,
            "result": res,
            "ffprobe": probe_value
        }),
    )
    .await?;

    if !res.ok {
        let quarantined: Option<String> = if state.config.dry_run {
            Some(format!(
                "dryrun://quarantine/{}",
                std::path::Path::new(&res.file_path)
                    .file_name()
                    .map(|b| b.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ))
        } else {
            match import::quarantine(&picked, &paths.quarantine_path) {
                Ok(q) => Some(q),
                Err(e) => {
                    tracing::warn!(error = %e, "quarantine failed");
                    None
                }
            }
        };
        acquisitions::mark_needs_review(
            &state.db,
            &a.id,
            &res.reason,
            quarantined.as_deref(),
            &res.reason,
        )
        .await
        .map_err(|e| e.to_string())?;
        publish(
            state,
            "verify.failed",
            serde_json::json!({
                "acquisitionId": a.id,
                "reason": res.reason,
                "quarantinedPath": quarantined,
                "dryRun": state.config.dry_run
            }),
        );
        maybe_rescout(state, final_attempt, &a.target_id, &a.id).await?;
        return Err("verify_failed".to_string());
    }

    acquisitions::mark_verified(&state.db, &a.id, &picked)
        .await
        .map_err(|e| e.to_string())?;
    publish(
        state,
        "verify.completed",
        serde_json::json!({
            "acquisitionId": a.id,
            "verifiedPath": picked,
            "dryRun": state.config.dry_run
        }),
    );

    enqueue_import(state, &a.id).await
}

async fn set_evidence(
    state: &AppState,
    id: &str,
    evidence: &serde_json::Value,
) -> Result<(), String> {
    acquisitions::set_verify_evidence(
        &state.db,
        id,
        Some(&serde_json::to_string(evidence).unwrap_or_default()),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Enqueues rescout exactly once per acquisition failure chain: only when this
/// verify job is on its final attempt (no further retries will run).
async fn maybe_rescout(
    state: &AppState,
    final_attempt: bool,
    target_id: &str,
    failed_id: &str,
) -> Result<(), String> {
    if !final_attempt {
        return Ok(());
    }
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

async fn enqueue_import(state: &AppState, acquisition_id: &str) -> Result<(), String> {
    jobs::enqueue(
        &state.db,
        "acquisition_import",
        Some(&ImportPayload {
            acquisition_id: acquisition_id.to_string(),
        }),
        Some(chrono::Utc::now() + chrono::Duration::seconds(1)),
    )
    .await
    .map_err(|e| format!("enqueue_import_failed: {e}"))?;
    state.metrics.inc_jobs_enqueued();
    Ok(())
}

fn parse_payload(job: &Job) -> Result<VerifyPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;
    use std::sync::Arc;

    async fn seed(state: &AppState, content_path: Option<&str>) -> String {
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
            "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, content_path, created_at, updated_at)
             VALUES ('a1', 'c1', 't1', 'torrent', 'completed', 'qbittorrent', 'default', ?, ?, ?)",
        )
        .bind(content_path)
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        settings_paths(state).await;
        "a1".to_string()
    }

    async fn settings_paths(state: &AppState) {
        crate::storage::settings::upsert(
            &state.db,
            "setup.paths",
            r#"{"downloadsCompletedPath":"/tmp","libraryRoots":["/tmp/lib"],"quarantinePath":"/tmp/quarantine-root"}"#,
        )
        .await
        .unwrap();
    }

    async fn verify_job(state: &AppState) -> Job {
        jobs::enqueue(
            &state.db,
            "acquisition_verify",
            Some(&VerifyPayload {
                acquisition_id: "a1".into(),
            }),
            None,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn dryrun_synthetic_short_circuits_to_import() {
        let (_dir, state) = testutil::test_state().await;
        seed(&state, None).await;
        let job = verify_job(&state).await;

        handle(&state, &job).await.unwrap();

        let a = acquisitions::get(&state.db, "a1").await.unwrap().unwrap();
        assert_eq!(a.status, "verified");
        assert_eq!(a.verified_path.as_deref(), Some("dryrun://content"));
        assert!(a
            .verify_evidence_json
            .as_deref()
            .unwrap_or("")
            .contains("dry_run_no_fixture"));
        let n: (i64,) =
            sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_import'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(n.0, 1);
    }

    #[tokio::test]
    async fn missing_content_path_fails_outside_dry_run() {
        let (_dir, mut state) = testutil::test_state().await;
        let cfg = Arc::clone(&state.config);
        let mut cfg = (*cfg).clone();
        cfg.dry_run = false;
        state.config = Arc::new(cfg);
        seed(&state, None).await;
        let job = verify_job(&state).await;

        assert_eq!(
            handle(&state, &job).await.unwrap_err(),
            "missing_content_path"
        );
    }

    #[tokio::test]
    async fn rescout_enqueued_only_on_final_attempt() {
        let (_dir, state) = testutil::test_state().await;
        // An empty content directory makes pick_best_media_file fail with the
        // contract error "no_media_found" (a missing path yields a raw fs
        // error, matching the Go os.Stat behaviour).
        let empty = tempfile::tempdir().unwrap();
        seed(&state, Some(empty.path().to_str().unwrap())).await;

        // Non-final attempt: fails, records evidence, NO rescout.
        let job = verify_job(&state).await;
        let mut job = job;
        job.attempt = 0;
        let err = handle(&state, &job).await.unwrap_err();
        assert_eq!(err, "no_media_found");
        let n: (i64,) =
            sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_rescout'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(n.0, 0, "no rescout before final attempt");

        // Final attempt: rescout enqueued exactly once.
        let job = verify_job(&state).await;
        let mut job = job;
        job.attempt = MAX_ATTEMPTS - 1;
        handle(&state, &job).await.unwrap_err();
        let n: (i64,) =
            sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_rescout'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(n.0, 1);

        let a = acquisitions::get(&state.db, "a1").await.unwrap().unwrap();
        assert_eq!(a.status, "needs_review");
        assert!(a.verify_evidence_json.is_some());
    }
}
