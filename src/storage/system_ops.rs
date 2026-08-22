//! Operational parity helpers (`*arr` "System" area):
//!
//! - Database backups via SQLite `VACUUM INTO` into a timestamped file.
//! - Backup listing for a backup directory.
//! - Data-directory usage accounting.
//! - Remote path mappings CRUD and longest-prefix path translation.
//! - Health-check aggregation ([`health_issues`]).
//!
//! # Disk-space accounting
//!
//! [`disk_free`] reports true free space via `libc::statvfs`
//! (f_bavail × f_frsize). This is unix-only (`cfg(unix)`): there is no
//! portable `std` equivalent and shelling out to external binaries is banned
//! in this codebase; non-unix targets return `None` and the low-disk health
//! rule skips instead of guessing. [`dir_usage`] complements it with the
//! recursive byte size of a directory tree (symlinks skipped, depth-capped).

use crate::storage::{acquisitions, settings};
use chrono::Utc;
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::timeutil;

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

/// Filename prefix shared by every backup this module creates; used by
/// [`list_backups`] to distinguish our files from stray content.
pub const BACKUP_PREFIX: &str = "rivet-backup-";

/// Creates a consistent snapshot of the database with `VACUUM INTO` and
/// returns its full path. The destination directory is created on demand.
///
/// `VACUUM` cannot run inside an explicit transaction, so the statement runs
/// on a pooled connection directly — SQLite guarantees the written copy is
/// transactionally consistent regardless.
pub async fn backup_database(pool: &SqlitePool, dest_dir: &Path) -> Result<PathBuf, AppError> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| AppError::Internal(format!("backup dir create failed: {e}")))?;
    let name = format!(
        "{}{}.db",
        BACKUP_PREFIX,
        Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let dest = dest_dir.join(name);
    sqlx::query("VACUUM INTO ?")
        .bind(dest.to_string_lossy().as_ref())
        .execute(pool)
        .await
        .map_err(AppError::Db)?;
    Ok(dest)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub name: String,
    pub size_bytes: u64,
}

/// Lists backups previously created by [`backup_database`] in `dir`,
/// newest filename first. A missing or unreadable directory yields an empty
/// list (nothing to show yet), never an error.
pub fn list_backups(dir: &Path) -> Vec<BackupFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(BACKUP_PREFIX) && name.ends_with(".db")) {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        out.push(BackupFile {
            name,
            size_bytes: md.len(),
        });
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

// ---------------------------------------------------------------------------
// Directory usage accounting (see module docs for the statvfs limitation)
// ---------------------------------------------------------------------------

/// Maximum recursion depth of [`dir_usage`]; deeper levels are not descended.
pub const MAX_WALK_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirUsage {
    /// Recursive byte size of the tree (regular files only).
    pub bytes_used: u64,
    /// True when the walk hit [`MAX_WALK_DEPTH`] and under-counted.
    pub truncated: bool,
}

/// Recursively sums file sizes under `path`, skipping symlinks and capping
/// recursion at [`MAX_WALK_DEPTH`]. Unreadable entries are skipped rather
/// than failing the whole walk.
pub fn dir_usage(path: &Path) -> DirUsage {
    let mut truncated = false;
    let bytes_used = walk(path, 0, &mut truncated);
    DirUsage {
        bytes_used,
        truncated,
    }
}

fn walk(path: &Path, depth: usize, truncated: &mut bool) -> u64 {
    // symlink_metadata never follows the final link, so loops through
    // symlinks are impossible by construction.
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if md.is_file() {
        return md.len();
    }
    if !md.is_dir() {
        return 0;
    }
    if depth >= MAX_WALK_DEPTH {
        *truncated = true;
        return 0;
    }
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total += walk(&entry.path(), depth + 1, truncated);
        }
    }
    total
}

/// One gibibyte; the low-disk-space health threshold.
pub const LOW_DISK_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;

