//! `/setup/metadata/*`: TMDB/TVDB provider validation + persistence.
//!
//! The Go original delegated to the adapter constructors; here the wizard
//! performs the equivalent probe directly through [`crate::security::ssrf`]
//! (TMDB: GET /3/configuration with the API key; TVDB v4: POST /v4/login),
//! keeping the same error codes and hub events. Secrets are encrypted before
//! persisting.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::httpapi::middleware::auth::Admin;
use crate::security::ssrf::{self, SsrfError};
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};

const SETTING_KEY: &str = "setup.metadata";
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
const MAX_BODY: usize = 1024 * 1024;

/// Fallback TMDB API key (rate-limited, shared) for the out-of-box experience,
/// matching the Go adapter constant.
const FALLBACK_TMDB_API_KEY: &str = "c9856d0cb57c3f14bf75bdc6c063b8f3";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TMDBConfig {
    #[serde(default)]
    baseUrl: String,
    #[serde(default)]
    apiKey: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TVDBConfig {
    #[serde(default)]
    baseUrl: String,
    #[serde(default)]
    apiKey: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SetupMetadataValidateRequest {
    #[serde(default)]
    tmdb: Option<TMDBConfig>,
    #[serde(default)]
    tvdb: Option<TVDBConfig>,
}

#[derive(Debug, Default, Serialize)]
struct ProviderTestResult {
    ok: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    details: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ProviderTestResult {
    fn fail(code: &str) -> Self {
        ProviderTestResult { ok: false, error: Some(code.to_string()), ..Default::default() }
    }

    fn ok_with(details: BTreeMap<String, String>) -> Self {
        ProviderTestResult { ok: true, details, error: None }
    }
}

#[derive(Debug, Default, Serialize)]
struct SetupMetadataValidateResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tmdb: Option<ProviderTestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tvdb: Option<ProviderTestResult>,
}

#[derive(Debug, Serialize)]
struct SetupMetadataSaveResponse {
    saved: bool,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/setup/metadata/validate", axum::routing::post(validate))
        .route("/setup/metadata/save", axum::routing::post(save))
}

async fn validate(
    _user: Admin,
    State(state): State<AppState>,
    body: Result<
        axum::Json<SetupMetadataValidateRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let resp = validate_metadata(&state, &req).await;
    axum::Json(resp).into_response()
}

async fn save(
    user: Admin,
    State(state): State<AppState>,
    body: Result<
        axum::Json<SetupMetadataValidateRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let actor = user.0.user.id.clone();
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };

    let resp = validate_metadata(&state, &req).await;
    if !resp.ok {
        return (StatusCode::BAD_REQUEST, axum::Json(resp)).into_response();
    }

    // Encrypt API keys BEFORE persisting; an encryption failure fails the save
    // rather than storing plaintext or "" (the Go original stored "" silently).
    let mut persist = serde_json::Map::new();
    if let Some(tmdb) = &req.tmdb {
        let key_enc = match state
            .secret_box
            .encrypt_to_string(tmdb.apiKey.trim().as_bytes())
        {
            Ok(e) => e,
            Err(_) => return save_failed_response(),
        };
        persist.insert(
            "tmdb".into(),
            json!({"baseUrl": tmdb.baseUrl.trim(), "apiKeyEnc": key_enc}),
        );
    }
    if let Some(tvdb) = &req.tvdb {
        let key_enc = match state
            .secret_box
            .encrypt_to_string(tvdb.apiKey.trim().as_bytes())
        {
            Ok(e) => e,
            Err(_) => return save_failed_response(),
        };
        persist.insert(
            "tvdb".into(),
            json!({"baseUrl": tvdb.baseUrl.trim(), "apiKeyEnc": key_enc}),
        );
    }

    let payload = serde_json::Value::Object(persist).to_string();
    if crate::storage::settings::upsert(&state.db, SETTING_KEY, &payload)
        .await
        .is_err()
    {
        return save_failed_response();
    }

    let _ = insert_audit(
        &state.db,
        AuditEvent {
            action: "setup.metadata_saved".into(),
            actor_user_id: Some(actor.clone()),
            ..Default::default()
        },
    )
    .await;
    state.hub.publish_event(
        "audit.event",
        json!({"action": "setup.metadata_saved", "actorUserId": actor}),
    );
    axum::Json(SetupMetadataSaveResponse { saved: true }).into_response()
}

fn save_failed_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"error": "save_failed"})),
    )
        .into_response()
}

fn base_origin(raw: &str, default_base: &str) -> Result<url::Url, SsrfError> {
    let raw = raw.trim();
    let raw = if raw.is_empty() { default_base } else { raw };
    let mut u =
        url::Url::parse(raw).map_err(|_| SsrfError::BadScheme)?;
    u.set_path("");
    u.set_query(None);
    u.set_fragment(None);
    Ok(u)
}

fn join_url(origin: &url::Url, path: &str, query: Option<&str>) -> String {
    let mut u = origin.clone();
    u.set_path(path);
    u.set_query(query);
    u.set_fragment(None);
    u.to_string()
}

