//! `enrich_series` job handler: populates real seasons/episodes for a series
//! wanted target from a metadata provider (TMDB preferred, TVDB fallback).
//!
//! Flow: load the wanted target's external ids → pick the provider from the
//! decrypted `setup.metadata` setting → fetch every season's episodes → upsert
//! the series row (status derived from first/last air dates), its seasons
//! (monitored) and its episodes (monitored follows the series). Re-runs are
//! idempotent: storage upserts match on external ids and unique keys, and
//! import state (`has_file`) is never touched.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::metadata::tmdb::{TmdbClient, TmdbConfig};
use crate::adapters::metadata::tvdb::{TvdbClient, TvdbConfig};
use crate::jobs::EnrichSeriesPayload;
use crate::state::AppState;
use crate::storage::jobs::Job;
use crate::storage::series::{
    bulk_upsert_episodes, upsert_season, upsert_series, EpisodeUpsert, Series, SeriesUpsert,
};

const SETTING_KEY: &str = "setup.metadata";

/// What one enrichment run did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichReport {
    pub seasons: usize,
    pub episodes: usize,
    pub provider: String,
}

/// Job entry point.
pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: EnrichSeriesPayload = parse_payload(job)?;
    if payload.target_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }
    enrich(state, &payload.target_id).await.map(|_| ())
}

fn parse_payload(job: &Job) -> Result<EnrichSeriesPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

/// External ids carried on the wanted target payload. Ids may arrive as
/// strings or numbers; both snake_case and camelCase keys are accepted.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetPayload {
    #[serde(default, alias = "tmdb_id")]
    tmdb_id: Option<Value>,
    #[serde(default, alias = "tvdb_id")]
    tvdb_id: Option<Value>,
    #[serde(default)]
    title: Option<String>,
}

impl TargetPayload {
    fn id(raw: &Option<Value>) -> Option<String> {
        match raw {
            Some(Value::String(s)) => {
                let s = s.trim();
                (!s.is_empty()).then(|| s.to_string())
            }
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    }

    fn title(&self) -> Option<String> {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    }
}

/// One provider entry of the persisted `setup.metadata` setting
/// (`{"baseUrl": ..., "apiKeyEnc": ...}`), key decrypted via the SecretBox.
#[derive(Debug, Default)]
struct ProviderSetting {
    base_url: String,
    api_key: Option<String>,
}

async fn load_provider(state: &AppState, provider: &str) -> Result<ProviderSetting, String> {
    let Some(raw) = crate::storage::settings::get(&state.db, SETTING_KEY)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(ProviderSetting::default());
    };
    let obj: BTreeMap<String, Value> = serde_json::from_str(&raw).unwrap_or_default();
    let Some(entry) = obj.get(provider) else {
        return Ok(ProviderSetting::default());
    };
    let base_url = entry
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let api_key = entry
        .get("apiKeyEnc")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(|enc| state.secret_box.decrypt_string(enc).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    Ok(ProviderSetting { base_url, api_key })
}

/// Provider-normalized episode, ready for [`crate::storage::series`] upserts.
struct NormalizedEpisode {
    season: i64,
    number: i64,
    title: Option<String>,
    /// `YYYY-MM-DD` straight from the provider.
    air_date: Option<String>,
    runtime: Option<i64>,
    overview: Option<String>,
}

/// Everything enrichment needs from a provider before persistence.
struct FetchedSeries {
    title: Option<String>,
    first_air: Option<String>,
    last_air: Option<String>,
    episodes: Vec<NormalizedEpisode>,
}

pub async fn enrich(state: &AppState, target_id: &str) -> Result<EnrichReport, String> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT external_source, external_id, payload_json \
         FROM wanted_targets WHERE id = ?",
    )
    .bind(target_id.trim())
    .fetch_optional(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    let (source, external_id, payload_json) = row.ok_or("target_not_found")?;
    let payload: TargetPayload = match payload_json.as_deref() {
        Some(raw) => {
            serde_json::from_str(raw).map_err(|e| format!("target_payload_invalid: {e}"))?
        }
        None => TargetPayload::default(),
    };

    // Payload ids win; otherwise fall back to the target's own coordinates.
    let tmdb_id = TargetPayload::id(&payload.tmdb_id)
        .or_else(|| (source == "tmdb").then(|| external_id.clone()))
        .filter(|id| !id.is_empty());
    let tvdb_id = TargetPayload::id(&payload.tvdb_id)
        .or_else(|| (source == "tvdb").then(|| external_id.clone()))
        .filter(|id| !id.is_empty());

    let tmdb = load_provider(state, "tmdb").await?;
    let tvdb = load_provider(state, "tvdb").await?;

    // TMDB is preferred when the target carries a TMDB id AND a key is set.
    let (fetched, provider) = if let (Some(id), true) = (&tmdb_id, tmdb.api_key.is_some()) {
        (
            fetch_tmdb(state, &tmdb, id)
                .await
                .map_err(|e| format!("tmdb_enrich_failed: {e}"))?,
            "tmdb",
        )
    } else if let (Some(id), true) = (&tvdb_id, tvdb.api_key.is_some()) {
        (
            fetch_tvdb(state, &tvdb, id)
                .await
                .map_err(|e| format!("tvdb_enrich_failed: {e}"))?,
            "tvdb",
        )
    } else {
        return Err("no_metadata_provider_configured".to_string());
    };

    persist(
        state,
        provider,
        &payload,
        &fetched,
        tmdb_id.as_deref(),
        tvdb_id.as_deref(),
    )
    .await
    .map(|(seasons, episodes)| EnrichReport {
        seasons,
        episodes,
        provider: provider.into(),
    })
}

