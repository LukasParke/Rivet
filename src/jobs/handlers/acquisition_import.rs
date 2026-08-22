//! `acquisition_import`: copy/hardlink the verified file into the library.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_import.go`):
//! - Hardlink-first with copy fallback on cross-device (in
//!   [`crate::pipeline::import`]).
//! - Destination components are sanitized; destinations are uniquified.
//! - Dry-run computes the planned path without writing anything.

use std::path::Path;

use crate::domain::naming::{render_series, NamingContext};
use crate::jobs::handlers::{load_setup_paths, publish};
use crate::jobs::ImportPayload;
use crate::pipeline::import;
use crate::state::AppState;
use crate::storage::acquisitions::{self, Acquisition};
use crate::storage::jobs::Job;

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
    let target = load_target_for_import(state, &a.target_id).await;
    let (dest_dir, mut dest_path) = import::compute_import_destination(
        library_root,
        &target.kind,
        &target.name,
        &verified_path,
    );

    // Series naming template: when `naming.series` is configured the file
    // name is re-rendered from the parsed release plus stored series/episode
    // metadata. Without a template (or on a blank render) the legacy name
    // computed above stays untouched.
    let original_name = file_name_of(&dest_path);
    let templated = apply_series_naming(state, &a, &target, &verified_path).await;
    let mut renamed_from: Option<String> = None;
    if let Some((stem, _)) = &templated {
        let ext = Path::new(&dest_path)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let new_name = format!("{stem}{ext}");
        if !new_name.trim().is_empty() && new_name != original_name {
            renamed_from = Some(original_name.clone());
            if let Some(dir) = Path::new(&dest_path).parent() {
                dest_path = dir.join(&new_name).to_string_lossy().into_owned();
            }
        }
    }

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
    // The rename only happened when the file actually landed on disk, so
    // dry-runs report the templated name without recording a `renamed` event.
    if let (false, Some(previous)) = (state.config.dry_run, renamed_from.as_deref()) {
        record_rename_event(
            state,
            &a,
            previous,
            &dest,
            templated.as_ref().map(|(_, q)| q.as_str()).unwrap_or(""),
        )
        .await;
    }
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

/// Target metadata resolved for library layout and naming decisions.
struct TargetInfo {
    kind: String,
    name: String,
    external_source: String,
    external_id: String,
    payload_json: Option<String>,
}

/// Resolves the target's kind and display name for library layout. Falls back
/// through payload query → `<source>-<id>` → target id, mirroring the original.
async fn load_target_for_import(state: &AppState, target_id: &str) -> TargetInfo {
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
        return TargetInfo {
            kind: String::new(),
            name: target_id.to_string(),
            external_source: String::new(),
            external_id: String::new(),
            payload_json: None,
        };
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
    TargetInfo {
        kind: row.kind.trim().to_string(),
        name,
        external_source: row.external_source.trim().to_string(),
        external_id: row.external_id.trim().to_string(),
        payload_json: row.payload_json,
    }
}

/// Renders a file-name stem from the configured series naming template.
///
/// Returns `None` whenever naming does not apply: the target kind is not
/// series-shaped, no `naming.series` template is configured, or the render
/// came out empty. The parsed quality label rides along for the `renamed`
/// history event.
async fn apply_series_naming(
    state: &AppState,
    a: &Acquisition,
    target: &TargetInfo,
    verified_path: &str,
) -> Option<(String, String)> {
    match target.kind.to_ascii_lowercase().as_str() {
        "series" | "episode" | "season_pack" | "tv" => {}
        _ => return None,
    }
    let template = load_series_template(state).await?;

    let source_title = candidate_source_title(state, &a.candidate_id, verified_path).await;
    let parsed = crate::domain::parsing::parse(&source_title);

    let season = parsed.seasons.first().copied();
    let mut episodes = parsed.episodes.clone();
    if episodes.is_empty() {
        if let Some((start, end)) = parsed.episode_range {
            episodes.extend(start..=end);
        }
    }
    if episodes.is_empty() {
        episodes = target_payload_episodes(target);
    }

    // Canonical series identity from storage, matched by external id.
    let series = lookup_series(state, target).await;

    // Episode titles aligned with `episodes` (blank where unknown; the
    // template engine drops blank titles).
    let mut episode_titles = vec![String::new(); episodes.len()];
    if let (Some((series_id, _)), Some(season), false) = (&series, season, episodes.is_empty()) {
        if let Ok(all) = crate::storage::series::list_episodes_by_series(&state.db, series_id).await
        {
            for (i, n) in episodes.iter().enumerate() {
                episode_titles[i] = all
                    .iter()
                    .find(|e| e.season_number == season && e.episode_number == *n)
                    .and_then(|e| e.title.clone())
                    .unwrap_or_default();
            }
        }
    }

    let ctx = NamingContext {
        title: series
            .map(|(_, title)| title)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| parsed.title.clone()),
        year: parsed.year.map(i64::from),
        season,
        episodes,
        episode_titles,
        quality_label: parsed.quality.label(),
        languages: parsed.languages.clone(),
        group: parsed.release_group.clone(),
        edition: parsed.edition.clone(),
        air_date: parsed.air_date.clone(),
        ..NamingContext::default()
    };
    let stem = render_series(
        &template.standard_format,
        &template.multi_episode_format,
        &ctx,
        true,
    );
    let stem = stem.trim().to_string();
    (!stem.is_empty()).then_some((stem, ctx.quality_label))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesNamingTemplate {
    standard_format: String,
    #[serde(default)]
    multi_episode_format: String,
}