async fn body_limited(res: reqwest::Response, max: usize) -> Result<Vec<u8>, SsrfError> {
    let mut res = res;
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|_| SsrfError::Network)? {
        if out.len() + chunk.len() > max {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Cheap auth check against /3/configuration; returns the images base URL.
async fn test_tmdb(state: &AppState, cfg: &TMDBConfig) -> ProviderTestResult {
    let api_key_raw = cfg.apiKey.trim().to_string();
    let using_fallback = api_key_raw.is_empty();
    let api_key =
        if using_fallback { FALLBACK_TMDB_API_KEY.to_string() } else { api_key_raw };

    let origin = match base_origin(&cfg.baseUrl, "https://api.themoviedb.org") {
        Ok(o) => o,
        Err(e) => return ProviderTestResult::fail(&e.to_string()),
    };
    let url = join_url(&origin, "/3/configuration", Some(&format!("api_key={}", api_key)));

    match ssrf::get_validated(&url, state.policy, PROBE_TIMEOUT, 3).await {
        Err(_) => ProviderTestResult::fail("connect_failed"),
        Ok(res) => {
            let status = res.status();
            let body = body_limited(res, MAX_BODY).await.unwrap_or_default();
            if status == 401 || status == 403 {
                return ProviderTestResult::fail("invalid_api_key");
            }
            if status != 200 {
                return ProviderTestResult::fail("http_error");
            }
            #[derive(Deserialize)]
            struct ConfigResp {
                images: Images,
            }
            #[derive(Deserialize)]
            struct Images {
                #[serde(rename = "base_url")]
                base_url: String,
            }
            match serde_json::from_slice::<ConfigResp>(&body) {
                Ok(parsed) if !parsed.images.base_url.is_empty() => {
                    let mut details = BTreeMap::new();
                    details.insert("imagesBaseUrl".into(), parsed.images.base_url);
                    if using_fallback {
                        details.insert(
                            "warning".into(),
                            "Using fallback API key (rate-limited). Get your own free key at https://www.themoviedb.org/settings/api".into(),
                        );
                    }
                    ProviderTestResult::ok_with(details)
                }
                Ok(_) => ProviderTestResult::fail("unexpected_response"),
                Err(_) => ProviderTestResult::fail("parse_failed"),
            }
        }
    }
}

/// TVDB v4 requires a token from POST /v4/login.
async fn test_tvdb(state: &AppState, cfg: &TVDBConfig) -> ProviderTestResult {
    let api_key = cfg.apiKey.trim();
    if api_key.is_empty() {
        return ProviderTestResult::fail("api key required");
    }
    let origin = match base_origin(&cfg.baseUrl, "https://api4.thetvdb.com") {
        Ok(o) => o,
        Err(e) => return ProviderTestResult::fail(&e.to_string()),
    };
    let url = join_url(&origin, "/v4/login", None);
    let payload = json!({"apikey": api_key}).to_string();

    match ssrf::request_validated(
        reqwest::Method::POST,
        &url,
        state.policy,
        PROBE_TIMEOUT,
        &[("content-type", "application/json")],
        Some(payload),
    )
    .await
    {
        Err(_) => ProviderTestResult::fail("connect_failed"),
        Ok(res) => {
            let status = res.status();
            let body = body_limited(res, MAX_BODY).await.unwrap_or_default();
            if status == 401 || status == 403 {
                return ProviderTestResult::fail("invalid_api_key");
            }
            if status != 200 {
                return ProviderTestResult::fail("http_error");
            }
            #[derive(Deserialize)]
            struct LoginResp {
                data: LoginData,
            }
            #[derive(Deserialize)]
            struct LoginData {
                token: String,
            }
            match serde_json::from_slice::<LoginResp>(&body) {
                Ok(parsed) if !parsed.data.token.is_empty() => {
                    let mut details = BTreeMap::new();
                    details.insert("status".into(), "token_ok".into());
                    ProviderTestResult::ok_with(details)
                }
                Ok(_) => ProviderTestResult::fail("unexpected_response"),
                Err(_) => ProviderTestResult::fail("parse_failed"),
            }
        }
    }
}

async fn validate_metadata(
    state: &AppState,
    req: &SetupMetadataValidateRequest,
) -> SetupMetadataValidateResponse {
    let mut resp = SetupMetadataValidateResponse { ok: true, ..Default::default() };

    if req.tmdb.is_none() && req.tvdb.is_none() {
        resp.ok = false;
        state.hub.publish_event(
            "setup.metadata.validation.error",
            json!({
                "error": "no_providers_provided",
                "message": "At least one metadata provider (TMDB or TVDB) must be configured",
            }),
        );
        return resp;
    }

    state.hub.publish_event(
        "setup.metadata.validation.started",
        json!({"tmdb": req.tmdb.is_some(), "tvdb": req.tvdb.is_some()}),
    );

    if let Some(cfg) = &req.tmdb {
        state.hub.publish_event(
            "setup.metadata.validation.progress",
            json!({"provider": "tmdb", "status": "testing"}),
        );
        let r = test_tmdb(state, cfg).await;
        if !r.ok {
            resp.ok = false;
            state.hub.publish_event(
                "setup.metadata.validation.progress",
                json!({"provider": "tmdb", "status": "failed", "ok": false, "error": r.error}),
            );
        } else {
            state.hub.publish_event(
                "setup.metadata.validation.progress",
                json!({"provider": "tmdb", "status": "completed", "ok": true, "details": r.details}),
            );
        }
        resp.tmdb = Some(r);
    }

    if let Some(cfg) = &req.tvdb {
        state.hub.publish_event(
            "setup.metadata.validation.progress",
            json!({"provider": "tvdb", "status": "testing"}),
        );
        let r = test_tvdb(state, cfg).await;
        if !r.ok {
            resp.ok = false;
            state.hub.publish_event(
                "setup.metadata.validation.progress",
                json!({"provider": "tvdb", "status": "failed", "ok": false, "error": r.error}),
            );
        } else {
            state.hub.publish_event(
                "setup.metadata.validation.progress",
                json!({"provider": "tvdb", "status": "completed", "ok": true, "details": r.details}),
            );
        }
        resp.tvdb = Some(r);
    }

    state.hub.publish_event(
        "setup.metadata.validation.completed",
        json!({
            "ok": resp.ok,
            "summary": {
                "tmdbOk": resp.tmdb.as_ref().is_some_and(|r| r.ok),
                "tvdbOk": resp.tvdb.as_ref().is_some_and(|r| r.ok),
            },
        }),
    );

    resp
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
            dir.path().join("metadata_test.db").display()
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

    async fn post_admin(
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
        let app = axum::Router::new()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state);
        let resp = app.oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn admin_gate_blocks_anonymous() {
        let (_dir, state) = app_state().await;
        let (s, v) = post_admin(
            state,
            "/api/v1/setup/metadata/validate",
            r#"{"tmdb":{"baseUrl":"","apiKey":"k"}}"#,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "unauthorized");
    }

    #[tokio::test]
    async fn requires_at_least_one_provider() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;
        let (s, v) = post_admin(state, "/api/v1/setup/metadata/validate", "{}", Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], false);
    }

    /// Live probes against local stubs replicating both providers' contracts.
    #[tokio::test]
    async fn live_provider_probes_and_encrypted_save() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .and(query_param("api_key", "good-tmdb-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"images": {"base_url": "http://image.themoviedb.org/t/p/"}}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v4/login"))
            .and(wiremock::matchers::body_json(serde_json::json!({"apikey": "good-tvdb-key"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": {"token": "tok123"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v4/login"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        // TMDB ok, TVDB bad key.
        let body = serde_json::json!({
            "tmdb": {"baseUrl": server.uri(), "apiKey": "good-tmdb-key"},
            "tvdb": {"baseUrl": server.uri(), "apiKey": "bad-key"},
        })
        .to_string();
        let (_, v) = post_admin(
            state.clone(),
            "/api/v1/setup/metadata/validate",
            &body,
            Some(&sess),
        )
        .await;
        assert_eq!(v["tmdb"]["ok"], true);
        assert_eq!(v["tmdb"]["details"]["imagesBaseUrl"], "http://image.themoviedb.org/t/p/");
        assert_eq!(v["tvdb"]["ok"], false);
        assert_eq!(v["tvdb"]["error"], "invalid_api_key");

        // Save requires everything to pass.
        let good = serde_json::json!({
            "tmdb": {"baseUrl": server.uri(), "apiKey": "good-tmdb-key"},
            "tvdb": {"baseUrl": server.uri(), "apiKey": "good-tvdb-key"},
        })
        .to_string();
        let (s, v) = post_admin(
            state.clone(),
            "/api/v1/setup/metadata/save",
            &good,
            Some(&sess),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["saved"], true);

        let stored = crate::storage::settings::get(&state.db, "setup.metadata")
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.contains("good-tmdb-key"), "TMDB key stored in cleartext!");
        assert!(!stored.contains("good-tvdb-key"), "TVDB key stored in cleartext!");
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        let dec = state
            .secret_box
            .decrypt_string(parsed["tmdb"]["apiKeyEnc"].as_str().unwrap())
            .unwrap();
        assert_eq!(dec, b"good-tmdb-key");
    }

    #[tokio::test]
    async fn empty_tmdb_key_uses_fallback_and_warns() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .and(query_param("api_key", FALLBACK_TMDB_API_KEY))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"images": {"base_url": "http://img/"}}),
                ),
            )
            .mount(&server)
            .await;

        let body = serde_json::json!({
            "tmdb": {"baseUrl": server.uri(), "apiKey": ""},
        })
        .to_string();
        let (_, v) = post_admin(state, "/api/v1/setup/metadata/validate", &body, Some(&sess)).await;
        assert_eq!(v["tmdb"]["ok"], true);
        assert!(v["tmdb"]["details"]["warning"]
            .as_str()
            .unwrap_or("")
            .contains("fallback"));
    }
}
