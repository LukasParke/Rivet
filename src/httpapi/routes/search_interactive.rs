//! Interactive search & grab (operator): the *arr "Interactive Search"
//! endpoint. An operator types a free-text query, every configured indexer is
//! queried live, each release title is parsed and scored against the custom
//! formats, and the merged list is returned ranked by total score. The
//! operator can then grab a chosen release directly, bypassing decisioning
//! entirely: a manual pick IS the decision.
//!
//! Design notes:
//! - Per-indexer failures degrade to per-indexer error items; they never fail
//!   the whole search.
//! - No indexers configured is an honest empty result, not an error.
//! - Grab creates the acquisition row immediately and enqueues
//!   `acquisition_enqueue` (the same pipeline an automatic candidate uses),
//!   records an audit event, and publishes hub events.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::{Indexer, ReleaseKind, SearchQuery};
use crate::domain::formats::{total_score, CustomFormat};
use crate::error::AppError;
use crate::httpapi::middleware::auth::Operator;
use crate::jobs::handlers::{publish, search_common::load_indexers};
use crate::jobs::EnqueuePayload;
use crate::state::AppState;
use crate::storage::audit::{self, AuditEvent};
use crate::storage::jobs;
use crate::storage::settings;
use crate::storage::{acquisitions, domain};

/// Hard cap on merged results (releases + error items) returned to the client.
const MAX_RESULTS: usize = 200;

/// Per-indexer result cap. Indexers themselves are clamped to 100 by the
/// adapter; this keeps the intent explicit for the interactive path.
const PER_INDEXER_LIMIT: i64 = 100;

/// Settings key holding the wizard-persisted download-client credentials.
const CLIENTS_SETTING: &str = "setup.clients";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search/interactive", get(interactive_search))
        .route("/search/interactive/grab", post(interactive_grab))
}

// ---------------------------------------------------------------------------
// GET /search/interactive?query=...&seriesId=...&movieId=...
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveSearchParams {
    pub query: String,
    pub series_id: Option<String>,
    pub movie_id: Option<String>,
}

async fn interactive_search(
    Operator(user): Operator,
    State(state): State<AppState>,
    Query(params): Query<InteractiveSearchParams>,
) -> Result<Json<Value>, AppError> {
    let query = params.query.trim();
    if query.is_empty() {
        return Err(AppError::BadRequest("missing_query".into()));
    }
    let target_id = params
        .series_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            params
                .movie_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");

    let formats = load_custom_formats(&state.db).await?;

    // No indexers configured is an honest empty result. Any other loader
    // failure (e.g. unreadable settings) is a real 500.
    let indexers: Vec<Arc<dyn Indexer>> =
        match load_indexers(&state, "interactive", target_id).await {
            Ok(ix) => ix,
            Err(e) if e == "no_indexers_configured" => return Ok(Json(Value::Array(Vec::new()))),
            Err(e) => return Err(AppError::Internal(format!("indexer_load_failed: {e}"))),
        };

    let searches = indexers.iter().map(|ix| {
        let ix = Arc::clone(ix);
        let q = SearchQuery {
            query: query.to_string(),
            categories: vec![],
            limit: PER_INDEXER_LIMIT,
        };
        async move { (ix.name().to_string(), ix.search(&q).await) }
    });
    let results = futures_util::future::join_all(searches).await;

    // Ranked release items first (score desc), then per-indexer error items.
    let mut releases: Vec<(i64, Value)> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for (name, outcome) in results {
        match outcome {
            Ok(rels) => {
                for rel in rels {
                    let parsed = crate::domain::parsing::parse(&rel.title);
                    let score = total_score(&formats, &parsed);
                    let item = json!({
                        "title": rel.title,
                        "downloadUrl": rel.download_url,
                        "indexer": name,
                        "kind": kind_str(rel.kind),
                        "parsed": parsed,
                        // Custom-format score is the whole score today; both
                        // keys exist so clients can grow other components.
                        "formatScore": score,
                        "totalScore": score,
                    });
                    releases.push((score, item));
                }
            }
            Err(e) => errors.push(json!({ "indexer": name, "error": e.to_string() })),
        }
    }

    // Stable sort preserves indexer order among equal scores.
    releases.sort_by_key(|(score, _)| std::cmp::Reverse(*score));

    let mut items: Vec<Value> = releases.into_iter().map(|(_, v)| v).collect();
    items.append(&mut errors);
    items.truncate(MAX_RESULTS);

    publish(
        &state,
        "search.interactive.completed",
        json!({
            "userId": user.user.id,
            "query": query,
            "results": items.len(),
        }),
    );
    Ok(Json(Value::Array(items)))
}

