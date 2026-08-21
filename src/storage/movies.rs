//! Movie storage for the 0011 schema.
//!
//! Upserts match existing rows by tmdb id or library path (first hit wins),
//! mirroring [`crate::storage::series`] semantics.

use sqlx::SqlitePool;

use crate::timeutil;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Movie {
    pub id: String,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub title: String,
    pub sort_title: Option<String>,
    pub year: Option<i64>,
    pub status: String,
    pub overview: Option<String>,
    pub studio: Option<String>,
    pub collection: Option<String>,
    pub genres_json: Option<String>,
    pub path: String,
    pub quality_profile: String,
    pub minimum_availability: String,
    pub monitored: bool,
    pub tags_json: Option<String>,
    pub added_at: String,
    pub updated_at: String,
}

/// Input for [`upsert_movie`]. `id`, when given, is reused on insert;
/// otherwise a fresh uuid is generated.
#[derive(Debug, Clone)]
pub struct MovieUpsert {
    pub id: Option<String>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub title: String,
    pub sort_title: Option<String>,
    pub year: Option<i64>,
    pub status: String,
    pub overview: Option<String>,
    pub studio: Option<String>,
    pub collection: Option<String>,
    /// Serialized as the `genres_json` JSON array.
    pub genres: Vec<String>,
    pub path: String,
    pub quality_profile: String,
    pub minimum_availability: String,
    pub monitored: bool,
    /// Serialized as the `tags_json` JSON array.
    pub tags: Vec<String>,
}

fn json_array(v: &[String]) -> Option<String> {
    if v.is_empty() {
        None
    } else {
        Some(serde_json::to_string(v).expect("Vec<String> always serializes"))
    }
}

/// Finds an existing movie row by tmdb id or path (first hit wins).
async fn find_existing(
    pool: &SqlitePool,
    input: &MovieUpsert,
) -> Result<Option<Movie>, sqlx::Error> {
    sqlx::query_as::<_, Movie>(
        "SELECT * FROM movies WHERE \
         (tmdb_id IS NOT NULL AND tmdb_id = ?) OR path = ? LIMIT 1",
    )
    .bind(input.tmdb_id)
    .bind(input.path.trim())
    .fetch_optional(pool)
    .await
}

