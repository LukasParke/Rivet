//! Notification sink management endpoints (admin-only, mirroring the *arr
//! "Settings → Notifications" surface):
//!
//! - `GET    /notifications`          — list sinks
//! - `POST   /notifications`          — create a sink
//! - `PUT    /notifications/{id}`     — full-row update
//! - `DELETE /notifications/{id}`     — remove a sink
//! - `POST   /notifications/{id}/test` — send a synthetic event and report
//!   the per-sink delivery result honestly (`ok` / `error`), never masking
//!   an unreachable sink as success.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::httpapi::middleware::auth::Admin;
use crate::state::AppState;
use crate::storage::notifications::{self, NotificationSink};

/// The sink channels [`crate::notify`] knows how to deliver.
const KINDS: [&str; 3] = ["webhook", "ntfy", "discord"];

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/notifications",
            get(list_sinks).post(create_sink),
        )
        .route("/notifications/{id}", axum::routing::put(update_sink).delete(delete_sink))
        .route("/notifications/{id}/test", axum::routing::post(test_sink))
}

// ---------------------------------------------------------------------------
// Wire types (JSON key parity with the *arr-style surface)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct SinkWire {
    id: String,
    name: String,
    kind: String,
    config: Value,
    events: Vec<String>,
    enabled: bool,
    createdAt: String,
}