/// Bytes available to unprivileged users on the filesystem containing
/// `path` (`statvfs` f_bavail × f_frsize), or `None` when the call fails.
///
/// # Platform scope (`cfg(unix)`)
///
/// Unix only: `libc::statvfs` has no portable `std` equivalent and external
/// binaries are banned in this codebase. On non-unix targets this always
/// returns `None`, so callers (e.g. [`health_issues`]) skip free-space rules.
#[cfg(unix)]
pub fn disk_free(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Non-unix fallback: free space cannot be measured without platform APIs.
#[cfg(not(unix))]
pub fn disk_free(_path: &Path) -> Option<u64> {
    None
}

/// Resolves the main database file path from a live pool via SQLite's
/// `pragma_database_list`; `None` for in-memory databases.
async fn database_file(pool: &SqlitePool) -> Option<PathBuf> {
    let file: String =
        sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
            .fetch_one(pool)
            .await
            .ok()?;
    if file.is_empty() {
        None
    } else {
        Some(PathBuf::from(file))
    }
}

// ---------------------------------------------------------------------------
// Remote path mappings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RemotePathMapping {
    pub id: String,
    pub client_path: String,
    pub local_path: String,
}

pub async fn list_mappings(pool: &SqlitePool) -> Result<Vec<RemotePathMapping>, sqlx::Error> {
    sqlx::query_as::<_, RemotePathMapping>(
        "SELECT id, client_path, local_path FROM remote_path_mappings ORDER BY client_path",
    )
    .fetch_all(pool)
    .await
}

/// Creates or updates (by unique `client_path`) a mapping and returns the
/// stored row.
pub async fn upsert_mapping(
    pool: &SqlitePool,
    client_path: &str,
    local_path: &str,
) -> Result<RemotePathMapping, AppError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query_as::<_, RemotePathMapping>(
        "INSERT INTO remote_path_mappings (id, client_path, local_path) VALUES (?, ?, ?)
         ON CONFLICT(client_path) DO UPDATE SET local_path = excluded.local_path
         RETURNING id, client_path, local_path",
    )
    .bind(id)
    .bind(client_path)
    .bind(local_path)
    .fetch_one(pool)
    .await
    .map_err(AppError::Db)
}

/// Deletes by id; returns false when no row matched.
pub async fn delete_mapping(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM remote_path_mappings WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Translates a download-client-reported path into its local equivalent using
/// the longest matching `client_path` prefix. Returns `None` when no mapping
/// applies. The remainder after the prefix is appended verbatim, so
/// `/mnt/remote/data/movies/x` with mapping `/mnt/remote/data → /data`
/// yields `/data/movies/x`.
pub fn translate(mappings: &[RemotePathMapping], client_path: &str) -> Option<String> {
    mappings
        .iter()
        .filter(|m| !m.client_path.is_empty() && client_path.starts_with(&m.client_path))
        .max_by_key(|m| m.client_path.len())
        .map(|m| format!("{}{}", m.local_path, &client_path[m.client_path.len()..]))
}

// ---------------------------------------------------------------------------
// Health checks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    /// "error" or "warning".
    pub severity: String,
    /// Stable machine-readable source id (e.g. `failed_jobs`).
    pub source: String,
    pub message: String,
}

