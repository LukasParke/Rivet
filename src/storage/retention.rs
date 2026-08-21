//! Retention janitor: prunes expired sessions, finished jobs, old audit
//! events, and completed search runs (with their candidates and decisions).
//!
//! Reuses [`crate::storage::sessions::delete_expired`] and
//! [`crate::storage::jobs::prune_finished`] so the cutoff semantics stay in
//! exactly one place per table.


use chrono::{Duration, Utc};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::{storage::jobs, storage::sessions, timeutil};

/// How long expired sessions linger before deletion.
pub const SESSION_GRACE: Duration = Duration::days(7);
/// How long completed/failed jobs are kept.
pub const JOB_RETENTION: Duration = Duration::days(30);
/// How long audit events are kept.
pub const AUDIT_RETENTION: Duration = Duration::days(90);
/// How long completed search runs (and their candidates/decisions) are kept.
pub const SEARCH_RUN_RETENTION: Duration = Duration::days(90);

#[derive(Debug, Clone, Serialize)]
pub struct RetentionCounts {
    pub sessions: u64,
    pub jobs: u64,
    pub audit_events: u64,
    pub release_candidates: u64,
}

impl RetentionCounts {
    pub fn total(&self) -> u64 {
        self.sessions + self.jobs + self.audit_events + self.release_candidates
    }
}

