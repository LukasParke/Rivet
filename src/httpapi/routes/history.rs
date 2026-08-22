//! Activity history endpoints (*arr parity): paginated `history_events` with
//! optional event-type / series / movie filters. Operator role or above.
//!
//! Queries run inline against the 0011 schema (`history_events`); the raw
//! `data_json` payload is exposed both verbatim (`data`) and parsed
//! (`dataJson`, null when absent or not valid JSON).

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::error::ApiResult;
use crate::httpapi::middleware::auth::Operator;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/history", get(history))
}

// ---------------------------------------------------------------------------
// Pagination + wire envelope (same *arr defaults as /wanted)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Page<T> {
    items: Vec<T>,
    total: i64,
    page: i64,
    #[serde(rename = "pageSize")]
    page_size: i64,
}

/// Parses `page` (1-based) and `pageSize` (1..=100), clamping instead of erroring.
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

/// Raw row shape of `history_events`.
#[derive(Debug, sqlx::FromRow)]
struct HistoryRow {
    id: String,
    event_type: String,
    series_id: Option<String>,
    movie_id: Option<String>,
    acquisition_id: Option<String>,
    source_title: String,
    quality: Option<String>,
    quality_score: Option<i64>,
    data_json: Option<String>,
    created_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryItem {
    id: String,
    event_type: String,
    source_title: String,
    quality: Option<String>,
    quality_score: Option<i64>,
    /// Verbatim `data_json` column.
    data: Option<String>,
    /// Parsed `data_json`; null when absent or invalid JSON.
    data_json: Option<serde_json::Value>,
    created_at: String,
    series_id: Option<String>,
    movie_id: Option<String>,
    acquisition_id: Option<String>,
}

impl From<HistoryRow> for HistoryItem {
    fn from(r: HistoryRow) -> Self {
        let data_json = r
            .data_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok());
        Self {
            id: r.id,
            event_type: r.event_type,
            source_title: r.source_title,
            quality: r.quality,
            quality_score: r.quality_score,
            data: r.data_json,
            data_json,
            created_at: r.created_at,
            series_id: r.series_id,
            movie_id: r.movie_id,
            acquisition_id: r.acquisition_id,
        }
    }
}

const SELECT_COLUMNS: &str = "id, event_type, series_id, movie_id, acquisition_id, source_title, \
     quality, quality_score, data_json, created_at";

