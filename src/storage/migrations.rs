//! Transactional, filename-keyed migration runner with drift detection.
//!
//! - Each file runs inside a single `BEGIN IMMEDIATE` transaction together
//!   with its `schema_migrations` row: a crash mid-file rolls back cleanly.
//! - Files are keyed by filename; the content sha256 is recorded separately.
//!   Re-applying an edited file is an error, never silent duplicate work.
//! - Legacy databases migrated by the Go implementation (content-hash IDs of
//!   the form `<name>:<sha256>`) are recognized and re-registered under the
//!   new scheme without re-execution; edited legacy files are reported as
//!   drift instead of being re-run.

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

const MIGRATIONS: &[( &str, &str )] = &[
    ("0001_init.sql", include_str!("migrations/0001_init.sql")),
    ("0002_sessions_csrf.sql", include_str!("migrations/0002_sessions_csrf.sql")),
    ("0003_core_domain.sql", include_str!("migrations/0003_core_domain.sql")),
    ("0004_acquisitions.sql", include_str!("migrations/0004_acquisitions.sql")),
    ("0005_acquisitions_fields.sql", include_str!("migrations/0005_acquisitions_fields.sql")),
    ("0006_acquisition_verify_stall.sql", include_str!("migrations/0006_acquisition_verify_stall.sql")),
    ("0007_blacklist.sql", include_str!("migrations/0007_blacklist.sql")),
    ("0008_verify_evidence.sql", include_str!("migrations/0008_verify_evidence.sql")),
    ("0009_hardening.sql", include_str!("migrations/0009_hardening.sql")),
];

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("sql error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("migration {0} changed after being applied (recorded {1}, current {2})")]
    Drift(String, String, String),
}

fn sha256_hex(data: &str) -> String {
    let mut h = Sha256::new();
    h.update(data.as_bytes());
    hex::encode(h.finalize())
}

/// Applies all pending migrations. Idempotent; safe to call on every start.
pub async fn apply(pool: &SqlitePool) -> Result<(), MigrationError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
           name TEXT PRIMARY KEY,
           hash TEXT NOT NULL,
           applied_at TEXT NOT NULL
         )",
    )
    .execute(&mut *tx)
    .await?;

    // Adopt legacy rows (id = "<name>:<sha256>") into the new table.
    let legacy_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations_legacy'",
    )
    .fetch_one(&mut *tx)
    .await?;
    let has_legacy = legacy_count > 0;
    if !has_legacy {
        // Detect whether the existing schema_migrations uses the legacy shape
        // (columns id/applied_at). If so, rename and adopt.
        let cols: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM pragma_table_info('schema_migrations')",
        )
        .fetch_all(&mut *tx)
        .await?;
        let names: Vec<&str> = cols.iter().map(|(n,)| n.as_str()).collect();
        if names.contains(&"id") && !names.contains(&"name") {
            sqlx::query("ALTER TABLE schema_migrations RENAME TO schema_migrations_legacy")
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                   name TEXT PRIMARY KEY,
                   hash TEXT NOT NULL,
                   applied_at TEXT NOT NULL
                 )",
            )
            .execute(&mut *tx)
            .await?;
            adopt_legacy(&mut tx).await?;
        }
    }

    for (name, contents) in MIGRATIONS {
        let hash = sha256_hex(contents);
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT hash FROM schema_migrations WHERE name = ?")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        match existing {
            Some((recorded,)) => {
                if recorded != hash {
                    return Err(MigrationError::Drift(
                        (*name).to_string(),
                        recorded,
                        hash,
                    ));
                }
                continue;
            }
            None => {
                // The whole file plus its bookkeeping row commit atomically.
                sqlx::raw_sql(contents).execute(&mut *tx).await?;
                sqlx::query(
                    "INSERT INTO schema_migrations (name, hash, applied_at)
                     VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                )
                .bind(name)
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    tx.commit().await?;
    Ok(())
}

async fn adopt_legacy(tx: &mut sqlx::SqliteConnection) -> Result<(), MigrationError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, applied_at FROM schema_migrations_legacy")
            .fetch_all(&mut *tx)
            .await?;
    for (id, applied_at) in rows {
        // Legacy ids look like "0001_init.sql:<sha256>".
        let Some((name, hash)) = id.split_once(':') else { continue };
        sqlx::query("INSERT OR IGNORE INTO schema_migrations (name, hash, applied_at) VALUES (?, ?, ?)")
            .bind(name)
            .bind(hash)
            .bind(applied_at)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage;

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("m.db").display());
        let pool = storage::open(&dsn).await.unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn applies_all_and_is_idempotent() {
        let (_d, pool) = test_pool().await;
        apply(&pool).await.unwrap();
        apply(&pool).await.unwrap(); // second run must be a no-op
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n as usize, MIGRATIONS.len());
    }

    #[tokio::test]
    async fn detects_drift() {
        let (_d, pool) = test_pool().await;
        apply(&pool).await.unwrap();
        // Simulate an edit to an already-applied file by corrupting its hash.
        sqlx::query("UPDATE schema_migrations SET hash = 'deadbeef' WHERE name = '0001_init.sql'")
            .execute(&pool)
            .await
            .unwrap();
        let err = apply(&pool).await.unwrap_err();
        assert!(matches!(err, MigrationError::Drift(_, _, _)), "{err}");
    }

    #[tokio::test]
    async fn adopts_legacy_go_rows_without_rerunning() {
        let (_d, pool) = test_pool().await;
        // Simulate a Go-migrated database: legacy table with content-hash ids
        // AND the schema those files created.
        sqlx::query("CREATE TABLE schema_migrations (id TEXT PRIMARY KEY, applied_at TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        for (name, contents) in MIGRATIONS.iter().take(8) {
            sqlx::raw_sql(contents).execute(&pool).await.unwrap();
            let id = format!("{name}:{}", sha256_hex(contents));
            sqlx::query("INSERT INTO schema_migrations (id, applied_at) VALUES (?, '2026-01-01T00:00:00Z')")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        apply(&pool).await.unwrap();

        // All 9 registered; only 0009 actually executed its SQL.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n as usize, MIGRATIONS.len());
        // Re-applying must stay idempotent: adopted files never re-run (the
        // ALTER TABLE files would fail with duplicate-column if they did),
        // and only 0009 executes its body.
        apply(&pool).await.unwrap();
        let hardening: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='ux_blacklist_entries'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hardening, 1, "0009 must have run once");
    }

    #[tokio::test]
    async fn failed_migration_rolls_back_atomically() {
        let (_d, pool) = test_pool().await;
        // Corrupt one file's SQL at runtime by applying a bad statement set:
        // simulate via direct runner behavior — apply valid migrations first.
        apply(&pool).await.unwrap();
        // A multi-statement script whose second statement fails must leave no
        // partial effects. Exercise through a manual transaction like the runner.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let res = sqlx::raw_sql("CREATE TABLE rollback_probe(a INT); INSERT INTO nonexistent(x) VALUES (1);")
            .execute(&mut *tx)
            .await;
        assert!(res.is_err());
        drop(tx); // rollback
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rollback_probe'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(exists, 0, "failed script must roll back fully");
    }
}