fn issue(severity: &str, source: &str, message: impl Into<String>) -> Issue {
    Issue {
        severity: severity.to_string(),
        source: source.to_string(),
        message: message.into(),
    }
}
/// Aggregates operational health issues:
///
/// - more than 3 failed jobs in the last hour → error;
/// - no indexers configured → warning;
/// - migration drift (an applied migration file changed on disk) → error;
/// - acquisitions stuck in a non-terminal state for over 24h → warning;
/// - a staged database restore (`<db>.restore.pending`) awaiting the manual
///   swap → warning;
/// - less than [`LOW_DISK_THRESHOLD_BYTES`] free on the database volume
///   (unix only) → warning.
pub async fn health_issues(pool: &SqlitePool) -> Vec<Issue> {
    let mut issues = Vec::new();

    // Migration drift. `migrations::apply` is deliberately NOT called here:
    // its future is not `Send` (sqlx sqlite higher-ranked `Executor` lifetime
    // quirk), which would poison every axum handler awaiting it. Drift is
    // instead detected read-only by re-hashing the embedded migration files
    // and comparing against the recorded `schema_migrations` hashes.
    if let Some((name, recorded, current)) = migration_drift(pool).await {
        issues.push(issue(
            "error",
            "migrations",
            format!("migration {name} changed after being applied (recorded sha256 {recorded}, current {current})"),
        ));
    }

    // Failed jobs in the last hour.
    let cutoff = timeutil::fmt(Utc::now() - chrono::Duration::hours(1));
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(1) FROM jobs WHERE state = 'failed' AND updated_at >= ?")
            .bind(cutoff)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    if failed > 3 {
        issues.push(issue(
            "error",
            "failed_jobs",
            format!("{failed} jobs failed in the last hour"),
        ));
    }

    // Indexer configuration.
    match settings::get(pool, "setup.indexers").await {
        Ok(Some(raw)) => {
            if count_indexers(&raw) == 0 {
                issues.push(issue("warning", "indexers", "no indexers are configured"));
            }
        }
        Ok(None) => issues.push(issue("warning", "indexers", "no indexers are configured")),
        Err(e) => issues.push(issue(
            "warning",
            "indexers",
            format!("indexer settings unreadable: {e}"),
        )),
    }

    // Stale non-terminal acquisitions (>24h without observed activity).
    let stale_cutoff = Utc::now() - chrono::Duration::hours(24);
    match acquisitions::list_stale_nonterminal(pool, stale_cutoff, 100).await {
        Ok(stale) if !stale.is_empty() => issues.push(issue(
            "warning",
            "stale_acquisitions",
            format!(
                "{} acquisition(s) stuck non-terminal for over 24h",
                stale.len()
            ),
        )),
        Ok(_) => {}
        Err(e) => issues.push(issue(
            "warning",
            "stale_acquisitions",
            format!("stale-acquisition sweep failed: {e}"),
        )),
    }

    // Database-volume rules need the database file location; skipped for
    // in-memory databases.
    if let Some(db_file) = database_file(pool).await {
        // Staged restore from POST /system/backups/{name}/restore: the copy
        // sits next to the live db until the operator swaps it manually.
        let pending = PathBuf::from(format!("{}.restore.pending", db_file.display()));
        if pending.exists() {
            issues.push(issue(
                "warning",
                "pending_restore",
                format!(
                    "a staged database restore ({}) is awaiting the manual swap; \
                     stop the service, move it over {}, then restart",
                    pending.display(),
                    db_file.display()
                ),
            ));
        }

        // Low free space on the volume holding the database (unix only;
        // non-unix disk_free is None and the rule is skipped).
        match disk_free(&db_file) {
            Some(free) if free < LOW_DISK_THRESHOLD_BYTES => issues.push(issue(
                "warning",
                "disk_space",
                format!(
                    "only {:.1} GiB free on the database volume ({})",
                    free as f64 / (1024.0 * 1024.0 * 1024.0),
                    db_file
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                ),
            )),
            Some(_) | None => {}
        }
    }

    issues
}

/// Counts configured indexer entries across all kinds in the
/// `setup.indexers` JSON (`{"torznab": [...], "newznab": [...]}`).
fn count_indexers(raw: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.as_object().map(|o| {
                o.values()
                    .filter_map(|l| l.as_array())
                    .map(|a| a.len())
                    .sum()
            })
        })
        .unwrap_or(0)
}

/// The embedded migration scripts mirrored from `storage::migrations`. Kept
/// here solely for read-only drift detection (see [`health_issues`]); the
/// authoritative runner remains `storage::migrations`. Files added later are
/// simply not verified by this check until mirrored.
const EMBEDDED_MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init.sql", include_str!("migrations/0001_init.sql")),
    (
        "0002_sessions_csrf.sql",
        include_str!("migrations/0002_sessions_csrf.sql"),
    ),
    (
        "0003_core_domain.sql",
        include_str!("migrations/0003_core_domain.sql"),
    ),
    (
        "0004_acquisitions.sql",
        include_str!("migrations/0004_acquisitions.sql"),
    ),
    (
        "0005_acquisitions_fields.sql",
        include_str!("migrations/0005_acquisitions_fields.sql"),
    ),
    (
        "0006_acquisition_verify_stall.sql",
        include_str!("migrations/0006_acquisition_verify_stall.sql"),
    ),
    (
        "0007_blacklist.sql",
        include_str!("migrations/0007_blacklist.sql"),
    ),
    (
        "0008_verify_evidence.sql",
        include_str!("migrations/0008_verify_evidence.sql"),
    ),
    (
        "0009_hardening.sql",
        include_str!("migrations/0009_hardening.sql"),
    ),
    ("0010_totp.sql", include_str!("migrations/0010_totp.sql")),
    (
        "0011_arr_domain.sql",
        include_str!("migrations/0011_arr_domain.sql"),
    ),
];

fn sha256_hex(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data.as_bytes());
    hex::encode(h.finalize())
}