/// Loads the `naming.series` settings blob (`{standardFormat, ...}`).
/// `None` when unset, unparsable, or carrying a blank standard format.
async fn load_series_template(state: &AppState) -> Option<SeriesNamingTemplate> {
    let raw = crate::storage::settings::get(&state.db, "naming.series")
        .await
        .ok()
        .flatten()?;
    let template: SeriesNamingTemplate = serde_json::from_str(&raw).ok()?;
    (!template.standard_format.trim().is_empty()).then_some(template)
}

/// Release title used for parsing: the candidate's raw indexer title when
/// present, otherwise the verified file's stem.
async fn candidate_source_title(
    state: &AppState,
    candidate_id: &str,
    verified_path: &str,
) -> String {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT raw_json FROM release_candidates WHERE id = ?")
            .bind(candidate_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    raw.as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            Path::new(verified_path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

/// Episode numbers recorded on the wanted target payload (interactive grabs),
/// used when the release name itself carries no episode tokens.
fn target_payload_episodes(target: &TargetInfo) -> Vec<i64> {
    target
        .payload_json
        .as_deref()
        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .and_then(|v| v.get("episodes").and_then(|e| e.as_array()).cloned())
        .map(|arr| arr.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default()
}

/// Resolves `(id, title)` of the stored series matching the target's external
/// tvdb/tmdb id. `None` for other sources or unknown ids.
async fn lookup_series(state: &AppState, target: &TargetInfo) -> Option<(String, String)> {
    let column = match target.external_source.to_ascii_lowercase().as_str() {
        "tvdb" => "tvdb_id",
        "tmdb" => "tmdb_id",
        _ => return None,
    };
    let external_id: i64 = target.external_id.parse().ok()?;
    // `column` is one of two compile-time constants above, never user input.
    let sql = format!("SELECT id, title FROM series WHERE {column} = ? ORDER BY id LIMIT 1");
    sqlx::query_as::<_, (String, String)>(&sql)
        .bind(external_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

/// Best-effort `renamed` history row; a failed audit write never fails the
/// import itself.
async fn record_rename_event(
    state: &AppState,
    a: &Acquisition,
    previous_name: &str,
    new_path: &str,
    quality: &str,
) {
    let data = serde_json::json!({ "previousPath": previous_name, "path": new_path });
    if let Err(e) = sqlx::query(
        "INSERT INTO history_events (id, event_type, acquisition_id, source_title, quality, \
         data_json, created_at) VALUES (?, 'renamed', ?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&a.id)
    .bind(previous_name)
    .bind(quality)
    .bind(data.to_string())
    .bind(crate::timeutil::now())
    .execute(&state.db)
    .await
    {
        publish(
            state,
            "import.rename_history_failed",
            serde_json::json!({ "acquisitionId": a.id, "error": e.to_string() }),
        );
    }
}

fn file_name_of(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn parse_payload(job: &Job) -> Result<ImportPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;
    use crate::storage::jobs;
    use std::os::unix::fs::MetadataExt;
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

    /// Seeds a series-shaped pipeline row set: episode target `ts` (tvdb 42),
    /// candidate `cs` with a release-name title, acquisition `as`, plus the
    /// stored series `s1` and its S01E05 "Pilot" episode.
    async fn seed_series(state: &AppState, verified_path: Option<&str>) -> String {
        let now = crate::timeutil::now();
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, created_at, updated_at)
             VALUES ('ts', 'episode', 'tvdb', '42', 'wanted', ?, ?)",
        )
        .bind(&now)
        .bind(&now)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO search_runs (id, target_id, status) VALUES ('rs', 'ts', 'running')",
        )
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO release_candidates (id, search_run_id, indexer_name, raw_json, created_at)
             VALUES ('cs', 'rs', 'ix', ?, ?)",
        )
        .bind(r#"{"title":"Show.S01E05.WEB-DL.1080p.x264-GROUP"}"#)
        .bind(&now)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, verified_path, created_at, updated_at)
             VALUES ('as', 'cs', 'ts', 'torrent', 'verified', 'qbittorrent', 'default', ?, ?, ?)",
        )
        .bind(verified_path)
        .bind(&now)
        .bind(&now)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO series (id, tvdb_id, title, status, path, added_at, updated_at)
             VALUES ('s1', 42, 'Show', 'continuing', '/tv/Show', ?, ?)",
        )
        .bind(&now)
        .bind(&now)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO episodes (id, series_id, season_number, episode_number, title)
             VALUES ('es1', 's1', 1, 5, 'Pilot')",
        )
        .execute(&state.db)
        .await
        .unwrap();

        let lib = tempfile::tempdir().unwrap();
        let lib_path = lib.path().to_string_lossy().into_owned();
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
        "as".to_string()
    }

    async fn set_series_template(state: &AppState, standard: &str) {
        crate::storage::settings::upsert(
            &state.db,
            "naming.series",
            &serde_json::json!({ "standardFormat": standard, "multiEpisodeFormat": "" })
                .to_string(),
        )
        .await
        .unwrap();
    }

    async fn series_import_job(state: &AppState) -> Job {
        jobs::enqueue(
            &state.db,
            "acquisition_import",
            Some(&ImportPayload {
                acquisition_id: "as".into(),
            }),
            None,
        )
        .await
        .unwrap()
    }

    async fn renamed_event_count(state: &AppState) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM history_events WHERE event_type = 'renamed'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap()
    }

    /// Returns the kept-alive tempdirs (db + downloads) so the database and
    /// source files outlive the state they back.
    async fn real_import_state() -> (Vec<tempfile::TempDir>, AppState) {
        let (db_dir, mut state) = testutil::test_state().await;
        let cfg = (*state.config).clone();
        state.config = Arc::new(crate::config::Config {
            dry_run: false,
            ..cfg
        });
        let downloads = tempfile::tempdir().unwrap();
        (vec![db_dir, downloads], state)
    }

    #[tokio::test]
    async fn series_template_renames_real_import() {
        let (keep, state) = real_import_state().await;
        let src = keep[1]
            .path()
            .join("Show.S01E05.WEB-DL.1080p.x264-GROUP.mkv");
        std::fs::write(&src, vec![0u8; 1024]).unwrap();
        seed_series(&state, Some(src.to_str().unwrap())).await;
        set_series_template(
            &state,
            "{Title} - S{Season:00}E{Episode:00} - {Quality Full}",
        )
        .await;
        let job = series_import_job(&state).await;

        handle(&state, &job).await.unwrap();

        let a = acquisitions::get(&state.db, "as").await.unwrap().unwrap();
        let imported = a.imported_path.unwrap();
        assert!(
            imported.contains("/TV/") && imported.ends_with("/Show - S01E05 - WEB-DL 1080p.mkv"),
            "{imported}"
        );
        assert!(
            Path::new(&imported).exists(),
            "file must exist at {imported}"
        );
        assert_eq!(renamed_event_count(&state).await, 1);
    }

    #[tokio::test]
    async fn template_uses_stored_episode_title() {
        let (keep, state) = real_import_state().await;
        let src = keep[1]
            .path()
            .join("Show.S01E05.WEB-DL.1080p.x264-GROUP.mkv");
        std::fs::write(&src, vec![0u8; 1024]).unwrap();
        seed_series(&state, Some(src.to_str().unwrap())).await;
        set_series_template(
            &state,
            "{Title} - S{Season:00}E{Episode:00} - {Episode Title}",
        )
        .await;
        let job = series_import_job(&state).await;

        handle(&state, &job).await.unwrap();

        let a = acquisitions::get(&state.db, "as").await.unwrap().unwrap();
        let imported = a.imported_path.unwrap();
        assert!(
            imported.ends_with("/Show - S01E05 - Pilot.mkv"),
            "{imported}"
        );
    }

    #[tokio::test]
    async fn no_template_preserves_legacy_names() {
        let (keep, state) = real_import_state().await;
        let src = keep[1]
            .path()
            .join("Show.S01E05.WEB-DL.1080p.x264-GROUP.mkv");
        std::fs::write(&src, vec![0u8; 1024]).unwrap();
        seed_series(&state, Some(src.to_str().unwrap())).await;
        let job = series_import_job(&state).await;

        handle(&state, &job).await.unwrap();
        let a = acquisitions::get(&state.db, "as").await.unwrap().unwrap();
        let imported = a.imported_path.unwrap();
        // No payload query on this target, so the display name falls back to
        // `<source>-<id>`; the file name must be the untouched legacy form.
        assert!(
            imported.ends_with("/TV/tvdb-42/Season 01/tvdb-42 - Imported.mkv"),
            "legacy name must be untouched: {imported}"
        );
        assert_eq!(renamed_event_count(&state).await, 0);
    }

    #[tokio::test]
    async fn dryrun_reports_templated_name_without_writing_or_history() {
        let (_dir, state) = testutil::test_state().await;
        seed_series(
            &state,
            Some("/downloads/Show.S01E05.WEB-DL.1080p.x264-GROUP.mkv"),
        )
        .await;
        set_series_template(
            &state,
            "{Title} - S{Season:00}E{Episode:00} - {Quality Full}",
        )
        .await;
        let job = series_import_job(&state).await;

        handle(&state, &job).await.unwrap();

        let a = acquisitions::get(&state.db, "as").await.unwrap().unwrap();
        let planned = a.imported_path.unwrap();
        assert!(
            planned.ends_with("/Show - S01E05 - WEB-DL 1080p.mkv"),
            "{planned}"
        );
        assert!(!Path::new(&planned).exists(), "dry-run must not write");
        assert_eq!(renamed_event_count(&state).await, 0);
    }
}