async fn prune_audit_events(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let cutoff = timeutil::fmt(Utc::now() - AUDIT_RETENTION);
    let res = sqlx::query("DELETE FROM audit_events WHERE created_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Deletes decisions and release candidates belonging to search runs that
/// completed more than `SEARCH_RUN_RETENTION` ago. Returns candidate rows
/// removed (decisions cascade via FK and are deleted explicitly first so the
/// count is exact even if cascades were disabled).
async fn prune_completed_runs(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let cutoff = timeutil::fmt(Utc::now() - SEARCH_RUN_RETENTION);
    sqlx::query(
        "DELETE FROM decisions WHERE candidate_id IN (
           SELECT c.id FROM release_candidates c
           JOIN search_runs r ON r.id = c.search_run_id
           WHERE r.status = 'completed' AND r.completed_at < ?
         )",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;

    let res = sqlx::query(
        "DELETE FROM release_candidates WHERE search_run_id IN (
           SELECT id FROM search_runs WHERE status = 'completed' AND completed_at < ?
         )",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Runs every retention sweep once. Each step is independent: a failure aborts
/// the remaining sweeps with the error rather than silently skipping work.
pub async fn run_retention(pool: &SqlitePool) -> Result<RetentionCounts, sqlx::Error> {
    let sessions_n = sessions::delete_expired(pool, SESSION_GRACE).await?;
    let jobs_n = jobs::prune_finished(pool, JOB_RETENTION).await?;
    let audit_n = prune_audit_events(pool).await?;
    let candidates_n = prune_completed_runs(pool).await?;
    Ok(RetentionCounts {
        sessions: sessions_n,
        jobs: jobs_n,
        audit_events: audit_n,
        release_candidates: candidates_n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("r.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    fn days_ago(days: i64) -> String {
        timeutil::fmt(Utc::now() - Duration::days(days))
    }

    async fn seed_session(p: &SqlitePool, user_id: &str, expires_days_ago: i64) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(days_ago(1))
        .bind(days_ago(expires_days_ago))
        .execute(p)
        .await
        .unwrap();
        id
    }

    async fn seed_job(p: &SqlitePool, state: &str, updated_days_ago: i64) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let old = days_ago(updated_days_ago);
        sqlx::query(
            "INSERT INTO jobs (id, kind, state, attempt, run_after, created_at, updated_at)
             VALUES (?, 'import', ?, 0, ?, ?, ?)",
        )
        .bind(&id)
        .bind(state)
        .bind(&old)
        .bind(&old)
        .bind(&old)
        .execute(p)
        .await
        .unwrap();
        id
    }

    async fn seed_audit(p: &SqlitePool, created_days_ago: i64) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO audit_events (id, action, created_at) VALUES (?, 'login', ?)")
            .bind(&id)
            .bind(days_ago(created_days_ago))
            .execute(p)
            .await
            .unwrap();
        id
    }

    /// Creates a completed run at `completed_days_ago` with one candidate+decision.
    async fn seed_run(
        p: &SqlitePool,
        status: &str,
        completed_days_ago: Option<i64>,
    ) -> (String, String, String) {
        let t = crate::storage::domain::create_wanted_target(p, "movie", "tmdb", "1", None)
            .await
            .unwrap();
        let run = crate::storage::domain::create_search_run(p, &t.id).await.unwrap();
        let completed_at = completed_days_ago.map(|d| days_ago(d));
        if let Some(ca) = &completed_at {
            sqlx::query("UPDATE search_runs SET status = ?, completed_at = ? WHERE id = ?")
                .bind(status)
                .bind(ca)
                .bind(&run.id)
                .execute(p)
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE search_runs SET status = ? WHERE id = ?")
                .bind(status)
                .bind(&run.id)
                .execute(p)
                .await
                .unwrap();
        }
        let cand =
            crate::storage::domain::insert_candidate(p, &run.id, "idx", r#"{}"#, None)
                .await
                .unwrap();
        crate::storage::domain::insert_decision(p, &cand.id, true, 10, &["ok".into()])
            .await
            .unwrap();
        (t.id, run.id, cand.id)
    }

    #[tokio::test]
    async fn empty_database_is_a_noop() {
        let (_d, p) = pool().await;
        let counts = run_retention(&p).await.unwrap();
        assert_eq!(
            (counts.sessions, counts.jobs, counts.audit_events, counts.release_candidates),
            (0, 0, 0, 0)
        );
    }

    #[tokio::test]
    async fn prunes_only_rows_past_each_cutoff() {
        let (_d, p) = pool().await;
        let u = crate::storage::users::create(&p, "a@b.c", "argon2id$hash", "admin")
            .await
            .unwrap();

        // Sessions: expired >7d goes; recently-expired and active stay.
        let old_expired = seed_session(&p, &u.id, 8).await;
        let fresh_expired = seed_session(&p, &u.id, 3).await;
        let active = {
            let s = sessions::create(
                &p,
                &u.id,
                Duration::hours(1),
                None,
                None,
            )
            .await
            .unwrap();
            s.id
        };

        // Jobs: finished >30d goes; finished-fresh, queued-old stay.
        let dead_job = seed_job(&p, "completed", 31).await;
        let fresh_done = seed_job(&p, "failed", 5).await;
        let queued_old = seed_job(&p, "queued", 40).await;

        // Audit: >90d goes; recent stays.
        let old_event = seed_audit(&p, 91).await;
        seed_audit(&p, 30).await;

        let counts = run_retention(&p).await.unwrap();
        assert_eq!(counts.sessions, 1);
        assert_eq!(counts.jobs, 1);
        assert_eq!(counts.audit_events, 1);

        assert!(sessions::get(&p, &old_expired).await.unwrap().is_none());
        assert!(sessions::get(&p, &fresh_expired).await.unwrap().is_some());
        assert!(sessions::get(&p, &active).await.unwrap().is_some());

        assert!(jobs::get(&p, &dead_job).await.unwrap().is_none());
        assert!(jobs::get(&p, &fresh_done).await.unwrap().is_some());
        assert!(jobs::get(&p, &queued_old).await.unwrap().is_some(), "queued jobs never pruned");
    }

    #[tokio::test]
    async fn prunes_candidates_and_decisions_of_old_completed_runs() {
        let (_d, p) = pool().await;
        let (_, old_run, old_cand) = seed_run(&p, "completed", Some(91)).await;
        let (_, fresh_run, fresh_cand) = seed_run(&p, "completed", Some(10)).await;
        let (_, failed_run, failed_cand) = seed_run(&p, "failed", Some(200)).await;
        let (_, running_run, running_cand) = seed_run(&p, "running", None).await;

        let counts = run_retention(&p).await.unwrap();
        assert_eq!(counts.release_candidates, 1, "only the >90d completed run is swept");

        for (run_id, cand_id) in [
            (&old_run, &old_cand),
            (&fresh_run, &fresh_cand),
            (&failed_run, &failed_cand),
            (&running_run, &running_cand),
        ] {
            let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM release_candidates WHERE id = ?")
                .bind(cand_id)
                .fetch_one(&p)
                .await
                .unwrap();
            let d: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM decisions WHERE candidate_id IN
                   (SELECT id FROM release_candidates WHERE search_run_id = ?)",
            )
            .bind(run_id)
            .fetch_one(&p)
            .await
            .unwrap();
            let expected = if *run_id == old_run { 0 } else { 1 };
            assert_eq!(n.0, expected, "candidates for run {run_id}");
            assert_eq!(d.0, expected, "decisions for run {run_id}");
        }

        // The run rows themselves are retained (history), only their results go.
        let kept: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM search_runs")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(kept.0, 4);
    }

    /// Guards the timestamp format assumption retention depends on:
    /// lexicographic comparison == chronological comparison.
    #[test]
    fn cutoffs_compare_lexicographically() {
        let now = Utc::now();
        let older = now - Duration::days(91);
        assert!(timeutil::fmt(older) < timeutil::fmt(now));
        let parsed: DateTime<Utc> = DateTime::parse_from_rfc3339(&timeutil::fmt(now))
            .unwrap()
            .into();
        assert_eq!(parsed, now);
    }
}
