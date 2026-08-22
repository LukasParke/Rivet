//! Library management endpoints (*arr parity): series and movie CRUD over the
//! 0011 domain tables, monitor toggles, and quality-profile configuration.
//!
//! Roles: reads and monitor toggles are operator+; create/delete and
//! quality-profile writes are admin-only. Destructive deletes are audited.
//!
//! Quality profiles are settings-backed: the profile list lives in the
//! `setup.policies` settings key (its `qualityProfiles` array, written by the
//! setup wizard) and the per-profile cutoff score map lives in
//! `profiles.cutoffs` — the same key `/wanted/cutoff_unmet` consumes. GET
//! merges the two into one view; PUT rewrites both keys inside a single
//! transaction.

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{ApiResult, AppError};
use crate::httpapi::middleware::auth::{Admin, Operator};
use crate::state::AppState;
use crate::storage::movies::{self, Movie, MovieUpsert};
use crate::storage::series::{self, Series, SeriesUpsert};

/// Settings key holding the setup-wizard policy document (qualityProfiles
/// array among other keys).
const SETUP_POLICIES: &str = "setup.policies";
/// Settings key mapping quality-profile name → cutoff score, consumed by
/// `/wanted/cutoff_unmet`.
const PROFILES_CUTOFFS: &str = "profiles.cutoffs";

/// Profile assigned to newly added series/movies when the request omits one.
const DEFAULT_PROFILE: &str = "default";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/series", get(list_series).post(create_series))
        .route("/series/{id}", get(get_series_detail).delete(delete_series))
        .route("/series/{id}/monitor", put(set_series_monitor))
        .route("/movies", get(list_movies).post(create_movie))
        .route("/movies/{id}", delete(delete_movie))
        .route("/movies/{id}/monitor", put(set_movie_monitor))
        .route(
            "/qualityprofiles",
            get(quality_profiles).put(save_quality_profiles),
        )
}

// ---------------------------------------------------------------------------
// Series
// ---------------------------------------------------------------------------

async fn list_series(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<Series>>> {
    let monitored = params.get("monitored").and_then(|v| v.parse::<bool>().ok());
    Ok(Json(series::list_series(&state.db, monitored).await?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSeries {
    title: String,
    path: String,
    tvdb_id: Option<i64>,
    tmdb_id: Option<i64>,
    quality_profile: Option<String>,
    monitored: Option<bool>,
}

async fn create_series(
    Admin(user): Admin,
    State(state): State<AppState>,
    Json(req): Json<CreateSeries>,
) -> ApiResult<Json<Series>> {
    let title = req.title.trim();
    let path = req.path.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title is required".into()));
    }
    if path.is_empty() {
        return Err(AppError::BadRequest("path is required".into()));
    }
    let input = SeriesUpsert {
        id: None,
        tvdb_id: req.tvdb_id,
        tmdb_id: req.tmdb_id,
        imdb_id: None,
        title: title.to_string(),
        sort_title: None,
        status: "continuing".into(),
        overview: None,
        network: None,
        first_aired: None,
        genres: Vec::new(),
        path: path.to_string(),
        quality_profile: req
            .quality_profile
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_PROFILE.into()),
        monitored: req.monitored.unwrap_or(true),
        season_folder: true,
        tags: Vec::new(),
    };
    let (stored, created) = series::upsert_series(&state.db, &input).await?;
    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id),
            action: if created {
                "library.series_created".into()
            } else {
                "library.series_updated".into()
            },
            target_type: Some("series".into()),
            target_id: Some(stored.id.clone()),
            ..Default::default()
        },
    )
    .await?;
    Ok(Json(stored))
}

/// Wire shape of `GET /series/{id}`: the series plus its seasons and
/// episode/file counts.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesDetail {
    #[serde(flatten)]
    series: Series,
    seasons: Vec<series::Season>,
    episode_count: i64,
    episode_file_count: i64,
}

