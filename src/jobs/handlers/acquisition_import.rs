//! `acquisition_import`: copy/hardlink the verified file into the library.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_import.go`):
//! - Hardlink-first with copy fallback on cross-device (in
//!   [`crate::pipeline::import`]).
//! - Destination components are sanitized; destinations are uniquified.
//! - Dry-run computes the planned path without writing anything.

use crate::jobs::handlers::{load_setup_paths, publish};
use crate::jobs::ImportPayload;
use crate::pipeline::import;
use crate::state::AppState;
use crate::storage::acquisitions;
use crate::storage::jobs::{self, Job};

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: ImportPayload = parse_payload(job)?;
    if payload.acquisition_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }

    let a = acquisitions::get(&state.db, &payload.acquisition_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("acquisition_not_found")?;

    let verified_path = a
        .verified_path
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing_verified_path")?;

    let paths = load_setup_paths(state).await?;
    let library_root = paths
        .library_roots
        .first()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or("no_library_roots")?;

    publish(
        state,
        "import.started",
        serde_json::json!({
            "acquisitionId": a.id,
            "verifiedPath": verified_path,
            "dryRun": state.config.dry_run
        }),
    );

    let (kind, name) = load_target_for_import(state, &a.target_id).await;
    let (dest_dir, dest_path) =
        import::compute_import_destination(library_root, &kind, &name, &verified_path);

    let dest = if state.config.dry_run {
        // Compute what would have happened; do not mutate disk.
        dest_path.clone()
    } else {
        match import::import_file_to_path(&verified_path, &dest_path) {
            Ok(d) => d,
            Err(e) => {
                acquisitions::mark_failed(&state.db, &a.id, &e)
                    .await
                    .map_err(|db| db.to_string())?;
                publish(
                    state,
                    "import.failed",
                    serde_json::json!({
                        "acquisitionId": a.id,
                        "error": e,
                        "dryRun": state.config.dry_run
                    }),
                );
                return Err(e);
            }
        }
    };

    acquisitions::mark_imported(&state.db, &a.id, &dest)
        .await
        .map_err(|e| e.to_string())?;
    publish(
        state,
        "import.completed",
        serde_json::json!({
            "acquisitionId": a.id,
            "importedPath": dest,
            "destDir": dest_dir,
            "dryRun": state.config.dry_run
        }),
    );
    Ok(())
}

/// Resolves the target's kind and display name for library layout. Falls back
/// through payload query → `<source>-<id>` → target id, mirroring the original.
async fn load_target_for_import(state: &AppState, target_id: &str) -> (String, String) {
    #[derive(sqlx::FromRow)]
    struct Row {
        kind: String,
        external_source: String,
        external_id: String,
        payload_json: Option<String>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT kind, external_source, external_id, payload_json FROM wanted_targets WHERE id = ?",
    )
    .bind(target_id.trim())
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);
    let Some(row) = row else {
        return (String::new(), target_id.to_string());
    };
    let mut name = String::new();
    if let Some(payload) = &row.payload_json {
        if let Ok(m) = serde_json::from_str::<serde_json::Value>(payload) {
            if let Some(q) = m.get("query").and_then(|q| q.as_str()) {
                name = q.trim().to_string();
            }
        }
    }
    if name.is_empty() {
        name = if !row.external_source.is_empty() && !row.external_id.is_empty() {
            format!("{}-{}", row.external_source.trim(), row.external_id.trim())
        } else {
            target_id.to_string()
        };
    }
    (row.kind.trim().to_string(), name)
}

fn parse_payload(job: &Job) -> Result<ImportPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use super::*;
    use crate::jobs::handlers::testutil;
    use std::sync::Arc;

    async fn seed(state: &AppState, verified_path: Option<&str>) -> String {
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, payload_json, created_at, updated_at)
             VALUES ('t1', 'movie', 'tmdb', '550', 'wanted', ?, ?, ?)",
        )
        .bind(r#"{"query":"Test Movie"}"#)
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
            "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, verified_path, created_at, updated_at)
             VALUES ('a1', 'c1', 't1', 'torrent', 'verified', 'qbittorrent', 'default', ?, ?, ?)",
        )
        .bind(verified_path)
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();

        let lib = tempfile::tempdir().unwrap();
        let lib_path = lib.path().to_string_lossy().into_owned();
        // Keep the tempdir alive for the process lifetime of the test.
        std::mem::forget(lib);
        crate::storage::settings::upsert(
            &state.db,
            "setup.paths",
            &serde_json::json!({
                "downloadsCompletedPath": "/tmp",
                "libraryRoots": [lib_path],
                "quarantinePath": "/tmp/quarantine-root"
            })
            .to_string(),
        )
        .await
        .unwrap();
        "a1".to_string()
    }

    async fn import_job(state: &AppState) -> Job {
        jobs::enqueue(
            &state.db,
            "acquisition_import",
            Some(&ImportPayload {
                acquisition_id: "a1".into(),
            }),
            None,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn dryrun_records_planned_path_without_writing() {
        let (_dir, state) = testutil::test_state().await;
        seed(&state, Some("/downloads/movie.mkv")).await;
        let job = import_job(&state).await;

        handle(&state, &job).await.unwrap();

        let a = acquisitions::get(&state.db, "a1").await.unwrap().unwrap();
        assert_eq!(a.status, "imported");
        let planned = a.imported_path.unwrap();
        assert!(
            planned.contains("Movies/Test Movie/Test Movie.mkv"),
            "{planned}"
        );
        assert!(!std::path::Path::new(&planned).exists());
    }

    #[tokio::test]
    async fn real_import_hardlinks_into_library() {
        let (_dir, mut state) = testutil::test_state().await;
        let cfg = (*state.config).clone();
        state.config = Arc::new(crate::config::Config {
            dry_run: false,
            ..cfg
        });

        let downloads = tempfile::tempdir().unwrap();
        let src = downloads.path().join("movie.mkv");
        std::fs::write(&src, vec![0u8; 1024]).unwrap();

        seed(&state, Some(src.to_str().unwrap())).await;
        let job = import_job(&state).await;

        handle(&state, &job).await.unwrap();

        let a = acquisitions::get(&state.db, "a1").await.unwrap().unwrap();
        let imported = a.imported_path.unwrap();
        assert!(
            std::path::Path::new(&imported).exists(),
            "file must exist at {imported}"
        );
        assert_eq!(
            std::fs::metadata(&imported).unwrap().ino(),
            std::fs::metadata(&src).unwrap().ino(),
            "same filesystem: expect hardlink"
        );
    }

    #[tokio::test]
    async fn missing_verified_path_fails() {
        let (_dir, state) = testutil::test_state().await;
        seed(&state, None).await;
        let job = import_job(&state).await;
        assert_eq!(
            handle(&state, &job).await.unwrap_err(),
            "missing_verified_path"
        );
    }

    #[tokio::test]
    async fn import_failure_marks_acquisition_failed() {
        let (_dir, mut state) = testutil::test_state().await;
        let cfg = (*state.config).clone();
        state.config = Arc::new(crate::config::Config {
            dry_run: false,
            ..cfg
        });
        seed(&state, Some("/nonexistent/missing.mkv")).await;
        let job = import_job(&state).await;

        assert!(handle(&state, &job).await.is_err());
        let a = acquisitions::get(&state.db, "a1").await.unwrap().unwrap();
        assert_eq!(a.status, "failed");
    }
}
