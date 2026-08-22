//! Series / seasons / episodes / episode-file storage for the 0011 schema.
//!
//! Upserts match existing rows by tmdb id, tvdb id, or library path
//! (first hit wins) so a re-import of the same show updates in place.
//! `monitored` flags follow Sonarr semantics, documented per function:
//! monitoring a series cascades down to seasons and episodes; monitoring a
//! season marks the series and its episodes; episode flags are leaf-only.
//!
//! `genres_json` / `tags_json` columns are exposed as raw JSON array strings
//! (`genresJson` / `tagsJson` fields); the API layer parses them.

use sqlx::SqlitePool;

use crate::timeutil;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub id: String,
    pub tvdb_id: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub title: String,
    pub sort_title: Option<String>,
    pub status: String,
    pub overview: Option<String>,
    pub network: Option<String>,
    pub first_aired: Option<String>,
    pub genres_json: Option<String>,
    pub path: String,
    pub quality_profile: String,
    pub monitored: bool,
    pub season_folder: bool,
    pub tags_json: Option<String>,
    pub added_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Season {
    pub id: String,
    pub series_id: String,
    pub number: i64,
    pub monitored: bool,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: String,
    pub series_id: String,
    pub season_number: i64,
    pub episode_number: i64,
    pub absolute_episode_number: Option<i64>,
    pub title: Option<String>,
    pub air_date_utc: Option<String>,
    pub runtime: Option<i64>,
    pub overview: Option<String>,
    pub monitored: bool,
    pub has_file: bool,
    pub episode_file_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeFile {
    pub id: String,
    pub series_id: String,
    pub season_number: i64,
    pub episode_numbers_json: String,
    pub relative_path: String,
    pub size_bytes: i64,
    pub quality: String,
    pub quality_score: i64,
    pub languages_json: Option<String>,
    pub date_added: String,
    pub original_path: Option<String>,
}

/// Input for [`upsert_series`]. `id`, when given, is reused on insert;
/// otherwise a fresh uuid is generated.
#[derive(Debug, Clone)]
pub struct SeriesUpsert {
    pub id: Option<String>,
    pub tvdb_id: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub title: String,
    pub sort_title: Option<String>,
    pub status: String,
    pub overview: Option<String>,
    pub network: Option<String>,
    pub first_aired: Option<String>,
    /// Serialized as the `genres_json` JSON array.
    pub genres: Vec<String>,
    pub path: String,
    pub quality_profile: String,
    pub monitored: bool,
    pub season_folder: bool,
    /// Serialized as the `tags_json` JSON array.
    pub tags: Vec<String>,
}

/// Input episode for [`bulk_upsert_episodes`].
#[derive(Debug, Clone)]
pub struct EpisodeUpsert {
    pub season_number: i64,
    pub episode_number: i64,
    pub absolute_episode_number: Option<i64>,
    pub title: Option<String>,
    pub air_date_utc: Option<String>,
    pub runtime: Option<i64>,
    pub overview: Option<String>,
    pub monitored: bool,
}

/// Input for [`insert_episode_file`]. `episode_numbers` are the episode
/// numbers within `season_number` covered by the file.
#[derive(Debug, Clone)]
pub struct NewEpisodeFile {
    pub id: Option<String>,
    pub series_id: String,
    pub season_number: i64,
    pub episode_numbers: Vec<i64>,
    pub relative_path: String,
    pub size_bytes: i64,
    pub quality: String,
    pub quality_score: i64,
    pub languages: Vec<String>,
    pub original_path: Option<String>,
}

fn json_array(v: &[String]) -> Option<String> {
    if v.is_empty() {
        None
    } else {
        Some(serde_json::to_string(v).expect("Vec<String> always serializes"))
    }
}

/// Finds an existing series row by tmdb id, tvdb id, or path (first hit wins).
async fn find_existing(
    pool: &SqlitePool,
    input: &SeriesUpsert,
) -> Result<Option<Series>, sqlx::Error> {
    sqlx::query_as::<_, Series>(
        "SELECT * FROM series WHERE \
         (tmdb_id IS NOT NULL AND tmdb_id = ?) \
         OR (tvdb_id IS NOT NULL AND tvdb_id = ?) \
         OR path = ? LIMIT 1",
    )
    .bind(input.tmdb_id)
    .bind(input.tvdb_id)
    .bind(input.path.trim())
    .fetch_optional(pool)
    .await
}

/// Upserts a series matched by tmdb/tvdb id or path. Returns the stored row
/// and whether it was newly created. On update, `id` and `added_at` are kept.
pub async fn upsert_series(
    pool: &SqlitePool,
    input: &SeriesUpsert,
) -> Result<(Series, bool), sqlx::Error> {
    if let Some(existing) = find_existing(pool, input).await? {
        let now = timeutil::now();
        sqlx::query(
            "UPDATE series SET tvdb_id = ?, tmdb_id = ?, imdb_id = ?, title = ?, sort_title = ?, \
             status = ?, overview = ?, network = ?, first_aired = ?, genres_json = ?, path = ?, \
             quality_profile = ?, monitored = ?, season_folder = ?, tags_json = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(input.tvdb_id)
        .bind(input.tmdb_id)
        .bind(input.imdb_id.as_deref())
        .bind(input.title.trim())
        .bind(input.sort_title.as_deref())
        .bind(&input.status)
        .bind(input.overview.as_deref())
        .bind(input.network.as_deref())
        .bind(input.first_aired.as_deref())
        .bind(json_array(&input.genres))
        .bind(input.path.trim())
        .bind(&input.quality_profile)
        .bind(input.monitored)
        .bind(input.season_folder)
        .bind(json_array(&input.tags))
        .bind(&now)
        .bind(&existing.id)
        .execute(pool)
        .await?;
        let updated = get_series(pool, &existing.id)
            .await?
            .expect("row just updated");
        Ok((updated, false))
    } else {
        let id = input
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = timeutil::now();
        sqlx::query(
            "INSERT INTO series (id, tvdb_id, tmdb_id, imdb_id, title, sort_title, status, \
             overview, network, first_aired, genres_json, path, quality_profile, monitored, \
             season_folder, tags_json, added_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.tvdb_id)
        .bind(input.tmdb_id)
        .bind(input.imdb_id.as_deref())
        .bind(input.title.trim())
        .bind(input.sort_title.as_deref())
        .bind(&input.status)
        .bind(input.overview.as_deref())
        .bind(input.network.as_deref())
        .bind(input.first_aired.as_deref())
        .bind(json_array(&input.genres))
        .bind(input.path.trim())
        .bind(&input.quality_profile)
        .bind(input.monitored)
        .bind(input.season_folder)
        .bind(json_array(&input.tags))
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
        let created = get_series(pool, &id).await?.expect("row just inserted");
        Ok((created, true))
    }
}

pub async fn get_series(pool: &SqlitePool, id: &str) -> Result<Option<Series>, sqlx::Error> {
    sqlx::query_as::<_, Series>("SELECT * FROM series WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Lists series ordered by `COALESCE(sort_title, title)` (case-insensitive).
/// `monitored` filters when given.
pub async fn list_series(
    pool: &SqlitePool,
    monitored: Option<bool>,
) -> Result<Vec<Series>, sqlx::Error> {
    sqlx::query_as::<_, Series>(
        "SELECT * FROM series WHERE (? IS NULL OR monitored = ?) \
         ORDER BY COALESCE(sort_title, title) COLLATE NOCASE",
    )
    .bind(monitored)
    .bind(monitored.unwrap_or_default())
    .fetch_all(pool)
    .await
}

/// Deletes a series; seasons, episodes and episode files die with it via
/// `ON DELETE CASCADE`. Returns whether a row was removed.
pub async fn delete_series(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM series WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Upserts one season on its `(series_id, number)` unique key, updating the
/// monitored flag in place. Returns the stored row.
pub async fn upsert_season(
    pool: &SqlitePool,
    series_id: &str,
    number: i64,
    monitored: bool,
) -> Result<Season, sqlx::Error> {
    if let Some(existing) =
        sqlx::query_as::<_, Season>("SELECT * FROM seasons WHERE series_id = ? AND number = ?")
            .bind(series_id)
            .bind(number)
            .fetch_optional(pool)
            .await?
    {
        sqlx::query("UPDATE seasons SET monitored = ? WHERE id = ?")
            .bind(monitored)
            .bind(&existing.id)
            .execute(pool)
            .await?;
        Ok(Season {
            monitored,
            ..existing
        })
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO seasons (id, series_id, number, monitored) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(series_id)
            .bind(number)
            .bind(monitored)
            .execute(pool)
            .await?;
        Ok(Season {
            id,
            series_id: series_id.to_string(),
            number,
            monitored,
        })
    }
}

pub async fn list_seasons(pool: &SqlitePool, series_id: &str) -> Result<Vec<Season>, sqlx::Error> {
    sqlx::query_as::<_, Season>("SELECT * FROM seasons WHERE series_id = ? ORDER BY number")
        .bind(series_id)
        .fetch_all(pool)
        .await
}

/// Bulk-upserts episodes against the `(series_id, season_number,
/// episode_number)` unique key inside one transaction. Conflicts update
/// metadata and the monitored flag but never touch `has_file` /
/// `episode_file_id` (import state is owned by [`insert_episode_file`] /
/// [`delete_episode_file`]). Idempotent: re-running the same batch changes
/// nothing observable.
///
/// Episodes are sent as chunked multi-row `VALUES` statements (512 rows x
/// 10 bound params = 5120 host parameters, well under SQLite's default
/// `SQLITE_MAX_VARIABLE_NUMBER` of 32766) instead of one round trip per row.
/// Duplicate keys inside one batch keep the *last* occurrence, matching the
/// previous row-at-a-time loop's observable outcome.
pub async fn bulk_upsert_episodes(
    pool: &SqlitePool,
    series_id: &str,
    episodes: &[EpisodeUpsert],
) -> Result<(), sqlx::Error> {
    const ROWS_PER_STATEMENT: usize = 512;

    // Last occurrence of each (season, episode) key wins.
    let mut last_idx: std::collections::HashMap<(i64, i64), usize> =
        std::collections::HashMap::with_capacity(episodes.len());
    for (i, ep) in episodes.iter().enumerate() {
        last_idx.insert((ep.season_number, ep.episode_number), i);
    }
    let deduped: Vec<&EpisodeUpsert> = episodes
        .iter()
        .enumerate()
        .filter(|(i, ep)| last_idx.get(&(ep.season_number, ep.episode_number)) == Some(i))
        .map(|(_, ep)| ep)
        .collect();
    if deduped.is_empty() {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    for chunk in deduped.chunks(ROWS_PER_STATEMENT) {
        let mut sql = String::with_capacity(
            220 + chunk.len() * 96, /* INSERT head + per-row VALUES group */
        );
        sql.push_str(
            "INSERT INTO episodes (id, series_id, season_number, episode_number, \
             absolute_episode_number, title, air_date_utc, runtime, overview, monitored) VALUES ",
        );
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)");
        }
        sql.push_str(
            " ON CONFLICT(series_id, season_number, episode_number) DO UPDATE SET \
             absolute_episode_number = excluded.absolute_episode_number, \
             title = excluded.title, air_date_utc = excluded.air_date_utc, \
             runtime = excluded.runtime, overview = excluded.overview, \
             monitored = excluded.monitored",
        );

        let mut insert = sqlx::query(&sql);
        for ep in chunk {
            insert = insert
                .bind(uuid::Uuid::new_v4().to_string())
                .bind(series_id)
                .bind(ep.season_number)
                .bind(ep.episode_number)
                .bind(ep.absolute_episode_number)
                .bind(ep.title.as_deref())
                .bind(ep.air_date_utc.as_deref())
                .bind(ep.runtime)
                .bind(ep.overview.as_deref())
                .bind(ep.monitored);
        }
        insert.execute(&mut *tx).await?;
    }
    tx.commit().await
}

/// All episodes of a series, ordered by season then episode number.
pub async fn list_episodes_by_series(
    pool: &SqlitePool,
    series_id: &str,
) -> Result<Vec<Episode>, sqlx::Error> {
    sqlx::query_as::<_, Episode>(
        "SELECT * FROM episodes WHERE series_id = ? ORDER BY season_number, episode_number",
    )
    .bind(series_id)
    .fetch_all(pool)
    .await
}

/// Episodes with `air_date_utc` in the inclusive `[start, end]` range
/// (RFC3339 strings compared lexicographically — timeutil guarantees order).
pub async fn list_episodes_by_air_date_range(
    pool: &SqlitePool,
    start: &str,
    end: &str,
) -> Result<Vec<Episode>, sqlx::Error> {
    sqlx::query_as::<_, Episode>(
        "SELECT * FROM episodes WHERE air_date_utc IS NOT NULL AND air_date_utc >= ? \
         AND air_date_utc <= ? ORDER BY air_date_utc",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
}

/// Missing episodes: monitored episodes of monitored series with no file
/// that have already aired (`air_date_utc <= now`). `now` is an RFC3339
/// timestamp string (use [`timeutil::now`]).
pub async fn list_missing_episodes(
    pool: &SqlitePool,
    now: &str,
) -> Result<Vec<Episode>, sqlx::Error> {
    sqlx::query_as::<_, Episode>(
        "SELECT e.* FROM episodes e JOIN series s ON s.id = e.series_id \
         WHERE e.monitored = 1 AND e.has_file = 0 AND s.monitored = 1 \
         AND e.air_date_utc IS NOT NULL AND e.air_date_utc <= ? \
         ORDER BY e.air_date_utc",
    )
    .bind(now)
    .fetch_all(pool)
    .await
}

/// Sets the monitored flag on a series and cascades it to every season and
/// episode of the series (Sonarr semantics: series-level monitoring governs
/// the whole tree). Returns false when the series does not exist.
pub async fn set_series_monitored(
    pool: &SqlitePool,
    id: &str,
    monitored: bool,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE series SET monitored = ?, updated_at = ? WHERE id = ?")
        .bind(monitored)
        .bind(timeutil::now())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE seasons SET monitored = ? WHERE series_id = ?")
        .bind(monitored)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE episodes SET monitored = ? WHERE series_id = ?")
        .bind(monitored)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Sets the monitored flag on one season. Cascades to that season's episodes
/// (monitoring a season monitors its episodes and marks the series monitored;
/// unmonitoring a season unmonitors its episodes but leaves the series flag
/// alone). Returns false when the season does not exist.
pub async fn set_season_monitored(
    pool: &SqlitePool,
    series_id: &str,
    number: i64,
    monitored: bool,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE seasons SET monitored = ? WHERE series_id = ? AND number = ?")
        .bind(monitored)
        .bind(series_id)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE episodes SET monitored = ? WHERE series_id = ? AND season_number = ?")
        .bind(monitored)
        .bind(series_id)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    if monitored {
        sqlx::query("UPDATE series SET monitored = 1 WHERE id = ?")
            .bind(series_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Sets the monitored flag on a single episode. Leaf-only: no cascade.
/// Returns false when the episode does not exist.
pub async fn set_episode_monitored(
    pool: &SqlitePool,
    id: &str,
    monitored: bool,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE episodes SET monitored = ? WHERE id = ?")
        .bind(monitored)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Inserts an episode file and marks the episodes it covers
/// (`has_file = 1`, `episode_file_id` set) in one transaction. Returns the
/// stored row.
pub async fn insert_episode_file(
    pool: &SqlitePool,
    input: &NewEpisodeFile,
) -> Result<EpisodeFile, sqlx::Error> {
    let id = input
        .id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let episode_numbers_json =
        serde_json::to_string(&input.episode_numbers).expect("Vec<i64> always serializes");
    let languages_json = json_array(&input.languages);
    let date_added = timeutil::now();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO episode_files (id, series_id, season_number, episode_numbers_json, \
         relative_path, size_bytes, quality, quality_score, languages_json, date_added, \
         original_path) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.series_id)
    .bind(input.season_number)
    .bind(&episode_numbers_json)
    .bind(&input.relative_path)
    .bind(input.size_bytes)
    .bind(&input.quality)
    .bind(input.quality_score)
    .bind(languages_json.clone())
    .bind(&date_added)
    .bind(input.original_path.as_deref())
    .execute(&mut *tx)
    .await?;
    for number in &input.episode_numbers {
        sqlx::query(
            "UPDATE episodes SET has_file = 1, episode_file_id = ? \
             WHERE series_id = ? AND season_number = ? AND episode_number = ?",
        )
        .bind(&id)
        .bind(&input.series_id)
        .bind(input.season_number)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(EpisodeFile {
        id,
        series_id: input.series_id.clone(),
        season_number: input.season_number,
        episode_numbers_json,
        relative_path: input.relative_path.clone(),
        size_bytes: input.size_bytes,
        quality: input.quality.clone(),
        quality_score: input.quality_score,
        languages_json,
        date_added,
        original_path: input.original_path.clone(),
    })
}

/// Fetches the file attached to an episode (via `episodes.episode_file_id`),
/// if any.
pub async fn get_episode_file_by_episode(
    pool: &SqlitePool,
    episode_id: &str,
) -> Result<Option<EpisodeFile>, sqlx::Error> {
    sqlx::query_as::<_, EpisodeFile>(
        "SELECT f.* FROM episode_files f \
         JOIN episodes e ON e.episode_file_id = f.id WHERE e.id = ?",
    )
    .bind(episode_id)
    .fetch_optional(pool)
    .await
}

/// Deletes an episode file and clears `has_file` / `episode_file_id` on the
/// episodes that referenced it. Returns whether a row was removed.
pub async fn delete_episode_file(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE episodes SET has_file = 0, episode_file_id = NULL WHERE episode_file_id = ?",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let res = sqlx::query("DELETE FROM episode_files WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("s.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    fn series_input(title: &str, path: &str) -> SeriesUpsert {
        SeriesUpsert {
            id: None,
            tvdb_id: None,
            tmdb_id: None,
            imdb_id: None,
            title: title.to_string(),
            sort_title: None,
            status: "continuing".to_string(),
            overview: None,
            network: None,
            first_aired: None,
            genres: vec!["drama".to_string()],
            path: path.to_string(),
            quality_profile: "default".to_string(),
            monitored: true,
            season_folder: true,
            tags: vec![],
        }
    }

    fn episode_input(season: i64, episode: i64, air: Option<&str>) -> EpisodeUpsert {
        EpisodeUpsert {
            season_number: season,
            episode_number: episode,
            absolute_episode_number: Some(season * 100 + episode),
            title: Some(format!("S{season}E{episode}")),
            air_date_utc: air.map(str::to_string),
            runtime: Some(42),
            overview: None,
            monitored: true,
        }
    }

    #[tokio::test]
    async fn upsert_matches_by_tmdb_tvdb_and_path() {
        let (_d, p) = pool().await;

        // Insert with tmdb id.
        let mut input = series_input("Show", "/tv/show");
        input.tmdb_id = Some(111);
        let (s1, created) = upsert_series(&p, &input).await.unwrap();
        assert!(created);

        // Same tmdb id, different path/title -> update in place.
        let mut by_tmdb = series_input("Show Renamed", "/tv/moved");
        by_tmdb.tmdb_id = Some(111);
        let (s2, created) = upsert_series(&p, &by_tmdb).await.unwrap();
        assert!(!created);
        assert_eq!(s2.id, s1.id);
        assert_eq!(s2.title, "Show Renamed");
        assert_eq!(s2.path, "/tv/moved");

        // Same tvdb id matches too.
        sqlx::query("UPDATE series SET tvdb_id = 222 WHERE id = ?")
            .bind(&s1.id)
            .execute(&p)
            .await
            .unwrap();
        let mut by_tvdb = series_input("Show Again", "/tv/again");
        by_tvdb.tvdb_id = Some(222);
        let (s3, created) = upsert_series(&p, &by_tvdb).await.unwrap();
        assert!(!created);
        assert_eq!(s3.id, s1.id);

        // No ids: path alone matches.
        let by_path = series_input("Show Final", "/tv/again");
        let (s4, created) = upsert_series(&p, &by_path).await.unwrap();
        assert!(!created);
        assert_eq!(s4.id, s1.id);

        assert_eq!(list_series(&p, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn series_path_is_unique() {
        let (_d, p) = pool().await;
        upsert_series(&p, &series_input("A", "/tv/a"))
            .await
            .unwrap();

        // A direct INSERT with a duplicate path must violate UNIQUE(path).
        let err = sqlx::query(
            "INSERT INTO series (id, title, path, status, added_at, updated_at) \
             VALUES ('x', 'B', '/tv/a', 'continuing', '2026', '2026')",
        )
        .execute(&p)
        .await;
        assert!(err.is_err(), "unique(path) must reject duplicates");

        // The upsert path resolves to the existing row instead of colliding.
        let mut other = series_input("B", "/tv/a");
        other.tmdb_id = Some(999);
        let (row, created) = upsert_series(&p, &other).await.unwrap();
        assert!(!created);
        assert_eq!(row.title, "B", "matched by path, updated in place");
        assert_eq!(list_series(&p, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_orders_by_sort_title_and_filters_monitored() {
        let (_d, p) = pool().await;
        let mut z = series_input("Zebra", "/tv/z");
        z.sort_title = Some("aaa".to_string());
        let mut a = series_input("Apple", "/tv/a");
        a.monitored = false;
        upsert_series(&p, &z).await.unwrap();
        upsert_series(&p, &a).await.unwrap();
        upsert_series(&p, &series_input("Mango", "/tv/m"))
            .await
            .unwrap();

        let all = list_series(&p, None).await.unwrap();
        let titles: Vec<&str> = all.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Zebra", "Apple", "Mango"], "sort_title wins");

        let monitored = list_series(&p, Some(true)).await.unwrap();
        assert_eq!(monitored.len(), 2);
        assert!(monitored.iter().all(|s| s.monitored));

        let unmonitored = list_series(&p, Some(false)).await.unwrap();
        assert_eq!(unmonitored.len(), 1);
        assert_eq!(unmonitored[0].title, "Apple");
    }

    #[tokio::test]
    async fn delete_series_cascades_seasons_episodes_files() {
        let (_d, p) = pool().await;
        let (s, _) = upsert_series(&p, &series_input("Show", "/tv/show"))
            .await
            .unwrap();
        upsert_season(&p, &s.id, 1, true).await.unwrap();
        bulk_upsert_episodes(&p, &s.id, &[episode_input(1, 1, None)])
            .await
            .unwrap();
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        insert_episode_file(
            &p,
            &NewEpisodeFile {
                id: None,
                series_id: s.id.clone(),
                season_number: 1,
                episode_numbers: vec![1],
                relative_path: "S01E01.mkv".to_string(),
                size_bytes: 1,
                quality: "webdl-1080p".to_string(),
                quality_score: 90,
                languages: vec!["eng".to_string()],
                original_path: None,
            },
        )
        .await
        .unwrap();

        assert!(delete_series(&p, &s.id).await.unwrap());
        assert!(
            !delete_series(&p, &s.id).await.unwrap(),
            "second delete reports false"
        );
        assert!(get_series(&p, &s.id).await.unwrap().is_none());
        assert!(list_seasons(&p, &s.id).await.unwrap().is_empty());
        assert!(list_episodes_by_series(&p, &s.id).await.unwrap().is_empty());
        assert!(
            get_episode_file_by_episode(&p, &eps[0].id)
                .await
                .unwrap()
                .is_none(),
            "episode file died with the series"
        );
    }

    #[tokio::test]
    async fn seasons_upsert_on_series_and_number() {
        let (_d, p) = pool().await;
        let (s, _) = upsert_series(&p, &series_input("Show", "/tv/show"))
            .await
            .unwrap();
        let s1 = upsert_season(&p, &s.id, 1, true).await.unwrap();
        let s1b = upsert_season(&p, &s.id, 1, false).await.unwrap();
        assert_eq!(s1b.id, s1.id, "same (series, number) updates in place");
        assert!(!s1b.monitored);
        upsert_season(&p, &s.id, 2, true).await.unwrap();

        let seasons = list_seasons(&p, &s.id).await.unwrap();
        assert_eq!(seasons.len(), 2);
        assert_eq!(seasons[0].number, 1);
        assert_eq!(seasons[1].number, 2);
        assert!(!seasons[0].monitored && seasons[1].monitored);
    }

    #[tokio::test]
    async fn bulk_upsert_is_idempotent_and_preserves_file_state() {
        let (_d, p) = pool().await;
        let (s, _) = upsert_series(&p, &series_input("Show", "/tv/show"))
            .await
            .unwrap();
        let batch = vec![
            episode_input(1, 1, Some("2026-01-01T00:00:00Z")),
            episode_input(1, 2, Some("2026-01-08T00:00:00Z")),
        ];
        bulk_upsert_episodes(&p, &s.id, &batch).await.unwrap();
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert_eq!(eps.len(), 2);

        // Attach a file to E1, then re-run the same batch.
        let file = insert_episode_file(
            &p,
            &NewEpisodeFile {
                id: None,
                series_id: s.id.clone(),
                season_number: 1,
                episode_numbers: vec![1],
                relative_path: "S01E01.mkv".to_string(),
                size_bytes: 5,
                quality: "webdl-1080p".to_string(),
                quality_score: 90,
                languages: vec![],
                original_path: None,
            },
        )
        .await
        .unwrap();

        bulk_upsert_episodes(&p, &s.id, &batch).await.unwrap();
        let eps2 = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert_eq!(eps2.len(), 2, "no duplicates from re-run");
        assert_eq!(eps2[0].id, eps[0].id, "conflict updates in place");
        assert!(eps2[0].has_file && eps2[0].episode_file_id.as_deref() == Some(&file.id));
        assert!(!eps2[1].has_file);

        // Metadata update flows through on conflict without adding rows.
        let mut renamed = episode_input(1, 2, Some("2026-01-09T00:00:00Z"));
        renamed.title = Some("Renamed".to_string());
        bulk_upsert_episodes(&p, &s.id, &[renamed]).await.unwrap();
        let eps3 = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert_eq!(eps3.len(), 2);
        assert_eq!(eps3[1].title.as_deref(), Some("Renamed"));
        assert_eq!(
            eps3[1].air_date_utc.as_deref(),
            Some("2026-01-09T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn episode_unique_constraint_across_series() {
        let (_d, p) = pool().await;
        let (a, _) = upsert_series(&p, &series_input("A", "/tv/a"))
            .await
            .unwrap();
        let (b, _) = upsert_series(&p, &series_input("B", "/tv/b"))
            .await
            .unwrap();
        bulk_upsert_episodes(&p, &a.id, &[episode_input(1, 1, None)])
            .await
            .unwrap();
        // Same S/E under another series is fine.
        bulk_upsert_episodes(&p, &b.id, &[episode_input(1, 1, None)])
            .await
            .unwrap();
        assert_eq!(list_episodes_by_series(&p, &a.id).await.unwrap().len(), 1);
        assert_eq!(list_episodes_by_series(&p, &b.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn air_date_range_and_missing_filtering() {
        let (_d, p) = pool().await;
        let mut monitored = series_input("Monitored", "/tv/m");
        monitored.monitored = true;
        let (ms, _) = upsert_series(&p, &monitored).await.unwrap();
        let mut unmonitored = series_input("Unmonitored", "/tv/u");
        unmonitored.monitored = false;
        let (us, _) = upsert_series(&p, &unmonitored).await.unwrap();

        let past = |d: &str| format!("2026-01-{d}T00:00:00Z");
        let mut batch = vec![
            episode_input(1, 1, Some(&past("01"))), // aired, missing -> wanted
            episode_input(1, 2, Some(&past("05"))), // aired, has file
            episode_input(1, 3, Some("2030-01-01T00:00:00Z")), // unaired, unmonitored
        ];
        batch[2].monitored = false;
        bulk_upsert_episodes(&p, &ms.id, &batch).await.unwrap();
        // Same aired+missing shape under an unmonitored series -> not wanted.
        bulk_upsert_episodes(&p, &us.id, &[episode_input(1, 1, Some(&past("02")))])
            .await
            .unwrap();

        insert_episode_file(
            &p,
            &NewEpisodeFile {
                id: None,
                series_id: ms.id.clone(),
                season_number: 1,
                episode_numbers: vec![2],
                relative_path: "S01E02.mkv".to_string(),
                size_bytes: 1,
                quality: "webdl-1080p".to_string(),
                quality_score: 90,
                languages: vec![],
                original_path: None,
            },
        )
        .await
        .unwrap();

        // Air-date range: inclusive bounds, all aired episodes in window —
        // calendar parity shows unmonitored series too (greyed client-side).
        let in_range = list_episodes_by_air_date_range(&p, &past("01"), &past("05"))
            .await
            .unwrap();
        assert_eq!(in_range.len(), 3);
        assert_eq!(in_range[0].episode_number, 1);
        let narrow = list_episodes_by_air_date_range(&p, &past("02"), &past("04"))
            .await
            .unwrap();
        assert_eq!(narrow.len(), 1);

        // Missing: aired, monitored (episode + series), no file.
        let missing = list_missing_episodes(&p, &timeutil::now()).await.unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].series_id, ms.id);
        assert_eq!(missing[0].episode_number, 1);

        // Cutoffs before/after the aired dates flip membership. Boundary is
        // inclusive: ep1 aired exactly at the 2026-01-01 cutoff counts.
        assert_eq!(
            list_missing_episodes(&p, "2026-01-01T00:00:00Z")
                .await
                .unwrap()
                .len(),
            1,
            "ep1 aired at the cutoff is missing"
        );
        assert!(list_missing_episodes(&p, "2025-12-31T23:59:59Z")
            .await
            .unwrap()
            .is_empty());
        let after = list_missing_episodes(&p, "2099-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(after.len(), 1, "unmonitored-series copy excluded");
        assert_eq!(after[0].series_id, ms.id);
    }

    #[tokio::test]
    async fn monitored_cascade_semantics() {
        let (_d, p) = pool().await;
        let (s, _) = upsert_series(&p, &series_input("Show", "/tv/show"))
            .await
            .unwrap();
        upsert_season(&p, &s.id, 1, true).await.unwrap();
        upsert_season(&p, &s.id, 2, true).await.unwrap();
        bulk_upsert_episodes(
            &p,
            &s.id,
            &[episode_input(1, 1, None), episode_input(2, 1, None)],
        )
        .await
        .unwrap();

        // Unmonitoring the series cascades to seasons and episodes.
        assert!(set_series_monitored(&p, &s.id, false).await.unwrap());
        assert!(!list_series(&p, Some(true))
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == s.id));
        assert!(list_seasons(&p, &s.id)
            .await
            .unwrap()
            .iter()
            .all(|x| !x.monitored));
        assert!(list_episodes_by_series(&p, &s.id)
            .await
            .unwrap()
            .iter()
            .all(|x| !x.monitored));
        assert!(!set_series_monitored(&p, "nope", true).await.unwrap());

        // Monitoring season 2 re-monitors the series and its episodes,
        // leaving season 1 unmonitored.
        assert!(set_season_monitored(&p, &s.id, 2, true).await.unwrap());
        let series = get_series(&p, &s.id).await.unwrap().unwrap();
        assert!(series.monitored, "monitoring a season monitors the series");
        let seasons = list_seasons(&p, &s.id).await.unwrap();
        assert!(!seasons[0].monitored && seasons[1].monitored);
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert!(!eps[0].monitored && eps[1].monitored);

        // Unmonitoring season 2 unmonitors its episodes but not the series.
        assert!(set_season_monitored(&p, &s.id, 2, false).await.unwrap());
        let series = get_series(&p, &s.id).await.unwrap().unwrap();
        assert!(series.monitored);
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert!(!eps[1].monitored && !eps[0].monitored);

        // Leaf-level episode flag, no cascade; unknown ids report false.
        assert!(set_episode_monitored(&p, &eps[0].id, true).await.unwrap());
        assert!(!set_episode_monitored(&p, "nope", true).await.unwrap());
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert!(eps[0].monitored && !eps[1].monitored);
        assert!(!set_season_monitored(&p, &s.id, 99, true).await.unwrap());
    }

    #[tokio::test]
    async fn episode_file_insert_get_delete_clears_has_file() {
        let (_d, p) = pool().await;
        let (s, _) = upsert_series(&p, &series_input("Show", "/tv/show"))
            .await
            .unwrap();
        bulk_upsert_episodes(
            &p,
            &s.id,
            &[episode_input(1, 1, None), episode_input(1, 2, None)],
        )
        .await
        .unwrap();

        let file = insert_episode_file(
            &p,
            &NewEpisodeFile {
                id: Some("file-1".to_string()),
                series_id: s.id.clone(),
                season_number: 1,
                episode_numbers: vec![1, 2],
                relative_path: "S01E01-E02.mkv".to_string(),
                size_bytes: 12345,
                quality: "webdl-1080p".to_string(),
                quality_score: 90,
                languages: vec!["eng".to_string(), "deu".to_string()],
                original_path: Some("/downloads/S01E01-E02.mkv".to_string()),
            },
        )
        .await
        .unwrap();
        assert_eq!(file.id, "file-1");
        assert_eq!(file.episode_numbers_json, "[1,2]");
        assert_eq!(file.languages_json.as_deref(), Some("[\"eng\",\"deu\"]"));

        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert!(eps
            .iter()
            .all(|e| e.has_file && e.episode_file_id.as_deref() == Some("file-1")));
        for e in &eps {
            let got = get_episode_file_by_episode(&p, &e.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got.id, "file-1");
            assert_eq!(got.relative_path, "S01E01-E02.mkv");
        }

        // An episode not covered by the file resolves to no file.
        bulk_upsert_episodes(&p, &s.id, &[episode_input(2, 1, None)])
            .await
            .unwrap();
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        let s2e1 = eps.iter().find(|e| e.season_number == 2).unwrap();
        assert!(get_episode_file_by_episode(&p, &s2e1.id)
            .await
            .unwrap()
            .is_none());

        // Delete clears flags on every covered episode.
        assert!(delete_episode_file(&p, "file-1").await.unwrap());
        assert!(
            !delete_episode_file(&p, "file-1").await.unwrap(),
            "second delete false"
        );
        let eps = list_episodes_by_series(&p, &s.id).await.unwrap();
        assert!(eps
            .iter()
            .all(|e| !e.has_file && e.episode_file_id.is_none()));
        assert!(get_episode_file_by_episode(&p, &eps[0].id)
            .await
            .unwrap()
            .is_none());
    }
}