async fn get_series_detail(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SeriesDetail>> {
    let id = id.trim();
    let s = series::get_series(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let seasons = series::list_seasons(&state.db, id).await?;
    let (episode_count, episode_file_count) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT COUNT(*), COALESCE(SUM(has_file), 0) FROM episodes WHERE series_id = ?",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(SeriesDetail {
        series: s,
        seasons,
        episode_count,
        episode_file_count,
    }))
}

async fn delete_series(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = id.trim();
    // Cascade removes seasons, episodes and episode files (FK ON DELETE
    // CASCADE in the 0011 schema).
    let deleted = series::delete_series(&state.db, id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }
    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id.clone()),
            action: "library.series_deleted".into(),
            target_type: Some("series".into()),
            target_id: Some(id.to_string()),
            ..Default::default()
        },
    )
    .await?;
    state.hub.publish(
        "library.series_deleted",
        json!({ "seriesId": id, "actorUserId": user.user.id }),
    );
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMonitored {
    monitored: bool,
}

async fn set_series_monitor(
    Operator(user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetMonitored>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = id.trim();
    // Cascades the flag to seasons and episodes (Sonarr semantics).
    let ok = series::set_series_monitored(&state.db, id, req.monitored).await?;
    if !ok {
        return Err(AppError::NotFound);
    }
    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id),
            action: "library.series_monitored".into(),
            target_type: Some("series".into()),
            target_id: Some(id.to_string()),
            details_json: Some(json!({ "monitored": req.monitored }).to_string()),
            ..Default::default()
        },
    )
    .await?;
    Ok(Json(json!({ "id": id, "monitored": req.monitored })))
}

// ---------------------------------------------------------------------------
// Movies
// ---------------------------------------------------------------------------

async fn list_movies(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<Movie>>> {
    let monitored = params.get("monitored").and_then(|v| v.parse::<bool>().ok());
    Ok(Json(movies::list_movies(&state.db, monitored).await?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateMovie {
    title: String,
    path: String,
    tmdb_id: Option<i64>,
    quality_profile: Option<String>,
    monitored: Option<bool>,
}

async fn create_movie(
    Admin(user): Admin,
    State(state): State<AppState>,
    Json(req): Json<CreateMovie>,
) -> ApiResult<Json<Movie>> {
    let title = req.title.trim();
    let path = req.path.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title is required".into()));
    }
    if path.is_empty() {
        return Err(AppError::BadRequest("path is required".into()));
    }
    let input = MovieUpsert {
        id: None,
        tmdb_id: req.tmdb_id,
        imdb_id: None,
        title: title.to_string(),
        sort_title: None,
        year: None,
        status: "released".into(),
        overview: None,
        studio: None,
        collection: None,
        genres: Vec::new(),
        path: path.to_string(),
        quality_profile: req
            .quality_profile
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_PROFILE.into()),
        minimum_availability: "released".into(),
        monitored: req.monitored.unwrap_or(true),
        tags: Vec::new(),
    };
    let (stored, created) = movies::upsert_movie(&state.db, &input).await?;
    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id),
            action: if created {
                "library.movie_created".into()
            } else {
                "library.movie_updated".into()
            },
            target_type: Some("movie".into()),
            target_id: Some(stored.id.clone()),
            ..Default::default()
        },
    )
    .await?;
    Ok(Json(stored))
}

async fn delete_movie(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = id.trim();
    let deleted = movies::delete_movie(&state.db, id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }
    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id.clone()),
            action: "library.movie_deleted".into(),
            target_type: Some("movie".into()),
            target_id: Some(id.to_string()),
            ..Default::default()
        },
    )
    .await?;
    state.hub.publish(
        "library.movie_deleted",
        json!({ "movieId": id, "actorUserId": user.user.id }),
    );
    Ok(Json(json!({ "deleted": id })))
}

async fn set_movie_monitor(
    Operator(user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetMonitored>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = id.trim();
    let ok = movies::set_movie_monitored(&state.db, id, req.monitored).await?;
    if !ok {
        return Err(AppError::NotFound);
    }
    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id),
            action: "library.movie_monitored".into(),
            target_type: Some("movie".into()),
            target_id: Some(id.to_string()),
            details_json: Some(json!({ "monitored": req.monitored }).to_string()),
            ..Default::default()
        },
    )
    .await?;
    Ok(Json(json!({ "id": id, "monitored": req.monitored })))
}

