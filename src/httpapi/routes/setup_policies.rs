//! `/setup/policies/*`: language / banned-tags / quality-profile validation
//! and persistence (admin only). Policies carry no secrets — stored as-is.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::httpapi::middleware::auth::Admin;
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};

const SETTING_KEY: &str = "setup.policies";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct QualityProfile {
    name: String,
    allowedQualities: Vec<String>,
    upgradeAllowed: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SetupPoliciesRequest {
    language: String,
    bannedTags: Vec<String>,
    qualityProfiles: Vec<QualityProfile>,
}

#[derive(Debug, Serialize)]
struct ValidateResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SaveResponse {
    saved: bool,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/setup/policies/validate", axum::routing::post(validate))
        .route("/setup/policies/save", axum::routing::post(save))
}

async fn validate(
    _user: Admin,
    State(state): State<AppState>,
    body: Result<axum::Json<SetupPoliciesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(axum::Json(req)) = body else {
        return crate::error::AppError::BadRequest("invalid_json".into()).into_response();
    };
    axum::Json(validate_policies(&state, &req)).into_response()
}

async fn save(
    user: Admin,
    State(state): State<AppState>,
    body: Result<axum::Json<SetupPoliciesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let actor = user.0.user.id.clone();
    let Ok(axum::Json(req)) = body else {
        return crate::error::AppError::BadRequest("invalid_json".into()).into_response();
    };

    let v = validate_policies(&state, &req);
    if !v.ok {
        return (StatusCode::BAD_REQUEST, axum::Json(v)).into_response();
    }

    let payload = match serde_json::to_string(&req) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "save_failed"})),
            )
                .into_response()
        }
    };
    if crate::storage::settings::upsert(&state.db, SETTING_KEY, &payload).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "save_failed"})),
        )
            .into_response();
    }

    let _ = insert_audit(
        &state.db,
        AuditEvent {
            action: "setup.policies_saved".into(),
            actor_user_id: Some(actor.clone()),
            ..Default::default()
        },
    )
    .await;
    state
        .hub
        .publish_event("audit.event", json!({"action": "setup.policies_saved", "actorUserId": actor}));
    axum::Json(SaveResponse { saved: true }).into_response()
}

fn validate_policies(state: &AppState, req: &SetupPoliciesRequest) -> ValidateResponse {
    state.hub.publish_event(
        "setup.policies.validation.started",
        json!({
            "qualityProfilesCount": req.qualityProfiles.len(),
            "bannedTagsCount": req.bannedTags.len(),
        }),
    );

    let mut errs: Vec<String> = Vec::new();

    if req.language.trim().is_empty() {
        errs.push("language is required".into());
    }
    if req.qualityProfiles.is_empty() {
        errs.push("at least one quality profile is required".into());
    }

    let mut seen = std::collections::HashSet::new();
    let mut profiles = req.qualityProfiles.clone();
    for p in profiles.iter_mut() {
        p.name = p.name.trim().to_string();
        if p.name.is_empty() {
            errs.push("qualityProfiles.name is required".into());
            continue;
        }
        if !seen.insert(p.name.to_ascii_lowercase()) {
            errs.push("qualityProfiles.name must be unique".into());
        }
        if p.allowedQualities.is_empty() {
            errs.push("qualityProfiles.allowedQualities must be non-empty".into());
        }
    }

    if req.bannedTags.iter().any(|t| t.trim().is_empty()) {
        errs.push("bannedTags must not include empty strings".into());
    }

    if !errs.is_empty() {
        state.hub.publish_event(
            "setup.policies.validation.error",
            json!({
                "error": "validation_failed",
                "errors": errs,
                "message": "Policy validation failed",
            }),
        );
        return ValidateResponse { ok: false, errors: errs };
    }

    state.hub.publish_event(
        "setup.policies.validation.completed",
        json!({
            "ok": true,
            "summary": {
                "language": req.language.trim(),
                "qualityProfiles": req.qualityProfiles.len(),
                "bannedTags": req.bannedTags.len(),
            },
        }),
    );
    ValidateResponse { ok: true, errors: Vec::new() }
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
            dir.path().join("policies_test.db").display()
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

    fn full_router(state: AppState) -> axum::Router {
        router()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state)
    }

    async fn post(
        state: AppState,
        uri: &str,
        body: &str,
        sess: Option<&crate::storage::sessions::Session>,
    ) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(s) = sess {
            b = b
                .header("cookie", format!("rivet_session={}", s.id))
                .header("x-csrf-token", &s.csrf_token);
        }
        let resp = full_router(state)
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    fn valid_body() -> String {
        serde_json::json!({
            "language": "en",
            "bannedTags": ["cam"],
            "qualityProfiles": [
                {"name": "HD", "allowedQualities": ["1080p", "720p"], "upgradeAllowed": true}
            ],
        })
        .to_string()
    }

    #[tokio::test]
    async fn validation_rejects_bad_policies() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        let bad = serde_json::json!({
            "language": "  ",
            "bannedTags": ["ok", "   "],
            "qualityProfiles": [
                {"name": "", "allowedQualities": [], "upgradeAllowed": false},
                {"name": "hd", "allowedQualities": ["720p"], "upgradeAllowed": false},
                {"name": "HD", "allowedQualities": ["720p"], "upgradeAllowed": false},
                {"name": "emptyq", "allowedQualities": [], "upgradeAllowed": false},
            ],
        })
        .to_string();

        let (s, v) = post(state.clone(), "/api/v1/setup/policies/validate", &bad, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK, "validate endpoint reports errors in-band");
        assert_eq!(v["ok"], false);
        let errors = v["errors"].as_array().unwrap();
        for expected in [
            "language is required",
            "bannedTags must not include empty strings",
            "qualityProfiles.name is required",
            "qualityProfiles.allowedQualities must be non-empty",
            "qualityProfiles.name must be unique",
        ] {
            assert!(
                errors.iter().any(|e| e.as_str() == Some(expected)),
                "missing error {expected} in {errors:?}"
            );
        }
    }

    #[tokio::test]
    async fn save_roundtrip_and_gate() {
        let (_dir, state) = app_state().await;
        let body = valid_body();

        // Anonymous => 401.
        let (s, v) = post(state.clone(), "/api/v1/setup/policies/save", &body, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "unauthorized");

        let sess = seed_admin_session(&state).await;
        let (s, v) = post(state.clone(), "/api/v1/setup/policies/save", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["saved"], true);

        let stored = crate::storage::settings::get(&state.db, "setup.policies")
            .await
            .unwrap()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed["language"], "en");
        assert_eq!(parsed["qualityProfiles"][0]["name"], "HD");

        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "setup.policies_saved"));

        // Invalid payload on save => 400 with the in-band errors.
        let (s, _) = post(
            state,
            "/api/v1/setup/policies/save",
            r#"{"language":"","bannedTags":[],"qualityProfiles":[]}"#,
            Some(&sess),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}
