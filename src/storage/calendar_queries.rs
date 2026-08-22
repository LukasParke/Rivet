//! Calendar/wanted episode queries over the arr domain tables
//! (`series`/`episodes`/`episode_files` from migration 0011).
//!
//! All queries join episodes with their series so callers get flat rows ready
//! for the wire; serde renames mirror the *arr JSON key style (`seriesTitle`).

use sqlx::SqlitePool;

/// Episode joined with its series. Used by the calendar and wanted-missing
/// listings; [`CutoffEpisode`] adds file quality columns for cutoff checks.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct EpisodeWithSeries {
    pub id: String,
    #[serde(rename = "seriesId")]
    pub series_id: String,
    #[serde(rename = "seasonNumber")]
    pub season_number: i64,
    #[serde(rename = "episodeNumber")]
    pub episode_number: i64,
    #[serde(rename = "absoluteEpisodeNumber")]
    pub absolute_episode_number: Option<i64>,
    pub title: Option<String>,
    #[serde(rename = "airDateUtc")]
    pub air_date_utc: Option<String>,
    pub runtime: Option<i64>,
    pub overview: Option<String>,
    pub monitored: bool,
    #[serde(rename = "hasFile")]
    pub has_file: bool,
    #[serde(rename = "episodeFileId")]
    pub episode_file_id: Option<String>,
    #[serde(rename = "seriesTitle")]
    pub series_title: String,
    #[serde(rename = "seriesPath")]
    pub series_path: String,
    #[serde(rename = "qualityProfile")]
    pub quality_profile: String,
}

/// [`EpisodeWithSeries`] plus the quality of the attached episode file, for
/// cutoff-unmet listings.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CutoffEpisode {
    pub id: String,
    #[serde(rename = "seriesId")]
    pub series_id: String,
    #[serde(rename = "seasonNumber")]
    pub season_number: i64,
    #[serde(rename = "episodeNumber")]
    pub episode_number: i64,
    #[serde(rename = "absoluteEpisodeNumber")]
    pub absolute_episode_number: Option<i64>,
    pub title: Option<String>,
    #[serde(rename = "airDateUtc")]
    pub air_date_utc: Option<String>,
    pub runtime: Option<i64>,
    pub overview: Option<String>,
    pub monitored: bool,
    #[serde(rename = "hasFile")]
    pub has_file: bool,
    #[serde(rename = "episodeFileId")]
    pub episode_file_id: Option<String>,
    #[serde(rename = "seriesTitle")]
    pub series_title: String,
    #[serde(rename = "seriesPath")]
    pub series_path: String,
    #[serde(rename = "qualityProfile")]
    pub quality_profile: String,
    pub quality: Option<String>,
    #[serde(rename = "qualityScore")]
    pub quality_score: Option<i64>,
}

/// Shared projection for episode+series joins; callers append file columns.
const EPISODE_SERIES_SELECT: &str = "e.id, e.series_id, e.season_number, e.episode_number, \
     e.absolute_episode_number, e.title, e.air_date_utc, e.runtime, e.overview, \
     e.monitored, e.has_file, e.episode_file_id, \
     s.title AS series_title, s.path AS series_path, s.quality_profile";

/// Episodes airing within `[start, end]` (inclusive), ordered by air date.
/// `start`/`end` are ISO `YYYY-MM-DD` dates; the end date is compared as
/// `date(end, '+1 day')` so its whole day is covered despite RFC3339 air
/// timestamps. Episodes without an air date never appear.
pub async fn list_episodes_in_range(
    pool: &SqlitePool,
    start: &str,
    end: &str,
) -> Result<Vec<EpisodeWithSeries>, sqlx::Error> {
    sqlx::query_as::<_, EpisodeWithSeries>(&format!(
        "SELECT {EPISODE_SERIES_SELECT} \
         FROM episodes e JOIN series s ON s.id = e.series_id \
         WHERE e.air_date_utc IS NOT NULL \
           AND e.air_date_utc >= ? AND e.air_date_utc < date(?, '+1 day') \
         ORDER BY e.air_date_utc, e.season_number, e.episode_number"
    ))
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
}

