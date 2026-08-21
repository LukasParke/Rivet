//! Liveness and readiness probes (`/healthz`, `/readyz`), wire-compatible
//! with the original Go handlers.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::AppState;

/// Hard budget for the readiness database ping (matches the Go handler).
const READY_TIMEOUT: Duration = Duration::from_secs(2);

/// GET /healthz — liveness only; never touches the database.
pub async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

/// GET /readyz — pings the database; 503 `db_unready` when it does not answer
/// within the timeout.
pub async fn readyz(State(state): State<AppState>) -> Response {
    let ping = tokio::time::timeout(
        READY_TIMEOUT,
        sqlx::query("SELECT 1").execute(&state.db),
    )
    .await;
    match ping {
        Ok(Ok(_)) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        // Timeout (Err), or the ping itself failed.
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": "db_unready" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Role};
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn test_state(pool: sqlx::SqlitePool) -> AppState {
        AppState {
            db: pool,
            secret_box: std::sync::Arc::new(
                crate::security::secrets::SecretBox::new(None).unwrap(),
            ),
            policy: crate::security::OutboundPolicy::dev(),
            hub: std::sync::Arc::new(Hub::new()),
            metrics: std::sync::Arc::new(crate::metrics::Metrics::default()),
            config: std::sync::Arc::new(crate::config::Config {
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
        }
    }

    /// Returns the TempDir alongside the pool: dropping it deletes the DB
    /// file, so callers must bind it for the pool's lifetime.
    async fn db_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        crate::storage::migrations::apply(&pool).await.unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn healthz_is_ok_without_db() {
        let (_dir, pool) = db_pool().await;
        let state = test_state(pool);
        let app = Router::new()
            .route("/healthz", get(healthz))
            .with_state(state);
        let res = app.oneshot(Request::get("/healthz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn readyz_ok_when_db_up() {
        let (_dir, pool) = db_pool().await;
        let state = test_state(pool);
        let app = Router::new()
            .route("/readyz", get(readyz))
            .with_state(state);
        let res = app.oneshot(Request::get("/readyz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn readyz_reports_db_unready_on_broken_pool() {
        // A pool pointing at a closed/corrupt database makes the ping fail.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("gone.db").display()))
            .await
            .unwrap();
        pool.close().await;
        let state = test_state(pool);
        let app = Router::new()
            .route("/readyz", get(readyz))
            .with_state(state);
        let res = app.oneshot(Request::get("/readyz").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"ok": false, "error": "db_unready"})
        );
    }

    // Silence unused-import warning for Role in case state shape changes.
    const _: Option<Role> = None;
}
