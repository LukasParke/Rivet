//! SQLite storage: connection pool with *enforced* pragmas and a
//! transactional, filename-keyed migration runner.
//!
//! Fixes two defects of the original implementation:
//! 1. `PRAGMA foreign_keys = ON` appeared only inside migration scripts, so it
//!    died with the migration connection and runtime connections never
//!    enforced any REFERENCES constraint. Here the connect options force
//!    foreign_keys/WAL/busy_timeout regardless of the configured DSN.
//! 2. Migrations ran as bare multi-statement scripts with content-hash IDs:
//!    a crash mid-file left a half-applied schema, and editing an applied
//!    file re-ran it. Here every file runs inside one BEGIN IMMEDIATE
//!    transaction together with its bookkeeping row, keyed by filename, with
//!    loud drift detection when an already-applied file changes.

pub mod acquisitions;
pub mod audit;
pub mod blacklist;
pub mod calendar_queries;
pub mod domain;
pub mod jobs;
pub mod migrations;
pub mod movies;
pub mod notifications;
pub mod retention;
pub mod series;
pub mod sessions;
pub mod settings;
pub mod system_ops;
pub mod users;

use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database open failed: {0}")]
    Open(#[from] sqlx::Error),
}

/// Opens the SQLite pool. Foreign keys, WAL and busy_timeout are forced here
/// so they hold no matter what the DSN string contains.
pub async fn open(database_url: &str) -> Result<SqlitePool, StorageError> {
    // Accept both "file:/path/app.db?_pragma=..." and plain filesystem paths.
    let raw = database_url.strip_prefix("file://").unwrap_or(database_url);
    let raw = raw.strip_prefix("file:").unwrap_or(raw);
    let path = raw.split('?').next().unwrap_or(raw);
    if path.is_empty() {
        return Err(StorageError::Open(sqlx::Error::Configuration(
            "empty database path".into(),
        )));
    }
    let mut opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);

    // Durability decision: WAL journal + `synchronous = NORMAL`. Under WAL,
    // NORMAL fsyncs only at checkpoints, not per commit — queue/series write
    // throughput (see benches/storage_benches.rs) is dominated by per-commit
    // sync cost, and FULL would pay an fsync on every enqueue/claim for
    // protection only against OS crash or power loss (process crashes are
    // already safe: WAL frames are written, just not synced). OFF was
    // rejected: a checkpoint failure could corrupt the database. NORMAL
    // matches SQLite's recommended setting for WAL-mode applications.
    opts = opts
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await?;

    Ok(pool)
}

/// Verifies foreign-key enforcement is live (used by tests and health checks).
pub async fn foreign_keys_enabled(pool: &SqlitePool) -> bool {
    let v: Option<i64> = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(pool)
        .await
        .ok()
        .flatten();
    v == Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_enforces_foreign_keys_regardless_of_dsn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let dsn = format!("file://{}", path.display());
        let pool = open(&dsn).await.unwrap();
        assert!(foreign_keys_enabled(&pool).await);
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "wal"
        );
    }

    #[tokio::test]
    async fn foreign_keys_actually_reject_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("t.db").display());
        let pool = open(&dsn).await.unwrap();
        migrations::apply(&pool).await.unwrap();
        let res = sqlx::query(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) VALUES ('s','ghost','2026-01-01','2027-01-01')",
        )
        .execute(&pool)
        .await;
        assert!(res.is_err(), "FK enforcement must reject orphan session");
    }
}