fn kind_str(kind: ReleaseKind) -> &'static str {
    match kind {
        ReleaseKind::Torrent => "torrent",
        ReleaseKind::Usenet => "usenet",
    }
}

/// Loads the `custom_formats` setting (a JSON array of [`CustomFormat`]).
/// Absent OR malformed yields an empty list: format scoring must never take
/// down search.
async fn load_custom_formats(pool: &sqlx::SqlitePool) -> Result<Vec<CustomFormat>, sqlx::Error> {
    match settings::get(pool, "custom_formats").await? {
        None => Ok(Vec::new()),
        Some(raw) => match serde_json::from_str::<Vec<CustomFormat>>(&raw) {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %e, "custom_formats setting malformed; scoring without formats");
                Ok(Vec::new())
            }
        },
    }
}

// ---------------------------------------------------------------------------
// POST /search/interactive/grab
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveGrabRequest {
    pub release_title: String,
    pub download_url: String,
    /// "torrent" | "usenet".
    pub kind: String,
    pub series_id: Option<String>,
    pub movie_id: Option<String>,
    pub season: Option<i64>,
    /// Absolute episode numbers when the operator picked specific episodes.
    pub episodes: Option<Vec<i64>>,
}

async fn interactive_grab(
    Operator(user): Operator,
    State(state): State<AppState>,
    Json(req): Json<InteractiveGrabRequest>,
) -> Result<Response, AppError> {
    let title = req.release_title.trim();
    let url = req.download_url.trim();
    if title.is_empty() || url.is_empty() {
        return Err(AppError::BadRequest("missing_fields".into()));
    }
    let kind = match req.kind.as_str() {
        "torrent" | "usenet" => req.kind.clone(),
        _ => return Err(AppError::BadRequest("invalid_kind".into())),
    };
    // Manual grabs bypass decisioning, but the acquisition schema is
    // candidate- and target-keyed (FKs + unique index), so the grab gets real
    // provenance rows instead of synthetic ids:
    //   - seriesId/movieId given  → the referenced wanted target MUST exist;
    //   - neither given           → a standalone "interactive" wanted target
    //     is created from the release title.
    // A one-candidate search run records what was grabbed.
    let provided = req
        .series_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| ("series", s))
        .or_else(|| {
            req.movie_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| ("movie", s))
        });
    let target_id = match provided {
        Some((_kind, id)) => {
            if domain::get_wanted(&state.db, id).await?.is_some() {
                id.to_string()
            } else {
                return Err(AppError::BadRequest("unknown_target".into()));
            }
        }
        None => {
            let kind_label = match crate::domain::parsing::parse(title).kind {
                crate::domain::parsing::ReleaseKind::Movie => "movie",
                _ => "series",
            };
            let t = domain::create_wanted_target(
                &state.db,
                kind_label,
                "interactive",
                title,
                Some(&json!({ "query": title, "source": "interactive_search" })),
            )
            .await?;
            t.id
        }
    };
    let run = domain::create_search_run(&state.db, &target_id).await?;
    // `download_url` is the field acquisition_enqueue reads; `downloadUrl`
    // kept for wire/UI compatibility.
    let raw = json!({
        "title": title,
        "download_url": url,
        "downloadUrl": url,
        "kind": kind,
        "source": "interactive_search",
    });
    let cand =
        domain::insert_candidate(&state.db, &run.id, "interactive", &raw.to_string(), None).await?;
    domain::set_completed(&state.db, &run.id).await?;

    let acq = acquisitions::create_acquisition(
        &state.db,
        &cand.id,
        &target_id,
        &kind,
        &pick_client_kind(&state, &kind).await,
        "manual",
    )
    .await
    .map_err(|e| AppError::Internal(format!("create_failed: {e}")))?;

    audit::insert(
        &state.db,
        AuditEvent {
            actor_user_id: Some(user.user.id.clone()),
            action: "search.interactive.grab".into(),
            target_type: Some("acquisition".into()),
            target_id: Some(acq.id.clone()),
            ip: None,
            user_agent: None,
            details_json: Some(
                json!({
                    "releaseTitle": title,
                    "downloadUrl": url,
                    "kind": kind,
                    "seriesId": req.series_id,
                    "movieId": req.movie_id,
                    "season": req.season,
                    "episodes": req.episodes,
                })
                .to_string(),
            ),
        },
    )
    .await?;

    publish(
        &state,
        "acquisition.created",
        json!({
            "acquisitionId": acq.id,
            "candidateId": cand.id,
            "targetId": target_id,
            "source": "interactive_search",
        }),
    );

    match jobs::enqueue(
        &state.db,
        "acquisition_enqueue",
        Some(&EnqueuePayload {
            acquisition_id: acq.id.clone(),
        }),
        None,
    )
    .await
    {
        Err(e) => {
            // Acquisition row exists; enqueue failed. Surface the warning
            // honestly instead of pretending the download started.
            tracing::error!(error = %e, acquisition = %acq.id, "interactive grab enqueue failed");
            publish(
                &state,
                "job.enqueued",
                json!({
                    "kind": "acquisition_enqueue",
                    "acquisitionId": acq.id,
                    "enqueueFailed": true,
                }),
            );
            Ok((
                StatusCode::CREATED,
                Json(json!({
                    "ok": true,
                    "acquisitionId": acq.id,
                    "jobId": "",
                    "warning": "enqueue_failed",
                })),
            )
                .into_response())
        }
        Ok(job) => {
            state.metrics.inc_jobs_enqueued();
            publish(
                &state,
                "job.enqueued",
                json!({
                    "jobId": job.id,
                    "kind": "acquisition_enqueue",
                    "acquisitionId": acq.id,
                }),
            );
            Ok((
                StatusCode::CREATED,
                Json(json!({ "ok": true, "acquisitionId": acq.id, "jobId": job.id })),
            )
                .into_response())
        }
    }
}

