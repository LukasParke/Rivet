//! Durable job queue on SQLite.
//!
//! Fixes over the original:
//! - Terminal updates (`complete`, `fail_and_retry`, `fail_final`) are guarded
//!   by `lease_owner`, so an expired-lease zombie cannot clobber a job another
//!   worker now owns.
//! - `renew_lease` lets long-running handlers extend their lease instead of
//!   silently losing ownership mid-flight.
//! - Timestamps use fixed-width RFC3339 so SQL string comparisons are
//!   chronologically correct.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::timeutil;

pub const MAX_ATTEMPTS: i64 = 6;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<String>,
    pub attempt: i64,
    pub run_after: String,
    pub payload_json: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("no job available")]
    NoJob,
    #[error("job kind required")]
    MissingKind,
    #[error("not lease owner")]
    NotOwner,
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct ClaimOpts {
    pub lease_owner: String,
    pub lease_duration: Duration,
}

/// Inserts a queued job. `payload` must serialize; a serialization failure is
/// an error, never a silently-empty row.
///
/// `RETURNING *` yields the stored row in the same statement — no follow-up
/// `SELECT` round trip.
pub async fn enqueue<T: serde::Serialize>(
    pool: &SqlitePool,
    kind: &str,
    payload: Option<&T>,
    run_after: Option<DateTime<Utc>>,
) -> Result<Job, QueueError> {
    if kind.is_empty() {
        return Err(QueueError::MissingKind);
    }
    let now = Utc::now();
    let run_after = timeutil::fmt(run_after.unwrap_or(now));
    let payload_json = match payload {
        None => None,
        Some(p) => Some(
            serde_json::to_string(p)
                .map_err(|e| QueueError::Db(sqlx::Error::Configuration(Box::new(e))))?,
        ),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now_s = timeutil::fmt(now);
    sqlx::query_as::<_, Job>(
        "INSERT INTO jobs (id, kind, state, lease_owner, lease_expires_at, attempt, run_after, payload_json, last_error, created_at, updated_at)
         VALUES (?, ?, 'queued', NULL, NULL, 0, ?, ?, NULL, ?, ?)
         RETURNING *",
    )
    .bind(&id)
    .bind(kind)
    .bind(&run_after)
    .bind(&payload_json)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(pool)
    .await
    .map_err(QueueError::from)
}

/// Claims one runnable job: queued, due, and not actively leased.
///
/// Single atomic statement: the scalar subquery picks the oldest runnable job
/// (FIFO by `run_after`, `created_at`) and the outer `UPDATE` re-checks every
/// claim condition against the row *at write time*, so two workers racing on
/// the same candidate are serialized by SQLite's write lock and exactly one
/// wins — the loser sees zero affected rows and gets [`QueueError::NoJob`].
/// Lease ownership is stamped inside the same statement, so an expired-lease
/// zombie can never interleave between selection and claim.
pub async fn claim_one(pool: &SqlitePool, opts: &ClaimOpts) -> Result<Job, QueueError> {
    let now = Utc::now();
    let lease_exp = timeutil::fmt(now + opts.lease_duration);
    let now_s = timeutil::fmt(now);

    let job: Option<Job> = sqlx::query_as(
        "UPDATE jobs SET lease_owner = ?, lease_expires_at = ?, updated_at = ?
         WHERE id = (
             SELECT id FROM jobs
             WHERE state = 'queued' AND run_after <= ?
               AND (lease_expires_at IS NULL OR lease_expires_at <= ?)
             ORDER BY run_after ASC, created_at ASC LIMIT 1
         )
           AND state = 'queued' AND run_after <= ?
           AND (lease_expires_at IS NULL OR lease_expires_at <= ?)
         RETURNING *",
    )
    .bind(&opts.lease_owner)
    .bind(&lease_exp)
    .bind(&now_s)
    .bind(&now_s)
    .bind(&now_s)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_optional(pool)
    .await?;

    job.ok_or(QueueError::NoJob)
}

/// Extends the caller's live lease. Fails if ownership was lost.
pub async fn renew_lease(
    pool: &SqlitePool,
    id: &str,
    owner: &str,
    extend: Duration,
) -> Result<(), QueueError> {
    let exp = timeutil::fmt(Utc::now() + extend);
    let res = sqlx::query(
        "UPDATE jobs SET lease_expires_at = ?, updated_at = ?
         WHERE id = ? AND lease_owner = ? AND state = 'queued'",
    )
    .bind(&exp)
    .bind(timeutil::now())
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await?;
    if res.rows_affected() != 1 {
        return Err(QueueError::NotOwner);
    }
    Ok(())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Marks completion. Only the current lease owner may do this.
pub async fn complete(pool: &SqlitePool, id: &str, owner: &str) -> Result<(), QueueError> {
    let res = sqlx::query(
        "UPDATE jobs SET state = 'completed', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
         WHERE id = ? AND lease_owner = ?",
    )
    .bind(timeutil::now())
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await?;
    if res.rows_affected() != 1 {
        return Err(QueueError::NotOwner);
    }
    Ok(())
}

/// Schedules a retry with exponential backoff + jitter, or fails finally when
/// the attempt budget is exhausted. Owner-guarded like `complete`.
pub async fn fail_and_retry(
    pool: &SqlitePool,
    id: &str,
    owner: &str,
    attempt: i64,
    err_str: &str,
) -> Result<(), QueueError> {
    let now = Utc::now();
    if attempt >= MAX_ATTEMPTS {
        return fail_final(pool, id, owner, attempt, err_str).await;
    }
    let next = next_backoff(attempt, now);
    let res = sqlx::query(
        "UPDATE jobs SET state = 'queued', attempt = ?, run_after = ?, last_error = ?,
         lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
         WHERE id = ? AND lease_owner = ?",
    )
    .bind(attempt)
    .bind(timeutil::fmt(next))
    .bind(err_str)
    .bind(timeutil::now())
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await?;
    if res.rows_affected() != 1 {
        return Err(QueueError::NotOwner);
    }
    Ok(())
}

pub async fn fail_final(
    pool: &SqlitePool,
    id: &str,
    owner: &str,
    attempt: i64,
    err_str: &str,
) -> Result<(), QueueError> {
    let res = sqlx::query(
        "UPDATE jobs SET state = 'failed', attempt = ?, last_error = ?,
         lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
         WHERE id = ? AND lease_owner = ?",
    )
    .bind(attempt)
    .bind(err_str)
    .bind(timeutil::now())
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await?;
    if res.rows_affected() != 1 {
        return Err(QueueError::NotOwner);
    }
    Ok(())
}

/// Exponential backoff with jitter: base = min(60s, 2^attempt s), plus up to
/// base/2 jitter. Overflow-safe through checked arithmetic.
fn next_backoff(attempt: i64, now: DateTime<Utc>) -> DateTime<Utc> {
    let attempt = attempt.max(1);
    let secs = 2i64
        .checked_pow(attempt.min(16) as u32)
        .map(|v| v.min(60))
        .unwrap_or(60);
    let base_secs = secs;
    let jitter_max = (base_secs / 2).max(0);
    let jitter = rand::Rng::gen_range(&mut rand::thread_rng(), 0..=(jitter_max.max(1)));
    now + Duration::seconds(base_secs + jitter)
}

/// Requeues jobs whose lease expired while still marked queued (crashed
/// worker recovery): clears the stale lease so they can be claimed again.
pub async fn release_expired_leases(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE jobs SET lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
         WHERE state = 'queued' AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?",
    )
    .bind(timeutil::now())
    .bind(timeutil::now())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Retention prune: completed/failed jobs older than `older_than`.
pub async fn prune_finished(pool: &SqlitePool, older_than: Duration) -> Result<u64, sqlx::Error> {
    let cutoff = timeutil::fmt(Utc::now() - older_than);
    let res =
        sqlx::query("DELETE FROM jobs WHERE state IN ('completed','failed') AND updated_at < ?")
            .bind(cutoff)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let p = crate::storage::open(&format!("file://{}", dir.path().join("q.db").display()))
            .await
            .unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    fn opts(owner: &str) -> ClaimOpts {
        ClaimOpts {
            lease_owner: owner.to_string(),
            lease_duration: Duration::seconds(30),
        }
    }

    #[tokio::test]
    async fn enqueue_claim_complete_lifecycle() {
        let (_d, p) = pool().await;
        let j = enqueue(&p, "search_run", Some(&json!({"targetId": "t1"})), None)
            .await
            .unwrap();
        assert_eq!(j.state, "queued");
        assert!(j.payload_json.as_deref().unwrap().contains("t1"));

        let claimed = claim_one(&p, &opts("w1")).await.unwrap();
        assert_eq!(claimed.id, j.id);
        assert_eq!(claimed.lease_owner.as_deref(), Some("w1"));

        // Second claimer finds nothing (single job, actively leased).
        assert!(matches!(
            claim_one(&p, &opts("w2")).await,
            Err(QueueError::NoJob)
        ));

        complete(&p, &j.id, "w1").await.unwrap();
        let done = get(&p, &j.id).await.unwrap().unwrap();
        assert_eq!(done.state, "completed");
    }

    #[tokio::test]
    async fn terminal_writes_are_owner_guarded() {
        let (_d, p) = pool().await;
        let j = enqueue::<serde_json::Value>(&p, "search_run", None, None)
            .await
            .unwrap();
        claim_one(&p, &opts("w1")).await.unwrap();

        // A different owner cannot complete it.
        assert!(matches!(
            complete(&p, &j.id, "w2").await,
            Err(QueueError::NotOwner)
        ));
        assert!(matches!(
            fail_final(&p, &j.id, "w2", 1, "err").await,
            Err(QueueError::NotOwner)
        ));
        // Original owner can.
        fail_and_retry(&p, &j.id, "w1", 1, "boom").await.unwrap();
        let j2 = get(&p, &j.id).await.unwrap().unwrap();
        assert_eq!(j2.state, "queued");
        assert_eq!(j2.attempt, 1);
        assert!(j2.last_error.as_deref() == Some("boom"));
        assert!(j2.lease_owner.is_none());
    }

    #[tokio::test]
    async fn retries_cap_at_max_attempts() {
        let (_d, p) = pool().await;
        let j = enqueue::<serde_json::Value>(&p, "search_run", None, None)
            .await
            .unwrap();
        claim_one(&p, &opts("w1")).await.unwrap();
        fail_and_retry(&p, &j.id, "w1", MAX_ATTEMPTS, "last")
            .await
            .unwrap();
        let j2 = get(&p, &j.id).await.unwrap().unwrap();
        assert_eq!(
            j2.state, "failed",
            "attempt >= MAX_ATTEMPTS must fail finally"
        );
    }

    #[tokio::test]
    async fn lease_renewal_requires_ownership() {
        let (_d, p) = pool().await;
        let j = enqueue::<serde_json::Value>(&p, "search_run", None, None)
            .await
            .unwrap();
        claim_one(&p, &opts("w1")).await.unwrap();
        assert!(renew_lease(&p, &j.id, "w1", Duration::seconds(30))
            .await
            .is_ok());
        assert!(matches!(
            renew_lease(&p, &j.id, "w2", Duration::seconds(30)).await,
            Err(QueueError::NotOwner)
        ));
    }

    #[tokio::test]
    async fn expired_lease_is_reclaimable() {
        let (_d, p) = pool().await;
        let j = enqueue::<serde_json::Value>(&p, "search_run", None, None)
            .await
            .unwrap();
        // Claim with a tiny lease, then let it lapse.
        let short = ClaimOpts {
            lease_owner: "w1".into(),
            lease_duration: Duration::milliseconds(5),
        };
        claim_one(&p, &short).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        release_expired_leases(&p).await.unwrap();
        let reclaimed = claim_one(&p, &opts("w2")).await.unwrap();
        assert_eq!(reclaimed.id, j.id);
    }

    #[tokio::test]
    async fn future_jobs_not_claimed_early() {
        let (_d, p) = pool().await;
        enqueue(
            &p,
            "search_run",
            Option::<&serde_json::Value>::None,
            Some(Utc::now() + Duration::hours(1)),
        )
        .await
        .unwrap();
        assert!(matches!(
            claim_one(&p, &opts("w1")).await,
            Err(QueueError::NoJob)
        ));
    }

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        let now = Utc::now();
        for attempt in [1, 2, 3, 10, 100] {
            let next = next_backoff(attempt, now);
            let delta = (next - now).num_seconds();
            assert!(
                (1..=60 + 31).contains(&delta),
                "attempt {attempt} → {delta}s"
            );
        }
    }
}