async fn fetch_tmdb(
    state: &AppState,
    cfg: &ProviderSetting,
    show_id: &str,
) -> Result<FetchedSeries, String> {
    let mut client = TmdbClient::new(
        &TmdbConfig {
            api_key: cfg.api_key.clone(),
        },
        state.policy,
    );
    if !cfg.base_url.is_empty() {
        client = client.with_base_url(cfg.base_url.clone());
    }
    let show = client
        .show_details(show_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut episodes: Vec<NormalizedEpisode> = Vec::new();
    for season in &show.seasons {
        let eps = client
            .season_episodes(show_id, *season)
            .await
            .map_err(|e| format!("season_{season}: {e}"))?;
        for ep in eps {
            episodes.push(NormalizedEpisode {
                season: i64::from(*season),
                number: i64::from(ep.episode_number),
                title: (!ep.name.is_empty()).then_some(ep.name),
                air_date: ep.air_date,
                runtime: ep.runtime,
                overview: ep.overview,
            });
        }
    }
    Ok(FetchedSeries {
        title: show.name,
        first_air: min_date(&episodes).or(show.first_air_date),
        last_air: max_date(&episodes).or(show.last_air_date),
        episodes,
    })
}

async fn fetch_tvdb(
    state: &AppState,
    cfg: &ProviderSetting,
    series_id: &str,
) -> Result<FetchedSeries, String> {
    let config = TvdbConfig {
        api_key: cfg.api_key.clone(),
    };
    let mut client = TvdbClient::new(&config, state.policy).map_err(|e| e.to_string())?;
    if !cfg.base_url.is_empty() {
        client = client.with_base_url(cfg.base_url.clone());
    }
    let eps = client
        .series_episodes(series_id)
        .await
        .map_err(|e| e.to_string())?;
    let title = eps.iter().find_map(|ep| ep.series_name.clone());
    let episodes: Vec<NormalizedEpisode> = eps
        .into_iter()
        .map(|ep| NormalizedEpisode {
            season: i64::from(ep.season_number),
            number: i64::from(ep.number),
            title: ep.name,
            air_date: ep.aired,
            runtime: ep.runtime,
            overview: ep.overview,
        })
        .collect();
    Ok(FetchedSeries {
        title,
        first_air: min_date(&episodes),
        last_air: max_date(&episodes),
        episodes,
    })
}

fn date_of(
    episodes: &[NormalizedEpisode],
    pick: impl Fn(&[String]) -> Option<String>,
) -> Option<String> {
    let dates: Vec<String> = episodes.iter().filter_map(|e| e.air_date.clone()).collect();
    if dates.is_empty() {
        None
    } else {
        pick(&dates)
    }
}

fn min_date(episodes: &[NormalizedEpisode]) -> Option<String> {
    date_of(episodes, |d| d.iter().min().cloned())
}

fn max_date(episodes: &[NormalizedEpisode]) -> Option<String> {
    date_of(episodes, |d| d.iter().max().cloned())
}

/// Sonarr-style status derived purely from air dates: nothing aired yet and a
/// future premiere means `upcoming`; the latest airing in the past means
/// `ended`; anything else (airing now, or unknown dates) stays `continuing`.
fn series_status(first_air: Option<&str>, last_air: Option<&str>) -> String {
    let today = chrono::Utc::now().date_naive();
    let parse =
        |s: Option<&str>| s.and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    if parse(first_air).is_some_and(|first| first > today) {
        return "upcoming".to_string();
    }
    if parse(last_air).is_some_and(|last| last < today) {
        return "ended".to_string();
    }
    "continuing".to_string()
}

/// Providers deliver bare dates; the domain stores RFC3339 timestamps so the
/// calendar/wanted queries compare consistently with `timeutil::now()`.
fn air_date_utc(date: &str) -> String {
    if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
        format!("{date}T00:00:00Z")
    } else {
        date.to_string()
    }
}