/// Monitored episodes that already aired but have no file, newest air date
/// first. `offset` enables offset-based pagination alongside
/// [`count_missing_episodes`].
pub async fn missing_episodes(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> Result<Vec<EpisodeWithSeries>, sqlx::Error> {
    sqlx::query_as::<_, EpisodeWithSeries>(&format!(
        "SELECT {EPISODE_SERIES_SELECT} \
         FROM episodes e JOIN series s ON s.id = e.series_id \
         WHERE e.monitored = 1 AND e.has_file = 0 \
           AND e.air_date_utc IS NOT NULL AND e.air_date_utc <= ? \
         ORDER BY e.air_date_utc DESC, s.title, e.season_number, e.episode_number \
         LIMIT ? OFFSET ?"
    ))
    .bind(crate::timeutil::now())
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Total count matching [`missing_episodes`] (for pagination totals).
pub async fn count_missing_episodes(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM episodes e \
         WHERE e.monitored = 1 AND e.has_file = 0 \
           AND e.air_date_utc IS NOT NULL AND e.air_date_utc <= ?",
    )
    .bind(crate::timeutil::now())
    .fetch_one(pool)
    .await
}

/// Episodes airing from now through `days` days ahead, via the range query.
pub async fn upcoming(pool: &SqlitePool, days: i64) -> Result<Vec<EpisodeWithSeries>, sqlx::Error> {
    let today = chrono::Utc::now().date_naive();
    let end = (today + chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    list_episodes_in_range(pool, &crate::timeutil::now(), &end).await
}

/// Monitored, on-disk episodes whose file's `quality_score` is strictly below
/// the cutoff configured for the episode's series quality profile.
///
/// `cutoffs` maps profile name → minimum score (settings key
/// `profiles.cutoffs`). Profiles without an entry are never unmet, so an
/// empty map yields an empty result. Sorting matches
/// [`missing_episodes`] so both wanted lists read the same.
pub async fn cutoff_unmet_episodes(
    pool: &SqlitePool,
    cutoffs: &std::collections::HashMap<String, i64>,
) -> Result<Vec<CutoffEpisode>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CutoffEpisode>(&format!(
        "SELECT {EPISODE_SERIES_SELECT}, ef.quality, ef.quality_score \
         FROM episodes e \
         JOIN series s ON s.id = e.series_id \
         LEFT JOIN episode_files ef ON ef.id = e.episode_file_id \
         WHERE e.monitored = 1 AND e.has_file = 1"
    ))
    .fetch_all(pool)
    .await?;

    let mut unmet: Vec<CutoffEpisode> = rows
        .into_iter()
        .filter(|ep| {
            ep.quality_score
                .zip(cutoffs.get(&ep.quality_profile))
                .is_some_and(|(score, cutoff)| score < *cutoff)
        })
        .collect();
    unmet.sort_by(|a, b| {
        b.air_date_utc
            .cmp(&a.air_date_utc)
            .then_with(|| a.series_title.cmp(&b.series_title))
            .then_with(|| a.season_number.cmp(&b.season_number))
            .then_with(|| a.episode_number.cmp(&b.episode_number))
    });
    Ok(unmet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::migrations;

    async fn pool() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        migrations::apply(&pool).await.unwrap();
        (pool, dir)
    }

    /// Inserts a series.
    async fn seed_series(pool: &SqlitePool, id: &str, title: &str) {
        sqlx::query(
            "INSERT INTO series (id, title, path, quality_profile, added_at, updated_at) \
             VALUES (?, ?, ?, 'default', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(title)
        .bind(format!("/tv/{id}"))
        .execute(pool)
        .await
        .unwrap();
    }

    /// Inserts a season-1 episode of `series_id`.
    async fn seed_episode(
        pool: &SqlitePool,
        id: &str,
        series_id: &str,
        ep_no: i64,
        air: Option<&str>,
        monitored: bool,
        has_file: bool,
    ) {
        sqlx::query(
            "INSERT INTO episodes (id, series_id, season_number, episode_number, title, \
             air_date_utc, monitored, has_file) VALUES (?, ?, 1, ?, 'Pilot', ?, ?, ?)",
        )
        .bind(id)
        .bind(series_id)
        .bind(ep_no)
        .bind(air)
        .bind(monitored)
        .bind(has_file)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Attaches an on-disk episode file with `score` to episode `id`.
    async fn seed_file(pool: &SqlitePool, id: &str, score: i64) {
        sqlx::query(
            "INSERT INTO episode_files (id, series_id, season_number, episode_numbers_json, \
             relative_path, quality, quality_score, date_added) \
             VALUES (?, 's1', 1, '[1]', ?, 'WEBDL-1080p', ?, '2026-08-20T06:00:00Z')",
        )
        .bind(format!("f-{id}"))
        .bind(format!("/tv/s1/{id}.mkv"))
        .bind(score)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE episodes SET episode_file_id = ? WHERE id = ?")
            .bind(format!("f-{id}"))
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn range_filters_by_air_date_and_includes_end_day() {
        let (pool, _dir) = pool().await;
        seed_series(&pool, "s1", "Show").await;
        seed_episode(
            &pool,
            "e1",
            "s1",
            1,
            Some("2026-08-21T05:00:00Z"),
            true,
            false,
        )
        .await;
        // Same day as the range end: must be included despite the timestamp.
        seed_episode(
            &pool,
            "e2",
            "s1",
            2,
            Some("2026-08-28T23:30:00Z"),
            true,
            false,
        )
        .await;
        // One day past the end: excluded.
        seed_episode(
            &pool,
            "e3",
            "s1",
            3,
            Some("2026-08-29T00:30:00Z"),
            true,
            false,
        )
        .await;
        // No air date: excluded.
        seed_episode(&pool, "e4", "s1", 4, None, true, false).await;

        let rows = list_episodes_in_range(&pool, "2026-08-21", "2026-08-28")
            .await
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["e1", "e2"]);
        assert_eq!(rows[0].series_title, "Show");
        assert_eq!(rows[0].series_path, "/tv/s1");
        assert_eq!(rows[0].quality_profile, "default");
    }

    #[tokio::test]
    async fn missing_requires_monitored_aired_without_file() {
        let (pool, _dir) = pool().await;
        seed_series(&pool, "s1", "Show").await;
        // Qualifies: monitored, aired, no file.
        seed_episode(
            &pool,
            "e1",
            "s1",
            1,
            Some("2026-08-20T05:00:00Z"),
            true,
            false,
        )
        .await;
        // Unmonitored: excluded.
        seed_episode(
            &pool,
            "e2",
            "s1",
            2,
            Some("2026-08-20T05:00:00Z"),
            false,
            false,
        )
        .await;
        // Has file: excluded.
        seed_episode(
            &pool,
            "e3",
            "s1",
            3,
            Some("2026-08-20T05:00:00Z"),
            true,
            true,
        )
        .await;
        // Not yet aired: excluded.
        seed_episode(
            &pool,
            "e4",
            "s1",
            4,
            Some("2999-01-01T00:00:00Z"),
            true,
            false,
        )
        .await;

        assert_eq!(count_missing_episodes(&pool).await.unwrap(), 1);
        let rows = missing_episodes(&pool, 50, 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "e1");
    }

    #[tokio::test]
    async fn upcoming_returns_only_future_window() {
        let (pool, _dir) = pool().await;
        seed_series(&pool, "s1", "Show").await;
        let today = chrono::Utc::now().date_naive();
        let d = |days: i64| {
            Some(
                (today + chrono::Duration::days(days))
                    .and_hms_opt(5, 0, 0)
                    .unwrap()
                    .and_utc()
                    .to_rfc3339(),
            )
        };
        seed_episode(&pool, "past", "s1", 1, d(-30).as_deref(), true, false).await;
        seed_episode(&pool, "soon", "s1", 2, d(10).as_deref(), true, false).await;
        seed_episode(&pool, "far", "s1", 3, d(40).as_deref(), true, false).await;
        let rows = upcoming(&pool, 30).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["soon"]);
    }

    #[tokio::test]
    async fn cutoff_unmet_compares_file_score_to_profile_cutoff() {
        let (pool, _dir) = pool().await;
        seed_series(&pool, "s1", "Show").await;
        for (id, ep_no, score) in [("low", 1, 60), ("met", 2, 100)] {
            seed_episode(
                &pool,
                id,
                "s1",
                ep_no,
                Some("2026-08-20T05:00:00Z"),
                true,
                true,
            )
            .await;
            seed_file(&pool, id, score).await;
        }

        let mut cutoffs = std::collections::HashMap::new();
        cutoffs.insert("default".to_string(), 100_i64);
        let rows = cutoff_unmet_episodes(&pool, &cutoffs).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "low");
        assert_eq!(rows[0].quality.as_deref(), Some("WEBDL-1080p"));
        assert_eq!(rows[0].quality_score, Some(60));

        // No configured cutoff for the profile -> nothing is unmet.
        let rows = cutoff_unmet_episodes(&pool, &std::collections::HashMap::new())
            .await
            .unwrap();
        assert!(rows.is_empty());
    }
}
