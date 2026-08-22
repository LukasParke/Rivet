//! Calendar endpoints (*arr parity): episodes airing in a date range and the
//! monitored-but-missing backlog. Both require operator role or above.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Duration, NaiveDate, Utc};
use serde_json::json;

use crate::httpapi::middleware::auth::Operator;
use crate::state::AppState;
use crate::storage::calendar_queries::{self, EpisodeWithSeries};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/calendar", get(calendar_handler))
        .route("/calendar/missing", get(calendar_missing))
}

// ---------------------------------------------------------------------------
// Error plumbing: {"error": code} envelopes.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: String,
}

impl ApiError {
    fn bad_request(code: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: code.into(),
        }
    }
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
// GET /calendar?start&end
// ---------------------------------------------------------------------------

/// Parses an ISO `YYYY-MM-DD` query parameter; `None` means absent.
fn parse_date(raw: Option<&String>) -> Result<Option<NaiveDate>, ApiError> {
    match raw {
        None => Ok(None),
        Some(v) => NaiveDate::parse_from_str(v, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| ApiError::bad_request("invalid_date")),
    }
}

async fn calendar_handler(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<EpisodeWithSeries>>, ApiError> {
    let today = Utc::now().date_naive();
    let start = parse_date(params.get("start"))?;
    let end = parse_date(params.get("end"))?;
    // Default window: today ± 7 days, matching the *arr calendar default.
    let start = start.unwrap_or(today - Duration::days(7));
    let end = end.unwrap_or(today + Duration::days(7));

    let episodes = calendar_queries::list_episodes_in_range(
        &state.db,
        &start.format("%Y-%m-%d").to_string(),
        &end.format("%Y-%m-%d").to_string(),
    )
    .await?;
    Ok(Json(episodes))
}

/// Clamps an optional `limit` to 1..=100 with a default of 50.
fn clamp_limit(raw: Option<&String>) -> i64 {
    raw.and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 100)
}

// ---------------------------------------------------------------------------
// GET /calendar/missing
// ---------------------------------------------------------------------------

async fn calendar_missing(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<EpisodeWithSeries>>, ApiError> {
    let limit = clamp_limit(params.get("limit"));
    let episodes = calendar_queries::missing_episodes(&state.db, limit, 0).await?;
    Ok(Json(episodes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::security::secrets::SecretBox;
    use crate::storage::migrations;
    use crate::storage::sessions::Session;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::middleware;
    use std::sync::Arc;
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

    /// Creates a user + live session directly and returns its cookie header.
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
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn seed_series_and_episodes(state: &AppState) {
        sqlx::query(
            "INSERT INTO series (id, title, path, quality_profile, added_at, updated_at) \
             VALUES ('s1', 'Show', '/tv/Show', 'default', \
             '2026-01-01T00:00:00.000000000Z', '2026-01-01T00:00:00.000000000Z')",
        )
        .execute(&state.db)
        .await
        .unwrap();
        for (id, ep_no, air, monitored, has_file) in [
            ("in_range", 1, "2026-08-23T05:00:00Z", true, false),
            ("out_of_range", 2, "2020-01-05T05:00:00Z", true, false),
            ("aired_missing", 3, "2026-08-20T05:00:00Z", true, false),
            (
                "unmonitored_missing",
                4,
                "2026-08-19T05:00:00Z",
                false,
                false,
            ),
        ] {
            sqlx::query(
                "INSERT INTO episodes (id, series_id, season_number, episode_number, title, \
                 air_date_utc, monitored, has_file) VALUES (?, 's1', 1, ?, 'Ep', ?, ?, ?)",
            )
            .bind(id)
            .bind(ep_no)
            .bind(air)
            .bind(monitored)
            .bind(has_file)
            .execute(&state.db)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn calendar_filters_range_and_serializes_series_fields() {
        let (state, _dir) = test_state().await;
        seed_series_and_episodes(&state).await;
        let op = seed_session(&state, "range-op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(get(
                "/api/v1/calendar?start=2026-08-21&end=2026-08-28",
                Some(&op),
            ))
            .await
            .unwrap();
        let v = body_json(res).await;
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 1, "only in_range airs inside the window");
        assert_eq!(items[0]["id"], "in_range");
        assert_eq!(items[0]["seriesTitle"], "Show");
        assert_eq!(items[0]["seriesPath"], "/tv/Show");
        assert_eq!(items[0]["seasonNumber"], 1);

        // Bad date format -> 400 invalid_date.
        let res = full_router(&state)
            .oneshot(get("/api/v1/calendar?start=not-a-date", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_date");

        // Default window (today ± 7d) also contains the near-today episode
        // but never the 2020 one.
        let res = full_router(&state)
            .oneshot(get("/api/v1/calendar", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let items = body_json(res).await.as_array().unwrap().clone();
        assert!(
            items.iter().all(|e| e["id"] != "out_of_range"),
            "default range excludes far-past episode"
        );
        assert!(items.iter().any(|e| e["id"] == "in_range"));
    }

    #[tokio::test]
    async fn calendar_missing_returns_only_monitored_aired_without_file() {
        let (state, _dir) = test_state().await;
        seed_series_and_episodes(&state).await;
        let op = seed_session(&state, "missing-op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(get("/api/v1/calendar/missing", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let items = body_json(res).await.as_array().unwrap().clone();
        let ids: Vec<&str> = items.iter().map(|e| e["id"].as_str().unwrap()).collect();
        // Both are monitored + aired + without file; newest air date first.
        assert_eq!(ids, ["aired_missing", "out_of_range"]);
    }

    #[tokio::test]
    async fn auth_requires_operator_role() {
        let (state, _dir) = test_state().await;
        seed_series_and_episodes(&state).await;

        for (n, uri) in ["/api/v1/calendar", "/api/v1/calendar/missing"]
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