/// Finds or creates the series row keyed by external ids, refreshes its
/// status/title/first-aired, then bulk-upserts seasons (monitored) and
/// episodes (monitored follows the series).
async fn persist(
    state: &AppState,
    provider: &str,
    payload: &TargetPayload,
    fetched: &FetchedSeries,
    tmdb_id: Option<&str>,
    tvdb_id: Option<&str>,
) -> Result<(usize, usize), String> {
    let tmdb_num = tmdb_id.and_then(|id| id.parse::<i64>().ok());
    let tvdb_num = tvdb_id.and_then(|id| id.parse::<i64>().ok());
    let status = series_status(fetched.first_air.as_deref(), fetched.last_air.as_deref());

    let existing: Option<Series> = sqlx::query_as::<_, Series>(
        "SELECT * FROM series WHERE \
         (tmdb_id IS NOT NULL AND tmdb_id = ?) \
         OR (tvdb_id IS NOT NULL AND tvdb_id = ?) LIMIT 1",
    )
    .bind(tmdb_num)
    .bind(tvdb_num)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| e.to_string())?;

    let series = match existing {
        Some(series) => {
            // Refresh metadata in place; never clobber the library path or
            // monitoring choices made by the user. An absent provider title
            // keeps the existing one (COALESCE).
            let now = crate::timeutil::now();
            sqlx::query(
                "UPDATE series SET title = COALESCE(?, title), status = ?, \
                 first_aired = COALESCE(?, first_aired), updated_at = ? WHERE id = ?",
            )
            .bind(fetched.title.as_deref())
            .bind(&status)
            .bind(fetched.first_air.as_deref())
            .bind(&now)
            .bind(&series.id)
            .execute(&state.db)
            .await
            .map_err(|e| e.to_string())?;
            crate::storage::series::get_series(&state.db, &series.id)
                .await
                .map_err(|e| e.to_string())?
                .expect("row just updated")
        }
        None => {
            let external = tmdb_id.or(tvdb_id).unwrap_or("0");
            let title = fetched
                .title
                .clone()
                .or_else(|| payload.title())
                .unwrap_or_else(|| format!("{provider}-{external}"));
            let input = SeriesUpsert {
                id: None,
                tvdb_id: tvdb_num,
                tmdb_id: tmdb_num,
                imdb_id: None,
                title,
                sort_title: None,
                status,
                overview: None,
                network: None,
                first_aired: fetched.first_air.clone(),
                genres: vec![],
                // No library path yet; deterministic per external id until an
                // import assigns the real location.
                path: format!("/library/{provider}-{external}"),
                quality_profile: "default".to_string(),
                monitored: true,
                season_folder: true,
                tags: vec![],
            };
            upsert_series(&state.db, &input)
                .await
                .map_err(|e| e.to_string())?
                .0
        }
    };

    // Seasons come from the fetched episode set; all monitored.
    let seasons: BTreeSet<i64> = fetched.episodes.iter().map(|e| e.season).collect();
    for number in &seasons {
        upsert_season(&state.db, &series.id, *number, true)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Episodes monitored follow the series. Absolute numbering runs across
    // regular seasons only (specials stay NULL, matching Sonarr).
    let mut ordered: Vec<&NormalizedEpisode> = fetched.episodes.iter().collect();
    ordered.sort_by_key(|e| (e.season, e.number));
    let mut absolute: i64 = 0;
    let ups: Vec<EpisodeUpsert> = ordered
        .iter()
        .map(|&e| {
            let abs = if e.season > 0 {
                absolute += 1;
                Some(absolute)
            } else {
                None
            };
            EpisodeUpsert {
                season_number: e.season,
                episode_number: e.number,
                absolute_episode_number: abs,
                title: e.title.clone(),
                air_date_utc: e.air_date.as_deref().map(air_date_utc),
                runtime: e.runtime,
                overview: e.overview.clone(),
                monitored: series.monitored,
            }
        })
        .collect();
    bulk_upsert_episodes(&state.db, &series.id, &ups)
        .await
        .map_err(|e| e.to_string())?;

    Ok((seasons.len(), ups.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::settings;

    async fn seed_target(state: &AppState, payload: Value) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, payload_json, created_at, updated_at)
             VALUES (?, 'series', 'tmdb', '1399', 'wanted', ?, ?, ?)",
        )
        .bind(&id)
        .bind(payload.to_string())
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        id
    }

    async fn seed_provider_setting(state: &AppState, body: Value) {
        settings::upsert(&state.db, SETTING_KEY, &body.to_string())
            .await
            .unwrap();
    }

    fn enc(state: &AppState, plain: &str) -> String {
        state
            .secret_box
            .encrypt_to_string(plain.as_bytes())
            .unwrap()
    }

    async fn count_episodes(state: &AppState) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM episodes")
            .fetch_one(&state.db)
            .await
            .unwrap()
    }

    fn tmdb_mocks() -> Vec<Mock> {
        vec![
            Mock::given(method("GET"))
                .and(path("/3/tv/1399"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "name": "Game of Thrones",
                    "first_air_date": "2011-04-17",
                    "last_air_date": "2019-05-19",
                    "seasons": [{"season_number": 1}, {"season_number": 2}]
                }))),
            Mock::given(method("GET"))
                .and(path("/3/tv/1399/season/1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "episodes": [
                        {"episode_number": 1, "name": "Winter Is Coming", "air_date": "2011-04-17", "runtime": 62, "overview": "Ned"},
                        {"episode_number": 2, "name": "The Kingsroad", "air_date": "2011-04-24", "runtime": 56, "overview": null}
                    ]
                }))),
            Mock::given(method("GET"))
                .and(path("/3/tv/1399/season/2"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "episodes": [
                        {"episode_number": 1, "name": "The North Remembers", "air_date": "2011-04-17", "runtime": 63, "overview": null},
                        {"episode_number": 2, "name": "The Night Lands", "air_date": "2011-04-24", "runtime": 55, "overview": null}
                    ]
                }))),
        ]
    }

    #[tokio::test]
    async fn tmdb_enrichment_upserts_series_seasons_and_episodes() {
        let (dir, mut state) = crate::jobs::handlers::testutil::test_state().await;
        state.policy = crate::security::OutboundPolicy::dev();
        let server = MockServer::start().await;
        for mock in tmdb_mocks() {
            mock.mount(&server).await;
        }

        let target = seed_target(
            &state,
            serde_json::json!({"tmdbId": "1399", "tvdbId": "121361", "title": "GoT"}),
        )
        .await;
        seed_provider_setting(
            &state,
            serde_json::json!({"tmdb": {"baseUrl": server.uri(), "apiKeyEnc": enc(&state, "k")}}),
        )
        .await;

        let report = enrich(&state, &target).await.unwrap();
        assert_eq!(report.provider, "tmdb");
        assert_eq!(report.seasons, 2);
        assert_eq!(report.episodes, 4);
        assert_eq!(count_episodes(&state).await, 4);

        let series: Series = sqlx::query_as("SELECT * FROM series LIMIT 1")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(series.tmdb_id, Some(1399));
        assert_eq!(series.tvdb_id, Some(121361));
        assert_eq!(series.status, "ended", "last air date is in the past");
        assert_eq!(series.first_aired.as_deref(), Some("2011-04-17"));

        let ep: (i64, i64, Option<i64>, bool, String) = sqlx::query_as(
            "SELECT season_number, episode_number, absolute_episode_number, monitored, air_date_utc \
             FROM episodes WHERE season_number = 1 AND episode_number = 1",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(
            ep,
            (1, 1, Some(1), true, "2011-04-17T00:00:00Z".to_string())
        );

        // Absolute numbering runs across regular seasons in order.
        let s2e2_abs: Option<i64> = sqlx::query_scalar(
            "SELECT absolute_episode_number FROM episodes WHERE season_number = 2 AND episode_number = 2",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(s2e2_abs, Some(4));

        let seasons: Vec<(i64, bool)> =
            sqlx::query_as("SELECT number, monitored FROM seasons ORDER BY number")
                .fetch_all(&state.db)
                .await
                .unwrap();
        assert_eq!(seasons, vec![(1, true), (2, true)]);

        drop(dir);
    }

    #[tokio::test]
    async fn re_enrich_updates_in_place_without_duplicating() {
        let (dir, mut state) = crate::jobs::handlers::testutil::test_state().await;
        state.policy = crate::security::OutboundPolicy::dev();
        let server = MockServer::start().await;
        for mock in tmdb_mocks() {
            mock.expect(2).mount(&server).await;
        }

        let target = seed_target(&state, serde_json::json!({"tmdbId": "1399"})).await;
        seed_provider_setting(
            &state,
            serde_json::json!({"tmdb": {"baseUrl": server.uri(), "apiKeyEnc": enc(&state, "k")}}),
        )
        .await;

        enrich(&state, &target).await.unwrap();
        enrich(&state, &target).await.unwrap();

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM series")
                .fetch_one(&state.db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(count_episodes(&state).await, 4);
        drop(dir);
    }

    #[tokio::test]
    async fn tvdb_fallback_when_tmdb_key_missing_and_paginates() {
        let (dir, mut state) = crate::jobs::handlers::testutil::test_state().await;
        state.policy = crate::security::OutboundPolicy::dev();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v4/login"))
            .and(body_json(serde_json::json!({"apikey": "k"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": {"token": "tok"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v4/series/121361/episodes/default"))
            .and(query_param_is_missing("page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "episodes": [
                        {"number": 1, "seasonNumber": 1, "name": "Pilot", "aired": "2026-01-01", "runtime": 45, "overview": "o", "seriesName": "Future Show"},
                        {"number": 2, "seasonNumber": 1, "name": "Two", "aired": "2099-01-08", "runtime": null, "overview": null}
                    ],
                    "links": {"next": "/v4/series/121361/episodes/default?page=2"}
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v4/series/121361/episodes/default"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"episodes": [{"number": 1, "seasonNumber": 2, "name": "S2", "aired": "2099-02-01"}], "links": {}}
            })))
            .mount(&server)
            .await;

        // Only TVDB is configured; the target still carries both ids.
        let target = seed_target(
            &state,
            serde_json::json!({"tmdbId": "1399", "tvdbId": "121361"}),
        )
        .await;
        seed_provider_setting(
            &state,
            serde_json::json!({"tvdb": {"baseUrl": server.uri(), "apiKeyEnc": enc(&state, "k")}}),
        )
        .await;

        let report = enrich(&state, &target).await.unwrap();
        assert_eq!(report.provider, "tvdb");
        assert_eq!(report.seasons, 2);
        assert_eq!(report.episodes, 3);

        let series: Series = sqlx::query_as("SELECT * FROM series LIMIT 1")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(series.tvdb_id, Some(121361));
        assert_eq!(series.title, "Future Show");
        assert_eq!(series.status, "continuing", "an episode airs in the future");
        drop(dir);
    }

    #[tokio::test]
    async fn missing_target_or_provider_fails_cleanly() {
        let (dir, state) = crate::jobs::handlers::testutil::test_state().await;

        let err = enrich(&state, "nope").await.unwrap_err();
        assert_eq!(err, "target_not_found");

        let target = seed_target(&state, serde_json::json!({"tmdbId": "1399"})).await;
        // Setting exists but neither provider has a usable key.
        seed_provider_setting(
            &state,
            serde_json::json!({"tmdb": {"baseUrl": "", "apiKeyEnc": ""}}),
        )
        .await;
        let err = enrich(&state, &target).await.unwrap_err();
        assert_eq!(err, "no_metadata_provider_configured");
        drop(dir);
    }

    #[tokio::test]
    async fn provider_failure_surfaces_as_error() {
        let (dir, mut state) = crate::jobs::handlers::testutil::test_state().await;
        state.policy = crate::security::OutboundPolicy::dev();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let target = seed_target(&state, serde_json::json!({"tmdbId": "1399"})).await;
        seed_provider_setting(
            &state,
            serde_json::json!({"tmdb": {"baseUrl": server.uri(), "apiKeyEnc": enc(&state, "k")}}),
        )
        .await;

        let err = enrich(&state, &target).await.unwrap_err();
        assert!(err.starts_with("tmdb_enrich_failed"));
        drop(dir);
    }

    #[tokio::test]
    async fn status_rules_follow_air_dates() {
        let today = chrono::Utc::now().date_naive();
        let d = |days: i64| {
            (today + chrono::Duration::days(days))
                .format("%Y-%m-%d")
                .to_string()
        };
        assert_eq!(series_status(Some(&d(-30)), Some(&d(-1))), "ended");
        assert_eq!(series_status(Some(&d(-30)), Some(&d(7))), "continuing");
        assert_eq!(series_status(Some(&d(30)), None), "upcoming");
        assert_eq!(series_status(None, None), "continuing");
    }

    #[tokio::test]
    async fn bare_dates_become_rfc3339() {
        assert_eq!(air_date_utc("2026-01-05"), "2026-01-05T00:00:00Z");
        assert_eq!(air_date_utc("2026-01-05T12:00:00Z"), "2026-01-05T12:00:00Z");
    }

    #[test]
    fn payload_ids_accept_strings_numbers_and_snake_case() {
        let p: TargetPayload =
            serde_json::from_str(r#"{"tmdbId": "42", "tvdb_id": 7, "title": " T "}"#).unwrap();
        assert_eq!(TargetPayload::id(&p.tmdb_id).as_deref(), Some("42"));
        assert_eq!(TargetPayload::id(&p.tvdb_id).as_deref(), Some("7"));
        assert_eq!(p.title().as_deref(), Some("T"));
    }
}