impl From<NotificationSink> for SinkWire {
    fn from(s: NotificationSink) -> Self {
        // Parse before moving the owned strings out of the sink.
        let config = s.config();
        let events = s.events();
        SinkWire {
            id: s.id,
            name: s.name,
            kind: s.kind,
            config,
            events,
            enabled: s.enabled,
            createdAt: s.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SinkRequest {
    name: String,
    kind: String,
    #[serde(default)]
    config: Value,
    /// Defaults to `["*"]` (subscribe to everything) when omitted.
    events: Option<Vec<String>>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

impl SinkRequest {
    fn validate(&self) -> Result<(), AppError> {
        if self.name.trim().is_empty() {
            return Err(AppError::BadRequest("name is required".into()));
        }
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(AppError::BadRequest(format!(
                "kind must be one of {}",
                KINDS.join(", ")
            )));
        }
        if !self.config.is_object() {
            return Err(AppError::BadRequest("config must be a JSON object".into()));
        }
        Ok(())
    }

    fn events(&self) -> Vec<String> {
        self.events
            .clone()
            .unwrap_or_else(|| vec!["*".to_string()])
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_sinks(
    Admin(_user): Admin,
    State(state): State<AppState>,
) -> Result<Json<Vec<SinkWire>>, AppError> {
    let sinks = notifications::list(&state.db).await?;
    Ok(Json(sinks.into_iter().map(SinkWire::from).collect()))
}

async fn create_sink(
    Admin(_user): Admin,
    State(state): State<AppState>,
    Json(req): Json<SinkRequest>,
) -> Result<Json<SinkWire>, AppError> {
    req.validate()?;
    let sink = notifications::create(
        &state.db,
        req.name.trim(),
        &req.kind,
        &req.config,
        &req.events(),
        req.enabled,
    )
    .await?;
    Ok(Json(sink.into()))
}

async fn update_sink(
    Admin(_user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SinkRequest>,
) -> Result<Json<SinkWire>, AppError> {
    req.validate()?;
    let sink = notifications::update(
        &state.db,
        id.trim(),
        req.name.trim(),
        &req.kind,
        &req.config,
        &req.events(),
        req.enabled,
    )
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(sink.into()))
}

async fn delete_sink(
    Admin(_user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    if !notifications::delete(&state.db, id.trim()).await? {
        return Err(AppError::NotFound);
    }
    Ok(Json(json!({ "deleted": id.trim() })))
}

async fn test_sink(
    Admin(_user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let sink = notifications::get(&state.db, id.trim())
        .await?
        .ok_or(AppError::NotFound)?;
    match crate::notify::probe_sink(&state, &sink).await {
        Ok(()) => Ok(Json(json!({ "id": sink.id, "ok": true, "error": null }))),
        Err(err) => Ok(Json(json!({ "id": sink.id, "ok": false, "error": err }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::security::secrets::SecretBox;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("notif_api.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&pool).await.unwrap();
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

    /// Full router with the same global stack the parent attaches:
    /// load_session -> csrf (setup_gate is a no-op once an admin exists and
    /// these tests seed users directly).
    fn app(state: AppState) -> axum::Router {
        router()
            .nest("/api/v1", router())
            .layer(axum::middleware::from_fn(csrf))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                load_session,
            ))
            .with_state(state)
    }

    async fn login(state: &AppState, role: &str, n: usize) -> (String, String) {
        let hash = crate::security::password::hash_password("correct-horse-battery")
            .unwrap();
        let email = format!("{role}{n}@notif.test");
        let user = crate::storage::users::create(&state.db, &email, &hash, "viewer")
            .await
            .unwrap();
        if role != "viewer" {
            sqlx::query("UPDATE users SET role = ? WHERE id = ?")
                .bind(role)
                .bind(&user.id)
                .execute(&state.db)
                .await
                .unwrap();
        }
        let sess = crate::storage::sessions::create(
            &state.db,
            &user.id,
            chrono::Duration::hours(1),
            None,
            None,
        )
        .await
        .unwrap();
        let csrf = crate::storage::sessions::get(&state.db, &sess.id)
            .await
            .unwrap()
            .unwrap()
            .csrf_token;
        (sess.id, csrf)
    }

    fn req(
        method: &str,
        uri: &str,
        sid: Option<(&str, &str)>,
        body: Option<String>,
    ) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        if let Some((sid, csrf)) = sid {
            b = b
                .header("cookie", format!("rivet_session={sid}"))
                .header("x-csrf-token", csrf);
        }
        b.body(Body::from(body.unwrap_or_default())).unwrap()
    }

    async fn sink_json_body(url: &str, events: Value) -> String {
        json!({
            "name": "ops",
            "kind": "webhook",
            "config": {"url": url},
            "events": events,
            "enabled": true
        })
        .to_string()
    }

    #[tokio::test]
    async fn admin_gating_matrix() {
        let (state, _dir) = test_state().await;
        let (admin, admin_csrf) = login(&state, "admin", 0).await;
        let (op, op_csrf) = login(&state, "operator", 0).await;
        let (viewer, viewer_csrf) = login(&state, "viewer", 0).await;

        // Seed one sink as admin so GET has something to return.
        let res = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/v1/notifications",
                Some((&admin, &admin_csrf)),
                Some(sink_json_body("http://127.0.0.1:1/hook", json!(["*"])).await),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // (method, path, role session) -> expected status. Every endpoint is
        // admin-only: viewer and operator are forbidden everywhere, and
        // anonymous requests are unauthorized.
        let matrix = [
            ("GET", "/api/v1/notifications", "admin", StatusCode::OK),
            ("GET", "/api/v1/notifications", "operator", StatusCode::FORBIDDEN),
            ("POST", "/api/v1/notifications", "admin", StatusCode::OK),
            ("POST", "/api/v1/notifications", "operator", StatusCode::FORBIDDEN),
            ("POST", "/api/v1/notifications", "viewer", StatusCode::FORBIDDEN),
            ("PUT", "/api/v1/notifications/x", "admin", StatusCode::NOT_FOUND),
            ("PUT", "/api/v1/notifications/x", "operator", StatusCode::FORBIDDEN),
            ("PUT", "/api/v1/notifications/x", "viewer", StatusCode::FORBIDDEN),
            ("DELETE", "/api/v1/notifications/x", "admin", StatusCode::NOT_FOUND),
            ("DELETE", "/api/v1/notifications/x", "operator", StatusCode::FORBIDDEN),
            ("DELETE", "/api/v1/notifications/x", "viewer", StatusCode::FORBIDDEN),
            ("POST", "/api/v1/notifications/x/test", "admin", StatusCode::NOT_FOUND),
            ("POST", "/api/v1/notifications/x/test", "operator", StatusCode::FORBIDDEN),
            ("POST", "/api/v1/notifications/x/test", "viewer", StatusCode::FORBIDDEN),
        ];
        let sink_body = json!({
            "name": "m",
            "kind": "webhook",
            "config": {"url": "http://127.0.0.1:9/h"},
            "events": ["*"],
            "enabled": true
        })
        .to_string();
        let sessions: Vec<(&str, &str, &str)> = vec![
            ("admin", &admin, &admin_csrf),
            ("operator", &op, &op_csrf),
            ("viewer", &viewer, &viewer_csrf),
        ];
        for (method_name, path, role, expected) in matrix {
            let (_, sid, csrf) =
                sessions.iter().find(|s| s.0 == role).unwrap();
            // Allowed mutating rows carry a valid sink body; denied rows are
            // rejected by the Admin extractor before the body is read.
            let body = match (method_name, role) {
                ("POST" | "PUT", "admin") => Some(sink_body.clone()),
                _ => Some("{}".to_string()),
            };
            let res = app(state.clone())
                .oneshot(req(method_name, path, Some((sid, csrf)), body))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                expected,
                "{method_name} {path} as {role}"
            );
        }

        // Anonymous (no session): unauthorized on every endpoint.
        for (method_name, path) in [
            ("GET", "/api/v1/notifications"),
            ("POST", "/api/v1/notifications"),
            ("PUT", "/api/v1/notifications/x"),
            ("DELETE", "/api/v1/notifications/x"),
            ("POST", "/api/v1/notifications/x/test"),
        ] {
            let res = app(state.clone())
                .oneshot(req(method_name, path, None, None))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method_name} {path}");
        }
    }

    #[tokio::test]
    async fn create_list_update_delete_roundtrip() {
        let (state, _dir) = test_state().await;
        let (sid, csrf) = login(&state, "admin", 1).await;
        let a = app(state.clone());

        let res = a
            .oneshot(req(
                "POST",
                "/api/v1/notifications",
                Some((&sid, &csrf)),
                Some(
                    json!({
                        "name": "hook",
                        "kind": "webhook",
                        "config": {"url": "https://hooks.example.test/x"},
                        "events": ["acquisition.imported"],
                        "enabled": true
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(created["kind"], "webhook");
        assert_eq!(created["config"]["url"], "https://hooks.example.test/x");
        assert_eq!(created["events"][0], "acquisition.imported");
        let id = created["id"].as_str().unwrap().to_string();

        // Validation: bad kind and bad config shape are 400.
        for bad in [
            json!({"name": "n", "kind": "sms", "config": {}}),
            json!({"name": "n", "kind": "webhook", "config": "https://x"}),
            json!({"name": "  ", "kind": "webhook", "config": {}}),
        ] {
            let res = app(state.clone())
                .oneshot(req(
                    "POST",
                    "/api/v1/notifications",
                    Some((&sid, &csrf)),
                    Some(bad.to_string()),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }

        // Update flips enabled and events; missing events defaults to ["*"].
        let res = app(state.clone())
            .oneshot(req(
                "PUT",
                &format!("/api/v1/notifications/{id}"),
                Some((&sid, &csrf)),
                Some(
                    json!({
                        "name": "hook2",
                        "kind": "discord",
                        "config": {"webhookUrl": "https://discord.example.test/w"},
                        "enabled": false
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let updated: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(updated["name"], "hook2");
        assert_eq!(updated["kind"], "discord");
        assert_eq!(updated["enabled"], false);
        assert_eq!(updated["events"], json!(["*"]));

        // Update of a missing id is 404.
        let res = app(state.clone())
            .oneshot(req(
                "PUT",
                "/api/v1/notifications/missing",
                Some((&sid, &csrf)),
                Some(
                    json!({"name": "n", "kind": "webhook", "config": {}})
                        .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Delete then 404 on repeat.
        let res = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/notifications/{id}"),
                Some((&sid, &csrf)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/notifications/{id}"),
                Some((&sid, &csrf)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = app(state.clone())
            .oneshot(req(
                "GET",
                "/api/v1/notifications",
                Some((&sid, &csrf)),
                None,
            ))
            .await
            .unwrap();
        let list: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(list.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_endpoint_reports_unreachable_sink_honestly() {
        let (state, _dir) = test_state().await;
        let (sid, csrf) = login(&state, "admin", 2).await;

        // Nothing listens on port 9 (discard); dev policy permits loopback so
        // the request fails fast with a connection error.
        let res = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/v1/notifications",
                Some((&sid, &csrf)),
                Some(
                    json!({
                        "name": "dead",
                        "kind": "webhook",
                        "config": {"url": "http://127.0.0.1:9/hook"},
                        "enabled": true
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let res = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/v1/notifications/{id}/test"),
                Some((&sid, &csrf)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["id"], id.as_str());
        assert_eq!(body["ok"], false, "unreachable sink must not report ok");
        let err = body["error"].as_str().unwrap();
        assert!(!err.is_empty(), "error reason must be surfaced: {body}");

        // Unknown sink id: 404, not a fake success.
        let res = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/v1/notifications/missing/test",
                Some((&sid, &csrf)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // And a healthy sink reports ok:true with null error.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let res = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/v1/notifications",
                Some((&sid, &csrf)),
                Some(
                    json!({
                        "name": "live",
                        "kind": "webhook",
                        "config": {"url": server.uri() + "/hook"},
                        "enabled": true
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        let live_id = created["id"].as_str().unwrap().to_string();
        let res = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/v1/notifications/{live_id}/test"),
                Some((&sid, &csrf)),
                None,
            ))
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["ok"], true, "{body}");
        assert!(body["error"].is_null());
    }
}
