//! Wanted endpoints (*arr parity): missing episodes and cutoff-unmet items.
//! Both require operator role or above.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::httpapi::middleware::auth::Operator;
use crate::state::AppState;
use crate::storage::calendar_queries::{self, CutoffEpisode, EpisodeWithSeries};

/// Settings key mapping quality-profile name → minimum quality score.
const PROFILES_CUTOFFS: &str = "profiles.cutoffs";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/wanted/missing", get(wanted_missing))
        .route("/wanted/cutoff_unmet", get(wanted_cutoff_unmet))
}

// ---------------------------------------------------------------------------
// Error plumbing: {"error": code} envelopes.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: String,
}

impl From<sqlx::Error> for ApiError {
    fn from(_: sqlx::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error".into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Pagination + wire envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Page<T> {
    items: Vec<T>,
    total: i64,
    page: i64,
    #[serde(rename = "pageSize")]
    page_size: i64,
}

/// Parses `page` (1-based) and `pageSize` (1..=100) with *arr defaults.
fn page_params(params: &HashMap<String, String>) -> (i64, i64) {
    let page = params
        .get("page")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let page_size = params
        .get("pageSize")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 100);
    (page, page_size)
}

// ---------------------------------------------------------------------------
// GET /wanted/missing?page&pageSize
// ---------------------------------------------------------------------------