// ---------------------------------------------------------------------------
// Quality profiles
// ---------------------------------------------------------------------------

/// Merged view of one quality profile: the setup-wizard definition plus its
/// configured cutoff score (null when no cutoff is set).
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct QualityProfileView {
    name: String,
    allowed_qualities: Vec<String>,
    cutoff: Option<i64>,
}

/// Reads and parses the `setup.policies` settings document; missing or
/// malformed settings degrade to an empty object.
async fn load_policies(db: &sqlx::SqlitePool) -> Result<serde_json::Value, sqlx::Error> {
    match crate::storage::settings::get(db, SETUP_POLICIES).await? {
        Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null)),
        None => Ok(serde_json::Value::Null),
    }
}

/// Reads the `profiles.cutoffs` map; absent or malformed settings yield an
/// empty map (same tolerance as `/wanted/cutoff_unmet`).
async fn load_cutoffs(db: &sqlx::SqlitePool) -> Result<HashMap<String, i64>, sqlx::Error> {
    let Some(raw) = crate::storage::settings::get(db, PROFILES_CUTOFFS).await? else {
        return Ok(HashMap::new());
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(HashMap::new());
    };
    let Some(map) = val.as_object() else {
        return Ok(HashMap::new());
    };
    Ok(map
        .iter()
        .filter_map(|(k, v)| v.as_i64().map(|score| (k.clone(), score)))
        .collect())
}

