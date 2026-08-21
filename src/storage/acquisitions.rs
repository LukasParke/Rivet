//! Acquisition storage: the unified torrent/usenet download lifecycle.
//!
//! Every status mutation is a single statement keyed on `id` and bumps
//! `updated_at`. Timestamps are stored via [`crate::timeutil`] so they remain
//! fixed-width RFC3339-nanos and lexicographically ordered.

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::SqlitePool;

use crate::timeutil;


#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Acquisition {
    pub id: String,
    pub candidate_id: String,
    pub target_id: String,
    pub kind: String,
    pub status: String,
    pub client_kind: String,
    pub client_name: String,
    pub source_url: Option<String>,
    pub external_id: Option<String>,
    pub content_path: Option<String>,
    pub verified_path: Option<String>,
    pub quarantined_path: Option<String>,
    pub verify_status: Option<String>,
    pub verify_evidence_json: Option<String>,
    pub imported_path: Option<String>,
    pub completed_at: Option<String>,
    pub imported_at: Option<String>,
    pub progress: Option<f64>,
    pub download_started_at: Option<String>,
    pub progress_changed_at: Option<String>,
    pub stalled_at: Option<String>,
    /// Column is nullable (added in 0006); NULL decodes as 0.
    #[sqlx(default)]
    pub recovery_attempt: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AcquisitionError {
    #[error("candidateID and targetID required")]
    MissingId,
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Clamps list limits exactly like the Go implementation: out-of-range values
/// fall back to 50.
fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 || limit > 200 {
        50
    } else {
        limit
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub async fn create_acquisition(
    pool: &SqlitePool,
    candidate_id: &str,
    target_id: &str,
    kind: &str,
    client_kind: &str,
    client_name: &str,
) -> Result<Acquisition, AcquisitionError> {
    if candidate_id.is_empty() || target_id.is_empty() {
        return Err(AcquisitionError::MissingId);
    }
    let now = fmt_time(Utc::now());
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO acquisitions
           (id, candidate_id, target_id, kind, status, client_kind, client_name,
            external_id, last_error, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'pending', ?, ?, NULL, NULL, ?, ?)",
    )
    .bind(&id)
    .bind(candidate_id)
    .bind(target_id)
    .bind(kind)
    .bind(client_kind)
    .bind(client_name)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(Acquisition {
        id,
        candidate_id: candidate_id.to_string(),
        target_id: target_id.to_string(),
        kind: kind.to_string(),
        status: "pending".to_string(),
        client_kind: client_kind.to_string(),
        client_name: client_name.to_string(),
        source_url: None,
        external_id: None,
        content_path: None,
        verified_path: None,
        quarantined_path: None,
        verify_status: None,
        verify_evidence_json: None,
        imported_path: None,
        completed_at: None,
        imported_at: None,
        progress: None,
        download_started_at: None,
        progress_changed_at: None,
        stalled_at: None,
        recovery_attempt: 0,
        last_error: None,
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Acquisition>, sqlx::Error> {
    sqlx::query_as::<_, Acquisition>("SELECT * FROM acquisitions WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_by_candidate(
    pool: &SqlitePool,
    candidate_id: &str,
) -> Result<Option<Acquisition>, sqlx::Error> {
    sqlx::query_as::<_, Acquisition>("SELECT * FROM acquisitions WHERE candidate_id = ?")
        .bind(candidate_id)
        .fetch_optional(pool)
        .await
}

/// Lists newest-first with an optional exact-status filter.
pub async fn list(
    pool: &SqlitePool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<Acquisition>, sqlx::Error> {
    let limit = clamp_limit(limit);
    match status.filter(|s| !s.trim().is_empty()) {
        Some(status) => {
            sqlx::query_as::<_, Acquisition>(
                "SELECT * FROM acquisitions WHERE status = ? ORDER BY created_at DESC LIMIT ?",
            )
            .bind(status)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query_as::<_, Acquisition>(
                "SELECT * FROM acquisitions ORDER BY created_at DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
}

/// Marks the acquisition queued on a download client. Clears `last_error`.
pub async fn mark_enqueued(
    pool: &SqlitePool,
    id: &str,
    external_id: Option<&str>,
    source_url: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE acquisitions
         SET status = 'enqueued', external_id = ?, source_url = COALESCE(?, source_url),
             last_error = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(external_id)
    .bind(source_url)
    .bind(timeutil::now())
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Records a failure; keeps prior progress/content state for later retries.
pub async fn mark_failed(
    pool: &SqlitePool,
    id: &str,
    last_error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE acquisitions SET status = 'failed', last_error = ?, updated_at = ? WHERE id = ?")
        .bind(last_error)
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Generic progress poll from the download client. `content_path` only ever
/// moves forward (COALESCE), matching the Go behaviour; every call bumps
/// `progress_changed_at` so the reconciler sees liveness.
pub async fn update_progress(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    progress: Option<f64>,
    content_path: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions
         SET status = ?, progress = ?, content_path = COALESCE(?, content_path),
             progress_changed_at = ?, stalled_at = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(status)
    .bind(progress)
    .bind(content_path)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// First observation of an active download: pins start timestamps once.
pub async fn mark_downloading(
    pool: &SqlitePool,
    id: &str,
    download_started_at: DateTime<Utc>,
    progress_changed_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE acquisitions
         SET status = 'downloading',
             download_started_at = COALESCE(download_started_at, ?),
             progress_changed_at = COALESCE(progress_changed_at, ?),
             stalled_at = NULL,
             updated_at = ?
         WHERE id = ?",
    )
    .bind(fmt_time(download_started_at))
    .bind(fmt_time(progress_changed_at))
    .bind(timeutil::now())
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn touch_progress_changed(
    pool: &SqlitePool,
    id: &str,
    when: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE acquisitions SET progress_changed_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(fmt_time(when))
    .bind(timeutil::now())
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn mark_stalled(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query("UPDATE acquisitions SET status = 'stalled', stalled_at = ?, updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn clear_stalled(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE acquisitions SET stalled_at = NULL, updated_at = ? WHERE id = ? AND stalled_at IS NOT NULL",
    )
    .bind(timeutil::now())
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Atomically bumps the recovery counter and returns the new value in the same
/// statement — fixes the Go UPDATE-then-SELECT race where concurrent recoverers
/// could observe a stale count.
///
/// Returns [`sqlx::Error::RowNotFound`] when `id` does not exist.
pub async fn increment_recovery_attempt(pool: &SqlitePool, id: &str) -> Result<i64, sqlx::Error> {
    let (n,): (i64,) = sqlx::query_as(
        "UPDATE acquisitions
         SET recovery_attempt = COALESCE(recovery_attempt, 0) + 1, updated_at = ?
         WHERE id = ?
         RETURNING COALESCE(recovery_attempt, 0)",
    )
    .bind(timeutil::now())
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Sets the verification outcome. Pass `None` paths to leave them untouched is
/// NOT supported: callers set what they know (`verified_path` xor
/// `quarantined_path`) and clear the other explicitly.
pub async fn set_verify_status(
    pool: &SqlitePool,
    id: &str,
    verify_status: &str,
    verified_path: Option<&str>,
    quarantined_path: Option<&str>,
    verify_evidence_json: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions
         SET verify_status = ?, verified_path = ?, quarantined_path = ?,
             verify_evidence_json = COALESCE(?, verify_evidence_json), updated_at = ?
         WHERE id = ?",
    )
    .bind(verify_status)
    .bind(verified_path)
    .bind(quarantined_path)
    .bind(verify_evidence_json)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Verification started: acquisition enters `verifying`, clears prior errors.
pub async fn mark_verifying(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions SET status = 'verifying', verify_status = 'running', last_error = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Verification passed: pins the verified path, clears any quarantine, and
/// moves the acquisition into the nonterminal `verified` lifecycle status the
/// reconciler maps to `acquisition_import`.
pub async fn mark_verified(
    pool: &SqlitePool,
    id: &str,
    verified_path: &str,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions
         SET status = 'verified', verify_status = 'passed', verified_path = ?,
             quarantined_path = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(verified_path)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Verification needs a human: parks the row on the review queue with the
/// failure reason.
pub async fn mark_needs_review(
    pool: &SqlitePool,
    id: &str,
    verify_status: &str,
    quarantined_path: Option<&str>,
    err_str: &str,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions
         SET status = 'needs_review', verify_status = ?, quarantined_path = ?, last_error = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(verify_status)
    .bind(quarantined_path)
    .bind(err_str)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Attaches structured verification evidence without changing any status.
pub async fn set_verify_evidence(
    pool: &SqlitePool,
    id: &str,
    evidence_json: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE acquisitions SET verify_evidence_json = ?, updated_at = ? WHERE id = ?")
        .bind(evidence_json)
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn mark_completed(
    pool: &SqlitePool,
    id: &str,
    content_path: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions SET status = 'completed', completed_at = ?, content_path = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(content_path)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Marks a verified download as fully imported into the library.
/// `imported_at` is stamped server-side from [`timeutil::now`].
pub async fn mark_imported(
    pool: &SqlitePool,
    id: &str,
    imported_path: &str,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query(
        "UPDATE acquisitions SET status = 'imported', imported_at = ?, imported_path = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(imported_path)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Admin override: accept a quarantined/verifying acquisition as passed and
/// drop it off the review queue. Does not touch the filesystem.
pub async fn admin_accept_verified(
    pool: &SqlitePool,
    id: &str,
    verified_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE acquisitions
         SET status = 'completed', verify_status = 'passed', verified_path = ?,
             quarantined_path = NULL, last_error = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(verified_path)
    .bind(timeutil::now())
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Unrestricted status write for admin/reconciler flows that need to force a
/// transition no dedicated helper covers.
pub async fn set_status(pool: &SqlitePool, id: &str, status: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE acquisitions SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Lists the review queue (needs_review rows), oldest first.
pub async fn list_needing_review(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<Acquisition>, sqlx::Error> {
    sqlx::query_as::<_, Acquisition>(
        "SELECT * FROM acquisitions WHERE status = 'needs_review' ORDER BY updated_at ASC LIMIT ?",
    )
    .bind(clamp_limit(limit))
    .fetch_all(pool)
    .await
}

/// Reconciler sweep: nonterminal rows whose last observed activity predates
/// `updated_before` (falling back through progress_changed_at,
/// download_started_at, and stalled_at to created_at).
pub async fn list_stale_nonterminal(
    pool: &SqlitePool,
    updated_before: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<Acquisition>, sqlx::Error> {
    sqlx::query_as::<_, Acquisition>(
        "SELECT * FROM acquisitions
         WHERE status NOT IN ('failed','needs_review','removed','imported')
           AND COALESCE(progress_changed_at, download_started_at, stalled_at, created_at) < ?
         ORDER BY created_at ASC
         LIMIT ?",
    )
    .bind(fmt_time(updated_before))
    .bind(clamp_limit(limit))
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("a.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    /// Creates the wanted target + release candidate FK targets an acquisition needs.
    async fn seed_refs(p: &SqlitePool) -> (String, String) {
        let target = crate::storage::domain::create_wanted_target(p, "movie", "tmdb", "42", None)
            .await
            .unwrap();
        let run = crate::storage::domain::create_search_run(p, &target.id).await.unwrap();
        let cand = crate::storage::domain::insert_candidate(
            p,
            &run.id,
            "idx1",
            r#"{"title":"x"}"#,
            None,
        )
        .await
        .unwrap();
        (target.id, cand.id)
    }

    async fn make(p: &SqlitePool) -> Acquisition {
        let (t, cand) = seed_refs(p).await;
        create_acquisition(p, &cand, &t, "torrent", "qbittorrent", "main")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_validates_ids() {
        let (_d, p) = pool().await;
        let err = create_acquisition(&p, "", "t", "torrent", "q", "m").await;
        assert!(matches!(err, Err(AcquisitionError::MissingId)));
        let err = create_acquisition(&p, "c", "", "torrent", "q", "m").await;
        assert!(matches!(err, Err(AcquisitionError::MissingId)));
    }

    #[tokio::test]
    async fn create_get_roundtrip_and_defaults() {
        let (_d, p) = pool().await;
        let (target, cand) = seed_refs(&p).await;
        let a = create_acquisition(&p, &cand, &target, "usenet", "sabnzbd", "main")
            .await
            .unwrap();
        assert_eq!(a.status, "pending");
        assert_eq!(a.recovery_attempt, 0);

        let got = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(got.candidate_id, cand);
        assert_eq!(got.client_kind, "sabnzbd");
        assert!(got.external_id.is_none());
        assert!(got.progress.is_none());
        assert_eq!(got.recovery_attempt, 0, "NULL recovery_attempt decodes as 0");

        // Duplicate candidate violates ux_acquisitions_candidate_id and surfaces as Db error.
        let dup = create_acquisition(&p, &cand, &target, "usenet", "sabnzbd", "main").await;
        let err = dup.unwrap_err();
        assert!(
            matches!(&err, AcquisitionError::Db(e) if e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false)),
            "expected unique violation, got {err:?}"
        );

        assert!(get(&p, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_by_candidate_found_and_missing() {
        let (_d, p) = pool().await;
        let a = make(&p).await;
        assert!(get_by_candidate(&p, &a.candidate_id).await.unwrap().is_some());
        assert!(get_by_candidate(&p, "missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn lifecycle_enqueue_fail_complete_import() {
        let (_d, p) = pool().await;
        let a = make(&p).await;

        mark_enqueued(&p, &a.id, Some("ext-1"), Some("https://idx/dl")).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "enqueued");
        assert_eq!(g.external_id.as_deref(), Some("ext-1"));
        assert_eq!(g.source_url.as_deref(), Some("https://idx/dl"));
        assert!(g.last_error.is_none());

        mark_failed(&p, &a.id, "boom").await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "failed");
        assert_eq!(g.last_error.as_deref(), Some("boom"));

        // Recovery from failure clears the error when re-enqueued.
        mark_enqueued(&p, &a.id, Some("ext-2"), None).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert!(g.last_error.is_none());
        assert_eq!(g.source_url.as_deref(), Some("https://idx/dl"), "COALESCE keeps prior url");

        mark_completed(&p, &a.id, Some("/data/movie.mkv")).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "completed");
        assert!(g.completed_at.is_some());
        assert_eq!(g.content_path.as_deref(), Some("/data/movie.mkv"));

        mark_imported(&p, &a.id, "/library/movie.mkv").await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "imported");
        assert_eq!(g.imported_path.as_deref(), Some("/library/movie.mkv"));
        assert!(g.imported_at.is_some());
    }

    #[tokio::test]
    async fn update_progress_bumps_progress_changed() {
        let (_d, p) = pool().await;
        let a = make(&p).await;
        assert!(get(&p, &a.id).await.unwrap().unwrap().progress_changed_at.is_none());

        update_progress(&p, &a.id, "downloading", Some(0.5), Some("/dl/x.mkv")).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.progress, Some(0.5));
        assert_eq!(g.content_path.as_deref(), Some("/dl/x.mkv"));
        let first_bump = g.progress_changed_at.clone().unwrap();

        // Later poll with no new path must not clobber content_path but still bumps.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        update_progress(&p, &a.id, "downloading", Some(0.9), None).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.progress, Some(0.9));
        assert_eq!(g.content_path.as_deref(), Some("/dl/x.mkv"));
        assert!(g.progress_changed_at.unwrap() > first_bump);
    }

    #[tokio::test]
    async fn stall_clear_cycle() {
        let (_d, p) = pool().await;
        let a = make(&p).await;
        mark_stalled(&p, &a.id).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "stalled");
        assert!(g.stalled_at.is_some());

        clear_stalled(&p, &a.id).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert!(g.stalled_at.is_none());
        assert_eq!(g.status, "stalled", "clear_stalled only resets the timestamp");
    }

    #[tokio::test]
    async fn recovery_attempts_increment_atomically() {
        let (_d, p) = pool().await;
        let a = make(&p).await;
        assert_eq!(increment_recovery_attempt(&p, &a.id).await.unwrap(), 1);
        assert_eq!(increment_recovery_attempt(&p, &a.id).await.unwrap(), 2);
        assert_eq!(get(&p, &a.id).await.unwrap().unwrap().recovery_attempt, 2);
        assert!(increment_recovery_attempt(&p, "missing").await.is_err());
    }

    #[tokio::test]
    async fn verify_status_and_evidence_persist() {
        let (_d, p) = pool().await;
        let a = make(&p).await;

        set_verify_status(&p, &a.id, "running", None, None, None).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.verify_status.as_deref(), Some("running"));

        set_verify_status(&p, &a.id, "quarantined", None, Some("/q/x.mkv"),
            Some(r#"{"rule":"size"}"#)).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.quarantined_path.as_deref(), Some("/q/x.mkv"));
        assert!(g.verified_path.is_none());
        assert_eq!(g.verify_evidence_json.as_deref(), Some(r#"{"rule":"size"}"#));

        // Evidence without new payload keeps the stored JSON.
        set_verify_status(&p, &a.id, "passed", Some("/v/x.mkv"), None, None).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.verify_status.as_deref(), Some("passed"));
        assert_eq!(g.verified_path.as_deref(), Some("/v/x.mkv"));
        assert_eq!(g.verify_evidence_json.as_deref(), Some(r#"{"rule":"size"}"#));

        admin_accept_verified(&p, &a.id, "/v/final.mkv").await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "completed");
        assert_eq!(g.verify_status.as_deref(), Some("passed"));
        assert_eq!(g.verified_path.as_deref(), Some("/v/final.mkv"));
        assert!(g.quarantined_path.is_none());
        assert!(g.last_error.is_none());
    }

    #[tokio::test]
    async fn verify_lifecycle_helpers() {
        let (_d, p) = pool().await;
        let a = make(&p).await;

        mark_failed(&p, &a.id, "earlier").await.unwrap();
        mark_verifying(&p, &a.id).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "verifying");
        assert_eq!(g.verify_status.as_deref(), Some("running"));
        assert!(g.last_error.is_none(), "verifying clears prior error");

        set_verify_evidence(&p, &a.id, Some(r#"{"n":1}"#)).await.unwrap();
        mark_needs_review(&p, &a.id, "suspicious", Some("/q/a.mkv"), "rule hit").await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "needs_review");
        assert_eq!(g.verify_status.as_deref(), Some("suspicious"));
        assert_eq!(g.quarantined_path.as_deref(), Some("/q/a.mkv"));
        assert_eq!(g.last_error.as_deref(), Some("rule hit"));
        assert_eq!(g.verify_evidence_json.as_deref(), Some(r#"{"n":1}"#));
        assert_eq!(list_needing_review(&p, 10).await.unwrap()[0].id, a.id);

        mark_verified(&p, &a.id, "/v/a.mkv").await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.verify_status.as_deref(), Some("passed"));
        assert_eq!(g.verified_path.as_deref(), Some("/v/a.mkv"));
    }

    #[tokio::test]
    async fn generic_set_status_and_filters() {
        let (_d, p) = pool().await;
        let a1 = make(&p).await;
        let (t, c2) = seed_refs(&p).await;
        let a2 = create_acquisition(&p, &c2, &t, "torrent", "deluge", "alt")
            .await
            .unwrap();

        set_status(&p, &a1.id, "needs_review").await.unwrap();
        let review = list_needing_review(&p, 10).await.unwrap();
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].id, a1.id);

        // Default listing includes both, newest first (a2 created later).
        let all = list(&p, None, 0).await.unwrap(); // invalid limit clamps to 50
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, a2.id);
        assert_eq!(all[1].id, a1.id);

        // Status filter narrows; blank status behaves like no filter.
        let failed = list(&p, Some("needs_review"), 10).await.unwrap();
        assert_eq!(failed.len(), 1);
        let blank = list(&p, Some("  "), 10).await.unwrap();
        assert_eq!(blank.len(), 2);

        // Small limit respected.
        assert_eq!(list(&p, None, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_nonterminal_sweep() {
        let (_d, p) = pool().await;
        let fresh = make(&p).await;
        let (t, c2) = seed_refs(&p).await;
        let stale = create_acquisition(&p, &c2, &t, "torrent", "qbit", "m")
            .await
            .unwrap();

        // Push `stale` back in time by rewriting its timestamps directly.
        let old = fmt_time(Utc::now() - Duration::hours(6));
        sqlx::query("UPDATE acquisitions SET created_at = ?, updated_at = ? WHERE id = ?")
            .bind(&old)
            .bind(&old)
            .bind(&stale.id)
            .execute(&p)
            .await
            .unwrap();

        let cutoff = Utc::now() - Duration::hours(1);
        let found = list_stale_nonterminal(&p, cutoff, 50).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, stale.id);

        // Fresh row never appears; terminal statuses are excluded even when old.
        update_progress(&p, &stale.id, "downloading", Some(0.1), None).await.unwrap();
        assert!(list_stale_nonterminal(&p, cutoff, 50).await.unwrap().is_empty());
        mark_stalled(&p, &fresh.id).await.unwrap();
        let old2 = fmt_time(Utc::now() - Duration::hours(6));
        sqlx::query("UPDATE acquisitions SET stalled_at = ?, updated_at = ? WHERE id = ?")
            .bind(&old2)
            .bind(&old2)
            .bind(&fresh.id)
            .execute(&p)
            .await
            .unwrap();
        let found = list_stale_nonterminal(&p, cutoff, 50).await.unwrap();
        assert_eq!(found.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), vec![fresh.id.as_str()]);
    }

    #[tokio::test]
    async fn downloading_pins_start_timestamps_once() {
        let (_d, p) = pool().await;
        let a = make(&p).await;
        let t0 = Utc::now() - Duration::minutes(5);
        mark_downloading(&p, &a.id, t0, t0).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.status, "downloading");
        let pinned = g.download_started_at.clone().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        mark_downloading(&p, &a.id, Utc::now(), Utc::now()).await.unwrap();
        let g = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(g.download_started_at.unwrap(), pinned, "start timestamp pinned once");
    }
}