async fn wanted_missing(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Page<EpisodeWithSeries>>, ApiError> {
    let (page, page_size) = page_params(&params);
    let total = calendar_queries::count_missing_episodes(&state.db).await?;
    let offset = (page - 1).saturating_mul(page_size);
    let items = calendar_queries::missing_episodes(&state.db, page_size, offset).await?;
    Ok(Json(Page {
        items,
        total,
        page,
        page_size,
    }))
}

// ---------------------------------------------------------------------------
// GET /wanted/cutoff_unmet?page&pageSize
// ---------------------------------------------------------------------------

/// Loads the `profiles.cutoffs` settings map ({profileName: score}). Absent or
/// malformed settings yield an empty map — nothing is then unmet, which is the
/// honest answer when no cutoffs are configured.
async fn load_cutoffs(state: &AppState) -> Result<HashMap<String, i64>, ApiError> {
    let Some(raw) = crate::storage::settings::get(&state.db, PROFILES_CUTOFFS).await? else {
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

async fn wanted_cutoff_unmet(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Page<CutoffEpisode>>, ApiError> {
    let (page, page_size) = page_params(&params);
    let cutoffs = load_cutoffs(&state).await?;
    let all = calendar_queries::cutoff_unmet_episodes(&state.db, &cutoffs).await?;
    let total = all.len() as i64;
    let offset = (page - 1).saturating_mul(page_size) as usize;
    let items = all.into_iter().skip(offset).take(page_size as usize).collect();
    Ok(Json(Page {
        items,
        total,
        page,
        page_size,
    }))
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

    /// Creates a user + live session directly.
    async fn seed_session(state: &AppState, email: &str, role: &str) -> Session {
        let u = crate::storage::users::create(&state.db, email, "test-hash", role)
            .await
            .unwrap();
        crate::storage::sessions::create(&state.db, &u.id, chrono::Duration::hours(1), None, None)
            .await
            .unwrap()
    }

    fn get(uri: &str, session: Option<&Session>) -> Request {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(s) = session {
            b = b.header("cookie", format!("rivet_session={}", s.id));
        }
        b.body(Body::empty()).unwrap()
    }

    async fn body_json(res: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// One series plus `n` monitored aired episodes without files.
    async fn seed_missing(state: &AppState, n: usize) {
        sqlx::query(
            "INSERT INTO series (id, title, path, quality_profile, added_at, updated_at) \
             VALUES ('s1', 'Show', '/tv/Show', 'default', \
             '2026-01-01T00:00:00.000000000Z', '2026-01-01T00:00:00.000000000Z')",
        )
        .execute(&state.db)
        .await
        .unwrap();
        for i in 0..n {
            sqlx::query(
                "INSERT INTO episodes (id, series_id, season_number, episode_number, title, \
                 air_date_utc, monitored, has_file) \
                 VALUES (?, 's1', 1, ?, 'Ep', ?, 1, 0)",
            )
            .bind(format!("e{i}"))
            .bind(i as i64 + 1)
            .bind(format!("2026-08-{:02}T05:00:00Z", 10 + i))
            .execute(&state.db)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn missing_is_paginated_with_total() {
        let (state, _dir) = test_state().await;
        seed_missing(&state, 3).await;
        let op = seed_session(&state, "op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(get(
                "/api/v1/wanted/missing?page=1&pageSize=2",
                Some(&op),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["total"], 3);
        assert_eq!(v["page"], 1);
        assert_eq!(v["pageSize"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);

        // Second page carries the remainder.
        let res = full_router(&state)
            .oneshot(get("/api/v1/wanted/missing?page=2&pageSize=2", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["items"].as_array().unwrap().len(), 1);

        // Out-of-range params clamp instead of erroring.
        let res = full_router(&state)
            .oneshot(get("/api/v1/wanted/missing?page=0&pageSize=999", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["page"], 1);
        assert_eq!(v["pageSize"], 100);
    }

    #[tokio::test]
    async fn cutoff_unmet_compares_scores_against_configured_cutoffs() {
        let (state, _dir) = test_state().await;
        sqlx::query(
            "INSERT INTO series (id, title, path, quality_profile, added_at, updated_at) \
             VALUES ('s1', 'Show', '/tv/Show', 'default', \
             '2026-01-01T00:00:00.000000000Z', '2026-01-01T00:00:00.000000000Z')",
        )
        .execute(&state.db)
        .await
        .unwrap();
        // Two on-disk episodes: one below cutoff, one meeting it.
        for (i, (id, score)) in [("low", 60), ("met", 100)].into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO episodes (id, series_id, season_number, episode_number, title, \
                 air_date_utc, monitored, has_file, episode_file_id) \
                 VALUES (?, 's1', 1, ?, 'Ep', '2026-08-20T05:00:00Z', 1, 1, ?)",
            )
            .bind(id)
            .bind(i as i64 + 1)
            .bind(format!("f-{id}"))
            .execute(&state.db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO episode_files (id, series_id, season_number, episode_numbers_json, \
                 relative_path, quality, quality_score, date_added) \
                 VALUES (?, 's1', 1, '[1]', ?, 'WEBDL-1080p', ?, '2026-08-20T06:00:00Z')",
            )
            .bind(format!("f-{id}"))
            .bind(format!("/tv/Show/{id}.mkv"))
            .bind(score)
            .execute(&state.db)
            .await
            .unwrap();
        }
        let op = seed_session(&state, "op@x.test", "operator").await;

        // Without the setting configured: honest empty result.
        let res = full_router(&state)
            .oneshot(get("/api/v1/wanted/cutoff_unmet", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["items"].as_array().unwrap().len(), 0);
        assert_eq!(v["total"], 0);

        // Configure {"default": 100}: only the 60-score file is unmet.
        crate::storage::settings::upsert(
            &state.db,
            PROFILES_CUTOFFS,
            r#"{"default": 100}"#,
        )
        .await
        .unwrap();
        let res = full_router(&state)
            .oneshot(get("/api/v1/wanted/cutoff_unmet", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let items = v["items"].as_array().unwrap();
        assert_eq!(v["total"], 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "low");
        assert_eq!(items[0]["qualityScore"], 60);
        assert_eq!(items[0]["seriesTitle"], "Show");
    }

    #[tokio::test]
    async fn auth_requires_operator_role() {
        let (state, _dir) = test_state().await;
        seed_missing(&state, 1).await;

        for (n, uri) in ["/api/v1/wanted/missing", "/api/v1/wanted/cutoff_unmet"]
            .into_iter()
            .enumerate()
        {
            // Anonymous -> 401 unauthorized.
            let res = full_router(&state).oneshot(get(uri, None)).await.unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");

            // Viewer -> 403 forbidden.
            let viewer = seed_session(&state, &format!("viewer{n}@x.test"), "viewer").await;
            let res = full_router(&state)
                .oneshot(get(uri, Some(&viewer)))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(body_json(res).await["error"], "forbidden");

            // Operator -> through to the handler.
            let op = seed_session(&state, &format!("op{n}@x.test"), "operator").await;
            let res = full_router(&state)
                .oneshot(get(uri, Some(&op)))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{uri}");
        }
    }
}