async fn quality_profiles(
    Operator(_user): Operator,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<QualityProfileView>>> {
    let policies = load_policies(&state.db).await?;
    let cutoffs = load_cutoffs(&state.db).await?;
    let mut out = Vec::new();
    if let Some(profiles) = policies.get("qualityProfiles").and_then(|p| p.as_array()) {
        for p in profiles {
            let Some(name) = p.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let allowed = p
                .get("allowedQualities")
                .and_then(|q| q.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|q| q.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            out.push(QualityProfileView {
                name: name.to_string(),
                allowed_qualities: allowed,
                cutoff: cutoffs.get(name).copied(),
            });
        }
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QualityProfileInput {
    name: String,
    allowed_qualities: Vec<String>,
    cutoff: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveQualityProfiles {
    quality_profiles: Vec<QualityProfileInput>,
}

async fn save_quality_profiles(
    Admin(user): Admin,
    State(state): State<AppState>,
    Json(req): Json<SaveQualityProfiles>,
) -> ApiResult<Json<serde_json::Value>> {
    // Validate in-band before touching settings.
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let mut profiles = req.quality_profiles;
    for p in profiles.iter_mut() {
        p.name = p.name.trim().to_string();
        if p.name.is_empty() {
            errors.push("qualityProfiles.name is required".into());
            continue;
        }
        if !seen.insert(p.name.to_ascii_lowercase()) {
            errors.push("qualityProfiles.name must be unique".into());
        }
        if p.allowed_qualities.is_empty() {
            errors.push("qualityProfiles.allowedQualities must be non-empty".into());
        }
        if let Some(c) = p.cutoff {
            if c < 0 {
                errors.push(format!(
                    "qualityProfiles.cutoff must be >= 0 for {}",
                    p.name
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(AppError::BadRequest(json!(errors).to_string()));
    }

    // Merge into the existing setup.policies document: keep every other key
    // (language, bannedTags, ...) and preserve extra per-profile fields such
    // as `upgradeAllowed` for profiles that already exist.
    let mut policies = load_policies(&state.db).await?;
    if !policies.is_object() {
        policies = json!({});
    }
    let existing: Vec<serde_json::Value> = policies
        .get("qualityProfiles")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut merged = Vec::with_capacity(profiles.len());
    for p in &profiles {
        let prior = existing
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(p.name.as_str()));
        let mut entry = match prior {
            Some(prior) => prior.clone(),
            None => json!({ "upgradeAllowed": true }),
        };
        let obj = entry.as_object_mut().unwrap();
        obj.insert("name".into(), json!(p.name));
        obj.insert("allowedQualities".into(), json!(p.allowed_qualities));
        merged.push(entry);
    }
    policies
        .as_object_mut()
        .unwrap()
        .insert("qualityProfiles".into(), serde_json::Value::Array(merged));

    let cutoffs: BTreeMap<String, i64> = profiles
        .iter()
        .filter_map(|p| p.cutoff.map(|c| (p.name.clone(), c)))
        .collect();

    // Both keys in one transaction so readers never observe a half-applied
    // profile/cutoff pair.
    let policies_json = policies.to_string();
    let cutoffs_json = serde_json::to_string(&cutoffs).unwrap();
    let mut tx = state.db.begin().await?;
    for (key, value) in [
        (SETUP_POLICIES, policies_json.as_str()),
        (PROFILES_CUTOFFS, cutoffs_json.as_str()),
    ] {
        sqlx::query(
            "INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, \
             updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(crate::timeutil::now())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    crate::storage::audit::insert(
        &state.db,
        crate::storage::audit::AuditEvent {
            actor_user_id: Some(user.user.id.clone()),
            action: "library.quality_profiles_saved".into(),
            target_type: Some("settings".into()),
            target_id: Some(SETUP_POLICIES.into()),
            ..Default::default()
        },
    )
    .await?;
    state.hub.publish(
        "library.quality_profiles_saved",
        json!({ "count": profiles.len(), "actorUserId": user.user.id }),
    );
    Ok(Json(json!({ "saved": profiles.len() })))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::security::secrets::SecretBox;
    use crate::storage::migrations;
    use crate::storage::sessions::Session;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware;
    use tower::ServiceExt;

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        migrations::apply(&pool).await.unwrap();
        let state = AppState {
            db: pool,
            secret_box: Arc::new(SecretBox::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: true,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: std::time::Duration::from_secs(3600),
                allow_internal_urls: true,
            }),
        };
        (state, dir)
    }

    /// Same global middleware stack the parent attaches under /api/v1.
    fn full_router(state: &AppState) -> axum::Router {
        router()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state.clone())
    }

    async fn seed_session(state: &AppState, email: &str, role: &str) -> Session {
        let u = crate::storage::users::create(&state.db, email, "test-hash", role)
            .await
            .unwrap();
        crate::storage::sessions::create(&state.db, &u.id, chrono::Duration::hours(1), None, None)
            .await
            .unwrap()
    }

    /// Builds an authed request, attaching the session cookie and CSRF token
    /// (the double-submit check applies to every unsafe method).
    fn req(method: &str, uri: &str, session: Option<&Session>, body: Option<String>) -> Request {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(s) = session {
            b = b
                .header("cookie", format!("rivet_session={}", s.id))
                .header("x-csrf-token", &s.csrf_token);
        }
        let b = if body.is_some() {
            b.header("content-type", "application/json")
        } else {
            b
        };
        b.body(Body::from(body.unwrap_or_default())).unwrap()
    }

    async fn body_json(res: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Seeds one series (with a season, an episode and an on-disk file) plus
    /// one movie, using the real storage upserts.
    async fn seed_library(state: &AppState) -> (String, String) {
        let (s, _) = series::upsert_series(
            &state.db,
            &SeriesUpsert {
                id: None,
                tvdb_id: Some(111),
                tmdb_id: Some(222),
                imdb_id: None,
                title: "Show".into(),
                sort_title: None,
                status: "continuing".into(),
                overview: None,
                network: None,
                first_aired: None,
                genres: Vec::new(),
                path: "/tv/Show".into(),
                quality_profile: "default".into(),
                monitored: true,
                season_folder: true,
                tags: Vec::new(),
            },
        )
        .await
        .unwrap();
        series::upsert_season(&state.db, &s.id, 1, true)
            .await
            .unwrap();
        series::bulk_upsert_episodes(
            &state.db,
            &s.id,
            &[series::EpisodeUpsert {
                season_number: 1,
                episode_number: 1,
                absolute_episode_number: None,
                title: Some("Pilot".into()),
                air_date_utc: Some("2026-08-01T00:00:00Z".into()),
                runtime: None,
                overview: None,
                monitored: true,
            }],
        )
        .await
        .unwrap();
        series::insert_episode_file(
            &state.db,
            &series::NewEpisodeFile {
                id: None,
                series_id: s.id.clone(),
                season_number: 1,
                episode_numbers: vec![1],
                relative_path: "Season 1/Show - S01E01.mkv".into(),
                size_bytes: 1024,
                quality: "WEBDL-1080p".into(),
                quality_score: 100,
                languages: Vec::new(),
                original_path: None,
            },
        )
        .await
        .unwrap();
        let eps = series::list_episodes_by_series(&state.db, &s.id)
            .await
            .unwrap();
        assert!(eps[0].has_file, "file insert must mark has_file");

        let (m, _) = movies::upsert_movie(
            &state.db,
            &MovieUpsert {
                id: None,
                tmdb_id: Some(333),
                imdb_id: None,
                title: "Film".into(),
                sort_title: None,
                year: Some(2025),
                status: "released".into(),
                overview: None,
                studio: None,
                collection: None,
                genres: Vec::new(),
                path: "/movies/Film".into(),
                quality_profile: "default".into(),
                minimum_availability: "released".into(),
                monitored: true,
                tags: Vec::new(),
            },
        )
        .await
        .unwrap();
        (s.id, m.id)
    }

    // -- Series --------------------------------------------------------------

    #[tokio::test]
    async fn series_list_and_detail() {
        let (state, _dir) = test_state().await;
        let (sid, _mid) = seed_library(&state).await;
        let op = seed_session(&state, "op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(req("GET", "/api/v1/series", Some(&op), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Show");
        assert_eq!(items[0]["tvdbId"], 111);
        assert_eq!(items[0]["monitored"], true);

        let res = full_router(&state)
            .oneshot(req(
                "GET",
                &format!("/api/v1/series/{sid}"),
                Some(&op),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["id"], sid.as_str());
        assert_eq!(v["episodeCount"], 1);
        assert_eq!(v["episodeFileCount"], 1);
        assert_eq!(v["seasons"].as_array().unwrap().len(), 1);
        assert_eq!(v["seasons"][0]["number"], 1);

        // Unknown id -> 404 not_found.
        let res = full_router(&state)
            .oneshot(req("GET", "/api/v1/series/nope", Some(&op), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(res).await["error"], "not_found");

        // monitored=false filter hides the monitored series.
        let res = full_router(&state)
            .oneshot(req(
                "GET",
                "/api/v1/series?monitored=false",
                Some(&op),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(body_json(res).await.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn series_create_upserts_and_defaults() {
        let (state, _dir) = test_state().await;
        let admin = seed_session(&state, "admin@x.test", "admin").await;
        let body = json!({
            "title": "New Show",
            "path": "/tv/New Show",
            "tvdbId": 42,
            "monitored": false,
        })
        .to_string();

        let res = full_router(&state)
            .oneshot(req("POST", "/api/v1/series", Some(&admin), Some(body)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["title"], "New Show");
        assert_eq!(v["monitored"], false);
        assert_eq!(v["qualityProfile"], "default", "omitted profile defaults");
        let id = v["id"].as_str().unwrap().to_string();

        // Re-posting with the same tvdbId updates in place (upsert match).
        let body2 = json!({
            "title": "New Show Renamed",
            "path": "/tv/New Show",
            "tvdbId": 42,
            "qualityProfile": "ultra",
        })
        .to_string();
        let res = full_router(&state)
            .oneshot(req("POST", "/api/v1/series", Some(&admin), Some(body2)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["id"], id.as_str(), "upsert keeps the row");
        assert_eq!(v["title"], "New Show Renamed");
        assert_eq!(v["qualityProfile"], "ultra");

        let n = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM series")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // Audit trail records create + update.
        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "library.series_created"));
        assert!(actions.iter().any(|a| a.action == "library.series_updated"));

        // Empty title / path -> 400.
        for bad in [
            json!({"title": "  ", "path": "/tv/x"}),
            json!({"title": "x", "path": ""}),
        ] {
            let res = full_router(&state)
                .oneshot(req(
                    "POST",
                    "/api/v1/series",
                    Some(&admin),
                    Some(bad.to_string()),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let err = body_json(res).await["error"].as_str().unwrap().to_string();
            assert!(
                err == "title is required" || err == "path is required",
                "unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn series_delete_cascades_and_audits() {
        let (state, _dir) = test_state().await;
        let (sid, _) = seed_library(&state).await;
        let admin = seed_session(&state, "admin@x.test", "admin").await;

        let res = full_router(&state)
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/series/{sid}"),
                Some(&admin),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Seasons, episodes and episode files died with the series.
        for table in ["series", "seasons", "episodes", "episode_files"] {
            let n = sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&state.db)
                .await
                .unwrap();
            assert_eq!(n, 0, "{table} must be empty after series delete");
        }
        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        let del = actions
            .iter()
            .find(|a| a.action == "library.series_deleted")
            .expect("delete audited");
        assert_eq!(del.target_id.as_deref(), Some(sid.as_str()));

        // Second delete -> 404.
        let res = full_router(&state)
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/series/{sid}"),
                Some(&admin),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn series_monitor_cascades() {
        let (state, _dir) = test_state().await;
        let (sid, _) = seed_library(&state).await;
        let op = seed_session(&state, "op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(req(
                "PUT",
                &format!("/api/v1/series/{sid}/monitor"),
                Some(&op),
                Some(json!({"monitored": false}).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["monitored"], false);

        // Cascade reached seasons and episodes.
        let (s_mon, e_mon) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT (SELECT monitored FROM seasons WHERE series_id = ? LIMIT 1), \
             (SELECT monitored FROM episodes WHERE series_id = ? LIMIT 1)",
        )
        .bind(&sid)
        .bind(&sid)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!((s_mon, e_mon), (0, 0));

        // Unknown id -> 404.
        let res = full_router(&state)
            .oneshot(req(
                "PUT",
                "/api/v1/series/nope/monitor",
                Some(&op),
                Some(json!({"monitored": true}).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    // -- Movies --------------------------------------------------------------

    #[tokio::test]
    async fn movies_create_list_delete() {
        let (state, _dir) = test_state().await;
        let admin = seed_session(&state, "admin@x.test", "admin").await;

        let body = json!({
            "title": "New Film",
            "path": "/movies/New Film",
            "tmdbId": 77,
            "qualityProfile": "ultra",
        })
        .to_string();
        let res = full_router(&state)
            .oneshot(req("POST", "/api/v1/movies", Some(&admin), Some(body)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["title"], "New Film");
        assert_eq!(v["qualityProfile"], "ultra");
        assert_eq!(v["monitored"], true, "monitored defaults to true");
        let mid = v["id"].as_str().unwrap().to_string();

        let op = seed_session(&state, "op@x.test", "operator").await;
        let res = full_router(&state)
            .oneshot(req("GET", "/api/v1/movies", Some(&op), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await.as_array().unwrap().len(), 1);

        let res = full_router(&state)
            .oneshot(req(
                "PUT",
                &format!("/api/v1/movies/{mid}/monitor"),
                Some(&op),
                Some(json!({"monitored": false}).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let row = movies::get_movie(&state.db, &mid).await.unwrap().unwrap();
        assert!(!row.monitored);

        let res = full_router(&state)
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/movies/{mid}"),
                Some(&admin),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(movies::get_movie(&state.db, &mid).await.unwrap().is_none());
        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "library.movie_deleted"));

        // Missing movie delete/monitor -> 404.
        let res = full_router(&state)
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/movies/{mid}"),
                Some(&admin),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = full_router(&state)
            .oneshot(req(
                "PUT",
                &format!("/api/v1/movies/{mid}/monitor"),
                Some(&op),
                Some(json!({"monitored": true}).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    // -- Quality profiles ----------------------------------------------------

    #[tokio::test]
    async fn quality_profiles_round_trip_and_merge() {
        let (state, _dir) = test_state().await;
        let admin = seed_session(&state, "admin@x.test", "admin").await;
        let op = seed_session(&state, "op@x.test", "operator").await;

        // Initially empty (no setup.policies yet).
        let res = full_router(&state)
            .oneshot(req("GET", "/api/v1/qualityprofiles", Some(&op), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await.as_array().unwrap().len(), 0);

        // Seed a wizard-style setup.policies document with extra keys and an
        // existing profile carrying upgradeAllowed.
        crate::storage::settings::upsert(
            &state.db,
            SETUP_POLICIES,
            r#"{"language":"en","bannedTags":["cam"],"qualityProfiles":[{"name":"HD","allowedQualities":["1080p"],"upgradeAllowed":true}]}"#,
        )
        .await
        .unwrap();

        // PUT replaces the list, adds a cutoff map, and preserves other keys.
        let body = json!({
            "qualityProfiles": [
                {"name": "HD", "allowedQualities": ["1080p", "720p"], "cutoff": 100},
                {"name": "SD", "allowedQualities": ["480p"], "cutoff": 0},
            ]
        })
        .to_string();
        let res = full_router(&state)
            .oneshot(req(
                "PUT",
                "/api/v1/qualityprofiles",
                Some(&admin),
                Some(body),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["saved"], 2);

        let policies: serde_json::Value = serde_json::from_str(
            &crate::storage::settings::get(&state.db, SETUP_POLICIES)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(policies["language"], "en", "unrelated keys survive");
        assert_eq!(policies["bannedTags"], json!(["cam"]));
        assert_eq!(policies["qualityProfiles"].as_array().unwrap().len(), 2);
        assert_eq!(
            policies["qualityProfiles"][0]["upgradeAllowed"], true,
            "prior profile extras preserved"
        );

        let cutoffs: serde_json::Value = serde_json::from_str(
            &crate::storage::settings::get(&state.db, PROFILES_CUTOFFS)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cutoffs, json!({"HD": 100, "SD": 0}));

        // GET merges profiles with their cutoffs.
        let res = full_router(&state)
            .oneshot(req("GET", "/api/v1/qualityprofiles", Some(&op), None))
            .await
            .unwrap();
        let v = body_json(res).await;
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["name"], "HD");
        assert_eq!(items[0]["allowedQualities"], json!(["1080p", "720p"]));
        assert_eq!(items[0]["cutoff"], 100);
        assert_eq!(items[1]["name"], "SD");
        assert_eq!(items[1]["cutoff"], 0);

        // The cutoff map is exactly what /wanted/cutoff_unmet consumes.
        let wanted = crate::httpapi::routes::wanted::router()
            .nest("/api/v1", crate::httpapi::routes::wanted::router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state.clone());
        let res = wanted
            .oneshot(req("GET", "/api/v1/wanted/cutoff_unmet", Some(&op), None))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "cutoff map readable by consumer"
        );

        // Validation: empty name, duplicate names, empty qualities, negative cutoff.
        let body = json!({
            "qualityProfiles": [
                {"name": "  ", "allowedQualities": ["1080p"]},
                {"name": "hd", "allowedQualities": []},
                {"name": "HD", "allowedQualities": ["720p"], "cutoff": -5},
            ]
        })
        .to_string();
        let res = full_router(&state)
            .oneshot(req(
                "PUT",
                "/api/v1/qualityprofiles",
                Some(&admin),
                Some(body),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        let errs = v["error"].as_str().unwrap();
        for expected in [
            "qualityProfiles.name is required",
            "qualityProfiles.allowedQualities must be non-empty",
            "qualityProfiles.name must be unique",
            "qualityProfiles.cutoff must be >= 0 for HD",
        ] {
            assert!(errs.contains(expected), "missing `{expected}` in {errs}");
        }
        // Rejected writes changed nothing.
        let cutoffs = crate::storage::settings::get(&state.db, PROFILES_CUTOFFS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cutoffs, r#"{"HD":100,"SD":0}"#);
    }

    // -- Auth matrix ---------------------------------------------------------

    #[tokio::test]
    async fn auth_matrix() {
        let (state, _dir) = test_state().await;
        let (sid, mid) = seed_library(&state).await;
        let _anon: Option<Session> = None;
        let viewer = seed_session(&state, "viewer@x.test", "viewer").await;
        let op = seed_session(&state, "op@x.test", "operator").await;
        let admin = seed_session(&state, "admin@x.test", "admin").await;

        let series_body = json!({"title": "T", "path": "/tv/T"}).to_string();
        let movies_body = json!({"title": "T", "path": "/movies/T"}).to_string();
        let qp_body =
            json!({"qualityProfiles": [{"name": "p", "allowedQualities": ["q"], "cutoff": 1}]})
                .to_string();

        // (method, path, body) — reads/monitor expect operator+, writes admin.
        let cases: Vec<(&str, String, Option<String>)> = vec![
            ("GET", "/api/v1/series".into(), None),
            ("GET", format!("/api/v1/series/{sid}"), None),
            ("GET", "/api/v1/movies".into(), None),
            ("GET", "/api/v1/qualityprofiles".into(), None),
            (
                "PUT",
                format!("/api/v1/series/{sid}/monitor"),
                Some(json!({"monitored": true}).to_string()),
            ),
            (
                "PUT",
                format!("/api/v1/movies/{mid}/monitor"),
                Some(json!({"monitored": true}).to_string()),
            ),
            ("POST", "/api/v1/series".into(), Some(series_body)),
            ("POST", "/api/v1/movies".into(), Some(movies_body)),
            ("DELETE", format!("/api/v1/series/{sid}"), None),
            ("DELETE", format!("/api/v1/movies/{mid}"), None),
            ("PUT", "/api/v1/qualityprofiles".into(), Some(qp_body)),
        ];

        // Anonymous -> 401 everywhere.
        for (method, path, body) in &cases {
            let res = full_router(&state)
                .oneshot(req(method, path, None, body.clone()))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        }

        // Viewer -> 403 everywhere.
        for (method, path, body) in &cases {
            let res = full_router(&state)
                .oneshot(req(method, path, Some(&viewer), body.clone()))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(
                body_json(res).await["error"],
                "forbidden",
                "{method} {path}"
            );
        }

        // Operator: reads and monitor toggles pass; admin-only writes -> 403.
        for (method, path, body) in &cases {
            let res = full_router(&state)
                .oneshot(req(method, path, Some(&op), body.clone()))
                .await
                .unwrap();
            let admin_only = matches!(*method, "POST" | "DELETE")
                || (*method == "PUT" && path.contains("qualityprofiles"));
            if admin_only {
                assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
            } else {
                assert_eq!(res.status(), StatusCode::OK, "{method} {path}");
            }
        }

        // Admin passes everything (deletes of the last rows may 404 on
        // repeat, so only assert not-forbidden).
        for (method, path, body) in &cases {
            let res = full_router(&state)
                .oneshot(req(method, path, Some(&admin), body.clone()))
                .await
                .unwrap();
            assert_ne!(res.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
    }

    /// Malformed JSON body -> 400, not a panic.
    #[tokio::test]
    async fn malformed_json_is_bad_request() {
        let (state, _dir) = test_state().await;
        let admin = seed_session(&state, "admin@x.test", "admin").await;
        for (method, path) in [
            ("POST", "/api/v1/series"),
            ("POST", "/api/v1/movies"),
            ("PUT", "/api/v1/qualityprofiles"),
        ] {
            let res = full_router(&state)
                .oneshot(req(method, path, Some(&admin), Some("{not json".into())))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}");
        }
    }
}