/// Picks the client kind recorded on a grabbed acquisition from the
/// wizard-persisted `setup.clients` setting: the FIRST configured client among
/// the pipeline-supported kinds — torrents `qbittorrent|deluge|transmission`,
/// usenet `nzbget|sabnzbd`. When the setting is absent or malformed — or the
/// process runs dry-run, where no credentials are needed and the enqueue
/// pipeline simulates anyway — the historical `qbittorrent` / `nzbget`
/// fallbacks are kept so a grab never fails on missing client config.
async fn pick_client_kind(state: &AppState, release_kind: &str) -> String {
    const TORRENT_ORDER: [&str; 3] = ["qbittorrent", "deluge", "transmission"];
    const USENET_ORDER: [&str; 2] = ["nzbget", "sabnzbd"];
    let candidates: &[&str] = if release_kind == "usenet" {
        &USENET_ORDER
    } else {
        &TORRENT_ORDER
    };
    let fallback = candidates[0];
    if state.config.dry_run {
        return fallback.to_string();
    }
    let raw = match settings::get(&state.db, CLIENTS_SETTING).await {
        Ok(Some(raw)) => raw,
        _ => return fallback.to_string(),
    };
    let Ok(cfg) = serde_json::from_str::<Value>(&raw) else {
        return fallback.to_string();
    };
    candidates
        .iter()
        .find(|k| cfg.get(*k).is_some_and(Value::is_object))
        .map(|k| (*k).to_string())
        .unwrap_or_else(|| fallback.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::{AuthenticatedUser, Viewer};
    use crate::security::secrets::SecretBox;
    use crate::storage::migrations;
    use crate::storage::open;
    use crate::ws::hub::Hub;
    use std::time::Duration;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(&format!("file://{}", dir.path().join("t.db").display()))
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
                dry_run: false,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: Duration::from_secs(3600),
                allow_internal_urls: true,
            }),
        };
        (state, dir)
    }

    fn operator_user() -> Operator {
        Operator(AuthenticatedUser {
            user: crate::storage::users::User {
                id: "u1".into(),
                email: "op@rivet.test".into(),
                password_hash: "x".into(),
                role: "operator".into(),
                totp_secret_enc: None,
                totp_enabled: false,
                created_at: String::new(),
                updated_at: String::new(),
            },
            session: crate::storage::sessions::Session {
                id: "s1".into(),
                user_id: "u1".into(),
                csrf_token: "c".into(),
                created_at: String::new(),
                expires_at: String::new(),
                revoked_at: None,
                last_seen_at: None,
                ip: None,
                user_agent: None,
            },
        })
    }

    /// The grab handler writes an audit row keyed on the acting user; seed a
    /// matching users row so the FK holds.
    async fn seed_operator_user(state: &AppState) {
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, role, created_at, updated_at)
             VALUES ('u1', 'op@rivet.test', 'x', 'operator', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&state.db)
        .await
        .unwrap();
    }

    async fn body_json(res: axum::http::Response<axum::body::Body>) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    const TORZNAB_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:torznab="http://torznab.com/schemas/2015/feed" version="2.0">
  <channel>
    <item>
      <title>Show.Name.S01E05.720p.HDTV.x264</title>
      <link>https://example.com/download/low</link>
      <torznab:attr name="seeders" value="5"/>
    </item>
    <item>
      <title>Show.Name.S01E05.1080p.WEB-DL.x265</title>
      <link>https://example.com/download/high</link>
      <torznab:attr name="seeders" value="42"/>
    </item>
  </channel>