/// Filters are passed as nullable binds so the statement stays static:
/// `(NULL IS NULL OR event_type = NULL)` is always true in SQLite.
async fn query_rows(
    db: &sqlx::SqlitePool,
    event_type: Option<&str>,
    series_id: Option<&str>,
    movie_id: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<HistoryRow>, sqlx::Error> {
    sqlx::query_as::<_, HistoryRow>(&format!(
        "SELECT {SELECT_COLUMNS} FROM history_events \
         WHERE (?1 IS NULL OR event_type = ?1) \
           AND (?2 IS NULL OR series_id = ?2) \
           AND (?3 IS NULL OR movie_id = ?3) \
         ORDER BY created_at DESC, id DESC LIMIT ?4 OFFSET ?5"
    ))
    .bind(event_type)
    .bind(series_id)
    .bind(movie_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

async fn count_rows(
    db: &sqlx::SqlitePool,
    event_type: Option<&str>,
    series_id: Option<&str>,
    movie_id: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM history_events \
         WHERE (?1 IS NULL OR event_type = ?1) \
           AND (?2 IS NULL OR series_id = ?2) \
           AND (?3 IS NULL OR movie_id = ?3)",
    )
    .bind(event_type)
    .bind(series_id)
    .bind(movie_id)
    .fetch_one(db)
    .await
}

// ---------------------------------------------------------------------------
// GET /history?page&pageSize&eventType&seriesId&movieId
// ---------------------------------------------------------------------------

async fn history(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Page<HistoryItem>>> {
    let (page, page_size) = page_params(&params);
    // Empty filter values count as "no filter" rather than matching nothing.
    let event_type = params
        .get("eventType")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty());
    let series_id = params
        .get("seriesId")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty());
    let movie_id = params
        .get("movieId")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty());

    let total = count_rows(&state.db, event_type, series_id, movie_id).await?;
    let offset = (page - 1).saturating_mul(page_size);
    let rows = query_rows(
        &state.db, event_type, series_id, movie_id, page_size, offset,
    )
    .await?;
    Ok(Json(Page {
        items: rows.into_iter().map(Into::into).collect(),
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

    /// One history event to insert; `created_at` is caller-controlled so
    /// ordering assertions are deterministic.
    struct EventSeed<'a> {
        id: &'a str,
        event_type: &'a str,
        series_id: Option<&'a str>,
        movie_id: Option<&'a str>,
        acquisition_id: Option<&'a str>,
        created_at: &'a str,
        data_json: Option<&'a str>,
    }

    /// Inserts one event against series `s1` with a canned payload.
    async fn seed_event(state: &AppState, id: &str, event_type: &str, created_at: &str) {
        seed_event_full(
            state,
            EventSeed {
                id,
                event_type,
                series_id: Some("s1"),
                movie_id: None,
                acquisition_id: None,
                created_at,
                data_json: Some(r#"{"indexer":"nzbin"}"#),
            },
        )
        .await;
    }

    async fn seed_event_full(state: &AppState, seed: EventSeed<'_>) {
        sqlx::query(
            "INSERT INTO history_events (id, event_type, series_id, episode_id, movie_id, \
             acquisition_id, source_title, quality, quality_score, data_json, created_at) \
             VALUES (?, ?, ?, NULL, ?, ?, 'Release.Name-GRP', 'WEBDL-1080p', 100, ?, ?)",
        )
        .bind(seed.id)
        .bind(seed.event_type)
        .bind(seed.series_id)
        .bind(seed.movie_id)
        .bind(seed.acquisition_id)
        .bind(seed.data_json)
        .bind(seed.created_at)
        .execute(&state.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn history_is_paginated_newest_first() {
        let (state, _dir) = test_state().await;
        for i in 0..3 {
            seed_event(
                &state,
                &format!("h{i}"),
                "grabbed",
                &format!("2026-08-{:02}T00:00:00Z", 10 + i),
            )
            .await;
        }
        let op = seed_session(&state, "op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(get("/api/v1/history?page=1&pageSize=2", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["total"], 3);
        assert_eq!(v["page"], 1);
        assert_eq!(v["pageSize"], 2);
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], "h2", "newest first");
        assert_eq!(items[1]["id"], "h1");

        // Second page carries the remainder.
        let res = full_router(&state)
            .oneshot(get("/api/v1/history?page=2&pageSize=2", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["items"].as_array().unwrap()[0]["id"], "h0");

        // Out-of-range params clamp instead of erroring.
        let res = full_router(&state)
            .oneshot(get("/api/v1/history?page=0&pageSize=999", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["page"], 1);
        assert_eq!(v["pageSize"], 100);
    }

    #[tokio::test]
    async fn history_item_shape_includes_parsed_data_json() {
        let (state, _dir) = test_state().await;
        seed_event_full(
            &state,
            EventSeed {
                id: "h-ok",
                event_type: "imported",
                series_id: Some("s1"),
                movie_id: None,
                acquisition_id: Some("a1"),
                created_at: "2026-08-20T00:00:00Z",
                data_json: Some(r#"{"path":"/tv/a.mkv","n":3}"#),
            },
        )
        .await;
        seed_event_full(
            &state,
            EventSeed {
                id: "h-bad",
                event_type: "grabbed",
                series_id: None,
                movie_id: Some("m1"),
                acquisition_id: None,
                created_at: "2026-08-19T00:00:00Z",
                data_json: Some("not json"),
            },
        )
        .await;
        seed_event_full(
            &state,
            EventSeed {
                id: "h-null",
                event_type: "failed",
                series_id: None,
                movie_id: None,
                acquisition_id: None,
                created_at: "2026-08-18T00:00:00Z",
                data_json: None,
            },
        )
        .await;
        let op = seed_session(&state, "op@x.test", "operator").await;

        let res = full_router(&state)
            .oneshot(get("/api/v1/history", Some(&op)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["total"], 3);
        let by_id = |id: &str| {
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|it| it["id"] == id)
                .unwrap()
                .clone()
        };

        let ok = by_id("h-ok");
        assert_eq!(ok["eventType"], "imported");
        assert_eq!(ok["sourceTitle"], "Release.Name-GRP");
        assert_eq!(ok["quality"], "WEBDL-1080p");
        assert_eq!(ok["qualityScore"], 100);
        assert_eq!(ok["seriesId"], "s1");
        assert_eq!(ok["acquisitionId"], "a1");
        assert_eq!(ok["createdAt"], "2026-08-20T00:00:00Z");
        assert_eq!(ok["data"], r#"{"path":"/tv/a.mkv","n":3}"#);
        assert_eq!(ok["dataJson"]["path"], "/tv/a.mkv");
        assert_eq!(ok["dataJson"]["n"], 3);

        // Unparsable and absent payloads stay null on the parsed field only.
        assert_eq!(by_id("h-bad")["dataJson"], serde_json::Value::Null);
        assert_eq!(by_id("h-bad")["data"], "not json");
        assert_eq!(by_id("h-null")["dataJson"], serde_json::Value::Null);
        assert_eq!(by_id("h-null")["data"], serde_json::Value::Null);
        assert_eq!(by_id("h-null")["movieId"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn history_filters_by_type_series_and_movie() {
        let (state, _dir) = test_state().await;
        for (id, event_type, sid, created) in [
            ("g1", "grabbed", Some("s1"), "2026-08-20T00:00:00Z"),
            ("i1", "imported", Some("s1"), "2026-08-21T00:00:00Z"),
            ("g2", "grabbed", Some("s2"), "2026-08-22T00:00:00Z"),
        ] {
            seed_event_full(
                &state,
                EventSeed {
                    id,
                    event_type,
                    series_id: sid,
                    movie_id: None,
                    acquisition_id: None,
                    created_at: created,
                    data_json: None,
                },
            )
            .await;
        }
        seed_event_full(
            &state,
            EventSeed {
                id: "mg",
                event_type: "grabbed",
                series_id: None,
                movie_id: Some("m1"),
                acquisition_id: None,
                created_at: "2026-08-23T00:00:00Z",
                data_json: None,
            },
        )
        .await;
        let op = seed_session(&state, "op@x.test", "operator").await;

        // eventType filter.
        let res = full_router(&state)
            .oneshot(get("/api/v1/history?eventType=grabbed", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["total"], 3);
        for it in v["items"].as_array().unwrap() {
            assert_eq!(it["eventType"], "grabbed");
        }

        // seriesId filter (combined with eventType).
        let res = full_router(&state)
            .oneshot(get(
                "/api/v1/history?eventType=grabbed&seriesId=s1",
                Some(&op),
            ))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["total"], 1);
        assert_eq!(v["items"][0]["id"], "g1");

        // movieId filter.
        let res = full_router(&state)
            .oneshot(get("/api/v1/history?movieId=m1", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["total"], 1);
        assert_eq!(v["items"][0]["id"], "mg");

        // Non-matching filter combination: honest empty page, not an error.
        let res = full_router(&state)
            .oneshot(get("/api/v1/history?seriesId=nope", Some(&op)))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["total"], 0);
        assert!(v["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn auth_requires_operator_role() {
        let (state, _dir) = test_state().await;
        seed_event(&state, "h1", "grabbed", "2026-08-20T00:00:00Z").await;

        // Anonymous -> 401 unauthorized.
        let res = full_router(&state)
            .oneshot(get("/api/v1/history", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_json(res).await["error"], "unauthorized");

        // Viewer -> 403 forbidden.
        let viewer = seed_session(&state, "viewer@x.test", "viewer").await;
        let res = full_router(&state)
            .oneshot(get("/api/v1/history", Some(&viewer)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(res).await["error"], "forbidden");

        // Operator and admin pass.
        for (email, role) in [("op@x.test", "operator"), ("admin@x.test", "admin")] {
            let s = seed_session(&state, email, role).await;
            let res = full_router(&state)
                .oneshot(get("/api/v1/history", Some(&s)))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{role}");
        }
    }
}