/// Read-only drift check: returns `(name, recorded_hash, current_hash)` for
/// the first applied migration whose recorded hash no longer matches its
/// embedded file, or `None` when everything matches (or the row names a file
/// this module does not embed).
async fn migration_drift(pool: &SqlitePool) -> Option<(String, String, String)> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, hash FROM schema_migrations")
        .fetch_all(pool)
        .await
        .ok()?;
    for (name, recorded) in rows {
        if let Some((_, contents)) = EMBEDDED_MIGRATIONS.iter().find(|(n, _)| *n == name) {
            let current = sha256_hex(contents);
            if current != recorded {
                return Some((name, recorded, current));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::migrations;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let p = crate::storage::open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    fn mapping(client_path: &str, local_path: &str) -> RemotePathMapping {
        RemotePathMapping {
            id: "m".into(),
            client_path: client_path.into(),
            local_path: local_path.into(),
        }
    }

    #[test]
    fn translate_uses_longest_prefix() {
        let maps = vec![
            mapping("/mnt/remote", "/data"),
            mapping("/mnt/remote/data", "/srv/media"),
        ];
        assert_eq!(
            translate(&maps, "/mnt/remote/data/movies/x.mkv").as_deref(),
            Some("/srv/media/movies/x.mkv")
        );
        assert_eq!(translate(&maps, "/mnt/other/file"), None);
    }

    #[test]
    fn translate_ignores_empty_prefix_and_exact_match() {
        let maps = vec![mapping("", "/never"), mapping("/mnt/a", "/b")];
        assert_eq!(translate(&maps, "/unmapped"), None);
        assert_eq!(translate(&maps, "/mnt/a").as_deref(), Some("/b"));
    }

    #[test]
    fn backups_listing_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join(format!("{BACKUP_PREFIX}20260101T000000Z.db")),
            b"x",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join(format!("{BACKUP_PREFIX}20260202T000000Z.db")),
            b"yy",
        )
        .unwrap();
        std::fs::write(dir.path().join("unrelated.db"), b"z").unwrap();
        let listed = list_backups(dir.path());
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed[0].name,
            format!("{BACKUP_PREFIX}20260202T000000Z.db")
        );
        assert_eq!(listed[1].size_bytes, 1);
        assert!(list_backups(&dir.path().join("missing")).is_empty());
    }

    #[tokio::test]
    async fn vacuum_into_creates_nonempty_backup() {
        let (dir, p) = pool().await;
        let dest = backup_database(&p, &dir.path().join("backups"))
            .await
            .unwrap();
        assert!(dest.is_file());
        assert!(dest.metadata().unwrap().len() > 0);
        let listed = list_backups(dest.parent().unwrap());
        assert_eq!(listed.len(), 1);
        assert!(listed[0].size_bytes > 0);
    }

    #[tokio::test]
    async fn mapping_crud_upserts_on_client_path() {
        let (_d, p) = pool().await;
        let m = upsert_mapping(&p, "/mnt/r", "/local").await.unwrap();
        let again = upsert_mapping(&p, "/mnt/r", "/local2").await.unwrap();
        // ON CONFLICT DO UPDATE keeps the original row (and its id).
        assert_eq!(m.id, again.id);
        assert_eq!(again.local_path, "/local2");
        assert_eq!(list_mappings(&p).await.unwrap().len(), 1);
        assert!(
            translate(&list_mappings(&p).await.unwrap(), "/mnt/r/x").as_deref()
                == Some("/local2/x")
        );
        assert!(delete_mapping(&p, &again.id).await.unwrap());
        assert!(!delete_mapping(&p, &again.id).await.unwrap());
        assert!(list_mappings(&p).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn healthy_pool_has_no_issues() {
        let (_d, p) = pool().await;
        // A fresh pool otherwise warns about unconfigured indexers.
        settings::upsert(
            &p,
            "setup.indexers",
            r#"{"torznab": [{"name": "ix"}], "newznab": []}"#,
        )
        .await
        .unwrap();
        assert!(health_issues(&p).await.is_empty());
    }

    #[tokio::test]
    async fn four_recent_failed_jobs_emit_issue() {
        let (_d, p) = pool().await;
        for i in 0..4 {
            sqlx::query(
                "INSERT INTO jobs (id, kind, state, attempt, run_after, created_at, updated_at)
                 VALUES (?, 'search_run', 'failed', 3, ?, ?, ?)",
            )
            .bind(format!("j{i}"))
            .bind(timeutil::now())
            .bind(timeutil::now())
            .bind(timeutil::now())
            .execute(&p)
            .await
            .unwrap();
        }
        let issues = health_issues(&p).await;
        assert!(issues
            .iter()
            .any(|i| i.source == "failed_jobs" && i.severity == "error"));
    }

    #[tokio::test]
    async fn missing_indexers_emit_warning() {
        let (_d, p) = pool().await;
        let issues = health_issues(&p).await;
        assert!(issues
            .iter()
            .any(|i| i.source == "indexers" && i.severity == "warning"));
    }

    #[tokio::test]
    async fn empty_indexer_lists_emit_warning_but_populated_do_not() {
        let (_d, p) = pool().await;
        settings::upsert(&p, "setup.indexers", r#"{"torznab": [], "newznab": []}"#)
            .await
            .unwrap();
        assert!(health_issues(&p)
            .await
            .iter()
            .any(|i| i.source == "indexers"));
        settings::upsert(
            &p,
            "setup.indexers",
            r#"{"torznab": [{"name": "ix"}], "newznab": []}"#,
        )
        .await
        .unwrap();
        assert!(!health_issues(&p)
            .await
            .iter()
            .any(|i| i.source == "indexers"));
    }

    #[tokio::test]
    async fn stale_nonterminal_acquisition_emits_warning() {
        let (_d, p) = pool().await;
        let old = timeutil::fmt(Utc::now() - chrono::Duration::hours(30));
        // Build a FK-valid acquisition through the domain helpers, then
        // backdate its activity timestamps past the 24h threshold.
        let t = crate::storage::domain::create_wanted_target(&p, "movie", "tmdb", "1", None)
            .await
            .unwrap();
        let run = crate::storage::domain::create_search_run(&p, &t.id)
            .await
            .unwrap();
        let cand = crate::storage::domain::insert_candidate(&p, &run.id, "torznab", "{}", None)
            .await
            .unwrap();
        acquisitions::create_acquisition(&p, &cand.id, &t.id, "torrent", "qbittorrent", "default")
            .await
            .unwrap();
        sqlx::query("UPDATE acquisitions SET created_at = ?, updated_at = ? WHERE 1 = 1")
            .bind(&old)
            .bind(&old)
            .execute(&p)
            .await
            .unwrap();
        let issues = health_issues(&p).await;
        assert!(issues.iter().any(|i| i.source == "stale_acquisitions"));
    }

    #[test]
    fn dir_usage_sums_files_skips_symlinks_and_caps_depth() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), [0u8; 10]).unwrap();
        let sub = dir.path().join("s");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.bin"), [0u8; 5]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("a.bin"), dir.path().join("link.bin")).unwrap();
        let usage = dir_usage(dir.path());
        assert_eq!(usage.bytes_used, 15);
        assert!(!usage.truncated);
    }

    #[tokio::test]
    async fn tampered_migration_hash_emits_drift_issue() {
        let (_d, p) = pool().await;
        sqlx::query("UPDATE schema_migrations SET hash = 'deadbeef' WHERE name = '0001_init.sql'")
            .execute(&p)
            .await
            .unwrap();
        assert!(health_issues(&p)
            .await
            .iter()
            .any(|i| i.source == "migrations" && i.severity == "error"));
    }
    #[test]
    fn disk_free_reports_positive_space_on_real_volume() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            let free = disk_free(dir.path()).expect("statvfs on a real tempdir");
            assert!(free > 0);
            // statvfs on a path that cannot exist must fail gracefully.
            assert_eq!(disk_free(&dir.path().join("no/such/dir")), None);
        }
        // Non-unix fallback is always None.
        #[cfg(not(unix))]
        assert_eq!(disk_free(dir.path()), None);
    }

    #[tokio::test]
    async fn pending_restore_marker_emits_health_issue() {
        let (_d, p) = pool().await;
        assert!(!health_issues(&p)
            .await
            .iter()
            .any(|i| i.source == "pending_restore"));

        let db_file = database_file(&p).await.expect("file-backed test pool");
        let pending = PathBuf::from(format!("{}.restore.pending", db_file.display()));
        std::fs::write(&pending, b"staged").unwrap();
        let issues = health_issues(&p).await;
        let issue = issues
            .iter()
            .find(|i| i.source == "pending_restore")
            .expect("staged marker must surface");
        assert_eq!(issue.severity, "warning");
        assert!(issue.message.contains("restore.pending"));
    }
}