</rss>"#;

    async fn seed_indexer_and_formats(state: &AppState, server_uri: &str) {
        let key_enc = state.secret_box.encrypt_to_string(b"test-api-key").unwrap();
        let indexers = json!({
            "torznab": [{
                "name": "stub",
                "url": server_uri,
                "apiKeyEnc": key_enc,
            }],
        });
        settings::upsert(&state.db, "setup.indexers", &indexers.to_string())
            .await
            .unwrap();

        // A custom format that rewards 1080p so the WEB-DL release outranks
        // the HDTV one regardless of arrival order.
        let formats = json!([{
            "name": "HD 1080p",
            "score": 100,
            "enabled": true,
            "conditions": [{ "kind": "resolution", "value": "1080p", "negate": false }],
        }]);
        settings::upsert(&state.db, "custom_formats", &formats.to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn search_scores_and_orders_releases() {
        let (state, _dir) = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("t", "search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TORZNAB_XML))
            .mount(&server)
            .await;
        seed_indexer_and_formats(&state, &server.uri()).await;

        let res = interactive_search(
            operator_user(),
            State(state.clone()),
            Query(InteractiveSearchParams {
                query: "show name".into(),
                series_id: Some("tvdb-1".into()),
                movie_id: None,
            }),
        )
        .await
        .unwrap();

        let items = res.0.as_array().expect("array response");
        assert_eq!(items.len(), 2);
        // Highest totalScore first.
        assert_eq!(items[0]["title"], "Show.Name.S01E05.1080p.WEB-DL.x265");
        assert_eq!(items[0]["formatScore"], 100);
        assert_eq!(items[0]["totalScore"], 100);
        assert_eq!(items[0]["indexer"], "stub");
        assert_eq!(items[0]["kind"], "torrent");
        assert_eq!(items[0]["downloadUrl"], "https://example.com/download/high");
        // Parsed facts travel with the item.
        assert_eq!(items[0]["parsed"]["quality"]["resolution"], json!("1080p"));
        assert_eq!(items[1]["title"], "Show.Name.S01E05.720p.HDTV.x264");
        assert_eq!(items[1]["totalScore"], 0);
    }

    #[tokio::test]
    async fn search_without_indexers_is_honest_empty() {
        let (state, _dir) = test_state().await;
        let res = interactive_search(
            operator_user(),
            State(state.clone()),
            Query(InteractiveSearchParams {
                query: "dune".into(),
                series_id: None,
                movie_id: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(res.0, json!([]));
    }

    #[tokio::test]
    async fn search_reports_per_indexer_errors_without_failing() {
        let (state, _dir) = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        seed_indexer_and_formats(&state, &server.uri()).await;

        let res = interactive_search(
            operator_user(),
            State(state.clone()),
            Query(InteractiveSearchParams {
                query: "x".into(),
                series_id: None,
                movie_id: None,
            }),
        )
        .await
        .unwrap();

        let items = res.0.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["indexer"], "stub");
        assert!(
            items[0]["error"].as_str().is_some(),
            "error surfaced as an item"
        );
    }

    #[tokio::test]
    async fn grab_creates_acquisition_and_queues_job() {
        let (state, _dir) = test_state().await;
        seed_operator_user(&state).await;
        let t = domain::create_wanted_target(&state.db, "series", "tvdb", "42", None)
            .await
            .unwrap();

        let res = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "Show.Name.S01E05.1080p.WEB-DL.x265".into(),
                download_url: "https://example.com/download/high".into(),
                kind: "torrent".into(),
                series_id: Some(t.id.clone()),
                movie_id: None,
                season: Some(1),
                episodes: Some(vec![5]),
            }),
        )
        .await
        .unwrap();

        assert_eq!(res.status(), StatusCode::CREATED);
        let v = body_json(res).await;
        let acq_id = v["acquisitionId"].as_str().unwrap();
        assert!(v["jobId"].as_str().is_some());

        let acq = acquisitions::get(&state.db, acq_id)
            .await
            .unwrap()
            .expect("row persisted");
        assert_eq!(acq.target_id, t.id);
        assert_eq!(acq.kind, "torrent");
        // No `setup.clients` seeded: the grab falls back to qbittorrent.
        assert_eq!(acq.client_kind, "qbittorrent");

        // The grab left real provenance: a completed one-candidate run.
        let (raw_json, run_id): (String, String) =
            sqlx::query_as("SELECT raw_json, search_run_id FROM release_candidates WHERE id = ?")
                .bind(&acq.candidate_id)
                .fetch_one(&state.db)
                .await
                .unwrap();

        let raw: Value = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(raw["downloadUrl"], "https://example.com/download/high");
        let run = domain::get_search_run(&state.db, &run_id).await.unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.target_id, t.id);

        // Exactly one queued job wired to the acquisition.
        let queued = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'acquisition_enqueue' AND state = 'queued'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(queued, 1);
        let payload: String =
            sqlx::query_scalar("SELECT payload_json FROM jobs WHERE kind = 'acquisition_enqueue'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["acquisitionId"], *acq_id);

        // The manual choice is audited.
        let audits =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_events WHERE action = 'search.interactive.grab' AND target_id = ?")
                .bind(acq_id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(audits, 1);
    }

    #[tokio::test]
    async fn grab_uses_first_configured_client_kind() {
        let (state, _dir) = test_state().await;
        seed_operator_user(&state).await;
        // Only deluge configured: the grab must record it, not the hardcoded
        // qbittorrent default.
        let clients = json!({ "deluge": { "url": "http://deluge:8112" } });
        settings::upsert(&state.db, CLIENTS_SETTING, &clients.to_string())
            .await
            .unwrap();
        let t = domain::create_wanted_target(&state.db, "series", "tvdb", "42", None)
            .await
            .unwrap();

        let res = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "Show.Name.S01E05.1080p.WEB-DL.x265".into(),
                download_url: "magnet:?xt=urn:btih:abc".into(),
                kind: "torrent".into(),
                series_id: Some(t.id),
                movie_id: None,
                season: None,
                episodes: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let acq_id = body_json(res).await["acquisitionId"]
            .as_str()
            .unwrap()
            .to_string();
        let acq = acquisitions::get(&state.db, &acq_id)
            .await
            .unwrap()
            .expect("row persisted");
        assert_eq!(acq.client_kind, "deluge");

        // Usenet grabs pick the first configured usenet client the same way.
        let usenet = json!({
            "sabnzbd": { "url": "http://sab:8080" },
            "nzbget": { "url": "http://nzb:6789" },
        });
        settings::upsert(&state.db, CLIENTS_SETTING, &usenet.to_string())
            .await
            .unwrap();
        let res = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "Show.Name.S01E05.NZB".into(),
                download_url: "https://example.com/nzb".into(),
                kind: "usenet".into(),
                series_id: None,
                movie_id: None,
                season: None,
                episodes: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let acq_id = body_json(res).await["acquisitionId"]
            .as_str()
            .unwrap()
            .to_string();
        let acq = acquisitions::get(&state.db, &acq_id)
            .await
            .unwrap()
            .expect("row persisted");
        assert_eq!(acq.client_kind, "nzbget");
    }

    #[tokio::test]
    async fn grab_without_target_creates_standalone_wanted_target() {
        let (state, _dir) = test_state().await;
        seed_operator_user(&state).await;
        let res = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "Some.Movie.2020.1080p.BluRay.x264".into(),
                download_url: "https://example.com/download/movie".into(),
                kind: "torrent".into(),
                series_id: None,
                movie_id: None,
                season: None,
                episodes: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        let targets = domain::list_wanted(&state.db, 10).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].external_source, "interactive");
        assert_eq!(targets[0].kind, "movie");

        // A referenced-but-unknown target is rejected, never fabricated.
        let err = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "Show.Name.S01E05".into(),
                download_url: "https://example.com/download/x".into(),
                kind: "torrent".into(),
                series_id: Some("ghost".into()),
                movie_id: None,
                season: None,
                episodes: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn grab_validates_input() {
        let (state, _dir) = test_state().await;

        let err = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "  ".into(),
                download_url: "https://example.com/x".into(),
                kind: "torrent".into(),
                series_id: None,
                movie_id: None,
                season: None,
                episodes: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));

        let err = interactive_grab(
            operator_user(),
            State(state.clone()),
            Json(InteractiveGrabRequest {
                release_title: "t".into(),
                download_url: "u".into(),
                kind: "ftp".into(),
                series_id: None,
                movie_id: None,
                season: None,
                episodes: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));

        // Nothing was written on rejected input.
        let n = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM acquisitions")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn viewer_gets_403() {
        let (state, _dir) = test_state().await;

        fn viewer_ctx() -> crate::httpapi::middleware::auth::AuthContext {
            crate::httpapi::middleware::auth::AuthContext {
                user: crate::storage::users::User {
                    id: "v1".into(),
                    email: "view@rivet.test".into(),
                    password_hash: "x".into(),
                    role: "viewer".into(),
                    totp_secret_enc: None,
                    totp_enabled: false,
                    created_at: String::new(),
                    updated_at: String::new(),
                },
                session: crate::storage::sessions::Session {
                    id: "s2".into(),
                    user_id: "v1".into(),
                    csrf_token: "c".into(),
                    created_at: String::new(),
                    expires_at: String::new(),
                    revoked_at: None,
                    last_seen_at: None,
                    ip: None,
                    user_agent: None,
                },
            }
        }

        // Exercise the exact extractor the route handlers are guarded by:
        // a live viewer session is rejected as Forbidden (403).
        let req = axum::http::Request::builder()
            .method(axum::http::Method::GET)
            .uri("/api/v1/search/interactive")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        parts.extensions.insert(viewer_ctx());
        let rejection =
            <Operator as axum::extract::FromRequestParts<AppState>>::from_request_parts(
                &mut parts, &state,
            )
            .await
            .err()
            .expect("operator guard must reject viewers");
        let (code, status) = rejection.code();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code.to_string(), "forbidden");

        // Sanity on the harness: the same context satisfies a Viewer guard.
        let req = axum::http::Request::builder()
            .method(axum::http::Method::GET)
            .uri("/api/v1/search/interactive")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        parts.extensions.insert(viewer_ctx());
        assert!(
            <Viewer as axum::extract::FromRequestParts<AppState>>::from_request_parts(
                &mut parts, &state,
            )
            .await
            .is_ok()
        );
    }
}
