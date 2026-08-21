//! `/setup/finish`: closes the wizard by snapshotting which settings sections
//! exist, closing registration by default and emitting the audit event.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

use crate::httpapi::middleware::auth::Admin;
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};

const REPORT_KEY: &str = "setup.report.latest";
const SECTIONS: [&str; 5] = [
    "setup.paths",
    "setup.clients",
    "setup.indexers",
    "setup.metadata",
    "setup.policies",
];

#[derive(Debug, Serialize)]
struct SetupFinishResponse {
    ok: bool,
    reportKey: String,
    summary: BTreeMap<String, String>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/setup/finish", axum::routing::post(finish))
}

async fn finish(user: Admin, State(state): State<AppState>) -> Response {
    let actor = user.0.user.id.clone();

    // Basic setup report snapshot.
    let mut summary = BTreeMap::new();
    summary.insert("finishedAt".into(), crate::timeutil::fmt(chrono::Utc::now()));
    for k in SECTIONS {
        match crate::storage::settings::get(&state.db, k).await {
            Ok(Some(_)) => {
                summary.insert(k.to_string(), "set".into());
            }
            Ok(None) => {
                summary.insert(k.to_string(), "missing".into());
            }
            Err(_) => {
                summary.insert(k.to_string(), "error".into());
            }
        }
    }

    let payload = match serde_json::to_string(&summary) {
        Ok(p) => p,
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "save_failed"})),
            )
                .into_response()
        }
    };
    if crate::storage::settings::upsert(&state.db, REPORT_KEY, &payload).await.is_err() {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "save_failed"})),
        )
            .into_response();
    }

    // Close registration by default (a future invites feature can flip it).
    let _ =
        crate::storage::settings::upsert(&state.db, "auth.registration_open", "false").await;

    let _ = insert_audit(
        &state.db,
        AuditEvent {
            action: "setup.finished".into(),
            actor_user_id: Some(actor.clone()),
            ..Default::default()
        },
    )
    .await;
    state.hub.publish_event(
        "audit.event",
        json!({"action": "setup.finished", "actorUserId": actor, "reportKey": REPORT_KEY}),
    );

    axum::Json(SetupFinishResponse { ok: true, reportKey: REPORT_KEY.into(), summary })
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::metrics::Metrics;
    use crate::security::secrets::SecretBox;
    use crate::security::OutboundPolicy;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware;
    use tower::ServiceExt;

    async fn app_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("finish_test.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&db).await.unwrap();
        let config = Arc::new(crate::config::Config {
            env: "test".into(),
            http_addr: String::new(),
            database_url: String::new(),
            dry_run: false,
            dry_run_fixture_path: None,
            ffprobe_path: None,
            secret_key_base64: None,
            session_cookie_name: "rivet_session".into(),
            session_ttl: std::time::Duration::from_secs(30 * 24 * 3600),
            allow_internal_urls: true,
        });
        (
            dir,
            AppState {
                db,
                secret_box: Arc::new(SecretBox::new(None).unwrap()),
                policy: OutboundPolicy::dev(),
                hub: Arc::new(Hub::new()),
                metrics: Arc::new(Metrics::default()),
                config,
            },
        )
    }

    async fn seed_admin_session(state: &AppState) -> crate::storage::sessions::Session {
        let hash = crate::security::password::hash_password("long-enough-password").unwrap();
        let u = crate::storage::users::create(&state.db, "admin@x.test", &hash, "admin")
            .await
            .unwrap();
        crate::storage::sessions::create(&state.db, &u.id, chrono::Duration::hours(1), None, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn finish_writes_report_and_closes_registration() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        // Two sections present, one absent.
        crate::storage::settings::upsert(&state.db, "setup.paths", "{}").await.unwrap();
        crate::storage::settings::upsert(&state.db, "setup.policies", "{}").await.unwrap();

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/setup/finish")
            .header("cookie", format!("rivet_session={}", sess.id))
            .header("x-csrf-token", &sess.csrf_token)
            .body(Body::empty())
            .unwrap();

        let app = axum::Router::new()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["reportKey"], "setup.report.latest");
        assert_eq!(v["summary"]["setup.paths"], "set");
        assert_eq!(v["summary"]["setup.policies"], "set");
        assert_eq!(v["summary"]["setup.clients"], "missing");
        assert!(v["summary"]["finishedAt"].is_string());

        let stored = crate::storage::settings::get(&state.db, REPORT_KEY)
            .await
            .unwrap()
            .unwrap();
        assert!(stored.contains("\"setup.clients\":\"missing\""));

        let reg = crate::storage::settings::get(&state.db, "auth.registration_open")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reg, "false");

        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "setup.finished"));
    }

    #[tokio::test]
    async fn finish_requires_admin() {
        let (_dir, state) = app_state().await;
        let resp = axum::Router::new()
            .layer(middleware::from_fn(csrf))
            .nest("/api/v1", router())
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/setup/finish")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