/// Upserts a movie matched by tmdb id or path. Returns the stored row and
/// whether it was newly created. On update, `id` and `added_at` are kept.
pub async fn upsert_movie(
    pool: &SqlitePool,
    input: &MovieUpsert,
) -> Result<(Movie, bool), sqlx::Error> {
    if let Some(existing) = find_existing(pool, input).await? {
        let now = timeutil::now();
        sqlx::query(
            "UPDATE movies SET tmdb_id = ?, imdb_id = ?, title = ?, sort_title = ?, year = ?, \
             status = ?, overview = ?, studio = ?, collection = ?, genres_json = ?, path = ?, \
             quality_profile = ?, minimum_availability = ?, monitored = ?, tags_json = ?, \
             updated_at = ? WHERE id = ?",
        )
        .bind(input.tmdb_id)
        .bind(input.imdb_id.as_deref())
        .bind(input.title.trim())
        .bind(input.sort_title.as_deref())
        .bind(input.year)
        .bind(&input.status)
        .bind(input.overview.as_deref())
        .bind(input.studio.as_deref())
        .bind(input.collection.as_deref())
        .bind(json_array(&input.genres))
        .bind(input.path.trim())
        .bind(&input.quality_profile)
        .bind(&input.minimum_availability)
        .bind(input.monitored)
        .bind(json_array(&input.tags))
        .bind(&now)
        .bind(&existing.id)
        .execute(pool)
        .await?;
        let updated = get_movie(pool, &existing.id).await?.expect("row just updated");
        Ok((updated, false))
    } else {
        let id = input
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = timeutil::now();
        sqlx::query(
            "INSERT INTO movies (id, tmdb_id, imdb_id, title, sort_title, year, status, \
             overview, studio, collection, genres_json, path, quality_profile, \
             minimum_availability, monitored, tags_json, added_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.tmdb_id)
        .bind(input.imdb_id.as_deref())
        .bind(input.title.trim())
        .bind(input.sort_title.as_deref())
        .bind(input.year)
        .bind(&input.status)
        .bind(input.overview.as_deref())
        .bind(input.studio.as_deref())
        .bind(input.collection.as_deref())
        .bind(json_array(&input.genres))
        .bind(input.path.trim())
        .bind(&input.quality_profile)
        .bind(&input.minimum_availability)
        .bind(input.monitored)
        .bind(json_array(&input.tags))
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
        let created = get_movie(pool, &id).await?.expect("row just inserted");
        Ok((created, true))
    }
}

pub async fn get_movie(pool: &SqlitePool, id: &str) -> Result<Option<Movie>, sqlx::Error> {
    sqlx::query_as::<_, Movie>("SELECT * FROM movies WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Lists movies ordered by `COALESCE(sort_title, title)` (case-insensitive).
/// `monitored` filters when given.
pub async fn list_movies(
    pool: &SqlitePool,
    monitored: Option<bool>,
) -> Result<Vec<Movie>, sqlx::Error> {
    sqlx::query_as::<_, Movie>(
        "SELECT * FROM movies WHERE (? IS NULL OR monitored = ?) \
         ORDER BY COALESCE(sort_title, title) COLLATE NOCASE",
    )
    .bind(monitored)
    .bind(monitored.unwrap_or_default())
    .fetch_all(pool)
    .await
}

pub async fn delete_movie(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM movies WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Sets the monitored flag on a single movie. Returns false when the movie
/// does not exist.
pub async fn set_movie_monitored(
    pool: &SqlitePool,
    id: &str,
    monitored: bool,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE movies SET monitored = ?, updated_at = ? WHERE id = ?")
        .bind(monitored)
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("m.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    fn movie_input(title: &str, path: &str) -> MovieUpsert {
        MovieUpsert {
            id: None,
            tmdb_id: None,
            imdb_id: None,
            title: title.to_string(),
            sort_title: None,
            year: Some(2026),
            status: "released".to_string(),
            overview: None,
            studio: None,
            collection: None,
            genres: vec!["sci-fi".to_string()],
            path: path.to_string(),
            quality_profile: "default".to_string(),
            minimum_availability: "released".to_string(),
            monitored: true,
            tags: vec![],
        }
    }

    #[tokio::test]
    async fn upsert_matches_by_tmdb_and_path_and_updates_in_place() {
        let (_d, p) = pool().await;
        let mut first = movie_input("Movie", "/movies/m");
        first.tmdb_id = Some(42);
        let (m1, created) = upsert_movie(&p, &first).await.unwrap();
        assert!(created);
        assert_eq!(m1.genres_json.as_deref(), Some("[\"sci-fi\"]"));

        // Same tmdb id -> update in place (id + added_at kept).
        let mut by_tmdb = movie_input("Movie II", "/movies/moved");
        by_tmdb.tmdb_id = Some(42);
        let (m2, created) = upsert_movie(&p, &by_tmdb).await.unwrap();
        assert!(!created);
        assert_eq!(m2.id, m1.id);
        assert_eq!(m2.title, "Movie II");
        assert_eq!(m2.path, "/movies/moved");
        assert_eq!(m2.added_at, m1.added_at);

        // Path alone matches when no tmdb id is given.
        let by_path = movie_input("Movie III", "/movies/moved");
        let (m3, created) = upsert_movie(&p, &by_path).await.unwrap();
        assert!(!created);
        assert_eq!(m3.id, m1.id);

        assert_eq!(list_movies(&p, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn movie_path_is_unique() {
        let (_d, p) = pool().await;
        upsert_movie(&p, &movie_input("A", "/movies/a")).await.unwrap();

        // A direct INSERT with a duplicate path must violate UNIQUE(path).
        let err = sqlx::query(
            "INSERT INTO movies (id, title, path, status, added_at, updated_at) \
             VALUES ('x', 'B', '/movies/a', 'released', '2026', '2026')",
        )
        .execute(&p)
        .await;
        assert!(err.is_err(), "unique(path) must reject duplicates");

        assert_eq!(list_movies(&p, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_orders_by_sort_title_and_filters_monitored() {
        let (_d, p) = pool().await;
        let mut z = movie_input("Zebra", "/movies/z");
        z.sort_title = Some("aaa".to_string());
        let mut a = movie_input("Apple", "/movies/a");
        a.monitored = false;
        upsert_movie(&p, &z).await.unwrap();
        upsert_movie(&p, &a).await.unwrap();
        upsert_movie(&p, &movie_input("Mango", "/movies/m")).await.unwrap();

        let all = list_movies(&p, None).await.unwrap();
        let titles: Vec<&str> = all.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, vec!["Zebra", "Apple", "Mango"], "sort_title wins");

        assert_eq!(list_movies(&p, Some(true)).await.unwrap().len(), 2);
        let unmonitored = list_movies(&p, Some(false)).await.unwrap();
        assert_eq!(unmonitored.len(), 1);
        assert_eq!(unmonitored[0].title, "Apple");
    }

    #[tokio::test]
    async fn get_delete_and_set_monitored_roundtrip() {
        let (_d, p) = pool().await;
        let (m, _) = upsert_movie(&p, &movie_input("Movie", "/movies/m")).await.unwrap();

        assert!(get_movie(&p, &m.id).await.unwrap().is_some());
        assert!(get_movie(&p, "nope").await.unwrap().is_none());

        assert!(set_movie_monitored(&p, &m.id, false).await.unwrap());
        assert!(!get_movie(&p, &m.id).await.unwrap().unwrap().monitored);
        assert!(list_movies(&p, Some(true)).await.unwrap().is_empty());
        assert!(!set_movie_monitored(&p, "nope", true).await.unwrap());

        assert!(delete_movie(&p, &m.id).await.unwrap());
        assert!(!delete_movie(&p, &m.id).await.unwrap(), "second delete reports false");
        assert!(get_movie(&p, &m.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn series_and_movie_paths_are_independent_namespaces() {
        let (_d, p) = pool().await;
        // The two tables do not share unique constraints.
        crate::storage::series::upsert_series(
            &p,
            &crate::storage::series::SeriesUpsert {
                id: None,
                tvdb_id: None,
                tmdb_id: None,
                imdb_id: None,
                title: "Show".to_string(),
                sort_title: None,
                status: "continuing".to_string(),
                overview: None,
                network: None,
                first_aired: None,
                genres: vec![],
                path: "/library/thing".to_string(),
                quality_profile: "default".to_string(),
                monitored: true,
                season_folder: true,
                tags: vec![],
            },
        )
        .await
        .unwrap();
        let (m, created) = upsert_movie(&p, &movie_input("Movie", "/library/thing")).await.unwrap();
        assert!(created);
        assert_eq!(list_movies(&p, None).await.unwrap().len(), 1);
        drop(m);
    }
}
