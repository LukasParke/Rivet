//! `/setup/clients/*`: download/usenet client validation + persistence.
//!
//! All connectivity probes route through [`crate::security::ssrf`] — URLs are
//! resolved once, policy-checked over every returned IP and pinned into the
//! client's resolver (`request_validated` / `get_validated`). No raw clients.
//! Secrets are encrypted via the shared SecretBox before persisting.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::httpapi::middleware::auth::Admin;
use crate::security::ssrf::{self, SsrfError};
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};

const SETTING_KEY: &str = "setup.clients";
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);
const MAX_BODY: usize = 64 * 1024;

/// Form/query encoding compatible enough with Go's `url.Values.Encode`.
const FORM_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn qenc(s: &str) -> String {
    utf8_percent_encode(s, FORM_SET).to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct QBittorrentConfig {
    url: String,
    username: String,
    password: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct NZBGetConfig {
    url: String,
    username: String,
    password: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SABnzbdConfig {
    url: String,
    apiKey: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SetupClientsValidateRequest {
    #[serde(default)]
    qbittorrent: Option<QBittorrentConfig>,
    #[serde(default)]
    nzbget: Option<NZBGetConfig>,
    #[serde(default)]
    sabnzbd: Option<SABnzbdConfig>,
}

#[derive(Debug, Default, Serialize)]
struct ClientTestResult {
    ok: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    details: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ClientTestResult {
    fn fail(code: &str) -> Self {
        ClientTestResult { ok: false, error: Some(code.to_string()), ..Default::default() }
    }

    fn ok_with(details: BTreeMap<String, String>) -> Self {
        ClientTestResult { ok: true, details, error: None }
    }
}

#[derive(Debug, Default, Serialize)]
struct SetupClientsValidateResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    qbittorrent: Option<ClientTestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nzbget: Option<ClientTestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sabnzbd: Option<ClientTestResult>,
}

#[derive(Debug, Serialize)]
struct SetupClientsSaveResponse {
    saved: bool,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/setup/clients/validate", axum::routing::post(validate))
        .route("/setup/clients/save", axum::routing::post(save))
}

async fn validate(
    _user: Admin,
    State(state): State<AppState>,
    body: Result<axum::Json<SetupClientsValidateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let resp = validate_clients(&state, &req).await;
    axum::Json(resp).into_response()
}

async fn save(
    user: Admin,
    State(state): State<AppState>,
    body: Result<axum::Json<SetupClientsValidateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let actor = user.0.user.id.clone();
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };

    let resp = validate_clients(&state, &req).await;
    if !resp.ok {
        return (StatusCode::BAD_REQUEST, axum::Json(resp)).into_response();
    }

    // Encrypt secrets BEFORE persisting; an encryption failure must fail the
    // save rather than store plaintext or empty strings (the Go original
    // silently stored "" on encryption errors).
    let mut persist = serde_json::Map::new();
    if let Some(qb) = &req.qbittorrent {
        let pw_enc = match state.secret_box.encrypt_to_string(qb.password.trim().as_bytes()) {
            Ok(e) => e,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": "save_failed"})),
                )
                    .into_response()
            }
        };
        persist.insert(
            "qbittorrent".into(),
            json!({
                "url": qb.url.trim(),
                "username": qb.username.trim(),
                "passwordEnc": pw_enc,
            }),
        );
    }
    if let Some(ng) = &req.nzbget {
        let pw_enc = match state.secret_box.encrypt_to_string(ng.password.trim().as_bytes()) {
            Ok(e) => e,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": "save_failed"})),
                )
                    .into_response()
            }
        };
        persist.insert(
            "nzbget".into(),
            json!({
                "url": ng.url.trim(),
                "username": ng.username.trim(),
                "passwordEnc": pw_enc,
            }),
        );
    }
    if let Some(sab) = &req.sabnzbd {
        let key_enc = match state.secret_box.encrypt_to_string(sab.apiKey.trim().as_bytes()) {
            Ok(e) => e,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": "save_failed"})),
                )
                    .into_response()
            }
        };
        persist.insert(
            "sabnzbd".into(),
            json!({
                "url": sab.url.trim(),
                "apiKeyEnc": key_enc,
            }),
        );
    }

    let payload = serde_json::Value::Object(persist).to_string();
    if let Err(_) = crate::storage::settings::upsert(&state.db, SETTING_KEY, &payload).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "save_failed"})),
        )
            .into_response();
    }

    let _ = insert_audit(
        &state.db,
        AuditEvent {
            action: "setup.clients_saved".into(),
            actor_user_id: Some(actor.clone()),
            ..Default::default()
        },
    )
    .await;
    state
        .hub
        .publish_event("audit.event", json!({"action": "setup.clients_saved", "actorUserId": actor}));
    axum::Json(SetupClientsSaveResponse { saved: true }).into_response()
}

async fn body_limited(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, SsrfError> {
    let mut resp = resp;
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| SsrfError::Network)? {
        if out.len() + chunk.len() > max {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// scheme://host[:port] origin of the configured URL…
fn base_origin(raw: &str) -> Option<url::Url> {
    let mut u = url::Url::parse(raw.trim()).ok()?;
    u.set_path("");
    u.set_query(None);
    u.set_fragment(None);
    Some(u)
}

/// …joined with an absolute path (and optional query), matching the Go
/// `base.ResolveReference(&url.URL{Path: ...})` semantics.
fn join_url(origin: &url::Url, path: &str, query: Option<&str>) -> String {
    let mut u = origin.clone();
    u.set_path(path);
    u.set_query(query);
    u.set_fragment(None);
    u.to_string()
}

async fn validate_clients(state: &AppState, req: &SetupClientsValidateRequest) -> SetupClientsValidateResponse {
    let mut resp = SetupClientsValidateResponse { ok: true, ..Default::default() };

    // Require at least one configured client.
    if req.qbittorrent.is_none() && req.nzbget.is_none() && req.sabnzbd.is_none() {
        state.hub.publish_event(
            "setup.clients.validation.error",
            json!({"error": "no_clients_provided", "message": "At least one client must be configured"}),
        );
        return SetupClientsValidateResponse { ok: false, ..Default::default() };
    }

    let count =
        req.qbittorrent.is_some() as i64 + req.nzbget.is_some() as i64 + req.sabnzbd.is_some() as i64;
    state.hub.publish_event(
        "setup.clients.validation.started",
        json!({
            "qbittorrent": req.qbittorrent.is_some(),
            "nzbget": req.nzbget.is_some(),
            "sabnzbd": req.sabnzbd.is_some(),
            "totalCount": count,
        }),
    );

    macro_rules! run_client {
        ($slot:ident, $name:literal, $probe:expr) => {
            if let Some(cfg) = &req.$slot {
                state.hub.publish_event(
                    "setup.clients.validation.progress",
                    json!({"client": $name, "status": "testing"}),
                );
                let r = $probe(state, cfg).await;
                resp.ok = resp.ok && r.ok;
                let done = json!({
                    "client": $name,
                    "status": "completed",
                    "ok": r.ok,
                    "error": r.error,
                    "details": r.details,
                });
                resp.$slot = Some(r);
                state.hub.publish_event("setup.clients.validation.progress", done);
            }
        };
    }

    run_client!(qbittorrent, "qbittorrent", test_qbittorrent);
    run_client!(nzbget, "nzbget", test_nzbget);
    run_client!(sabnzbd, "sabnzbd", test_sabnzbd);

    state.hub.publish_event(
        "setup.clients.validation.completed",
        json!({
            "ok": resp.ok,
            "summary": {
                "qbittorrentOk": resp.qbittorrent.as_ref().is_some_and(|r| r.ok),
                "nzbgetOk": resp.nzbget.as_ref().is_some_and(|r| r.ok),
                "sabnzbdOk": resp.sabnzbd.as_ref().is_some_and(|r| r.ok),
            },
        }),
    );

    resp
}

async fn test_qbittorrent(state: &AppState, cfg: &QBittorrentConfig) -> ClientTestResult {
    let url = cfg.url.trim();
    let username = cfg.username.trim();
    let password = cfg.password.trim();
    if url.is_empty() || username.is_empty() || password.is_empty() {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "qbittorrent", "error": "missing_fields", "errorCode": "missing_fields"}),
        );
        return ClientTestResult::fail("missing_fields");
    }

    // Login (POST form; sets the SID cookie).
    let Some(origin) = base_origin(url) else {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "qbittorrent", "error": "invalid URL", "errorCode": "invalid_url"}),
        );
        return ClientTestResult::fail("invalid URL");
    };
    let login_url = join_url(&origin, "/api/v2/auth/login", None);
    let form = format!("username={}&password={}", qenc(username), qenc(password));

    state.hub.publish_event(
        "setup.clients.test.authenticating",
        json!({"client": "qbittorrent"}),
    );
    let login = ssrf::request_validated(
        reqwest::Method::POST,
        &login_url,
        state.policy,
        PROBE_TIMEOUT,
        &[("content-type", "application/x-www-form-urlencoded")],
        Some(form),
    )
    .await;

    let login = match login {
        Ok(r) => r,
        Err(_) => {
            state.hub.publish_event(
                "setup.clients.test.failed",
                json!({"client": "qbittorrent", "error": "connect_failed", "errorCode": "connect_failed"}),
            );
            return ClientTestResult::fail("connect_failed");
        }
    };
    let status = login.status();
    let cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let body = body_limited(login, MAX_BODY).await.unwrap_or_default();
    let body_str = String::from_utf8_lossy(&body).trim().to_string();

    if status != 200 || body_str != "Ok." {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "qbittorrent", "error": "login_failed", "errorCode": "login_failed", "statusCode": status.as_u16()}),
        );
        return ClientTestResult::fail("login_failed");
    }

    // Version probe carrying the session cookie.
    state.hub.publish_event(
        "setup.clients.test.querying",
        json!({"client": "qbittorrent", "query": "version"}),
    );
    let ver_url = join_url(&origin, "/api/v2/app/version", None);
    let headers: Vec<(&str, &str)> = if cookie.is_empty() {
        vec![]
    } else {
        vec![("cookie", cookie.as_str())]
    };
    match ssrf::request_validated(reqwest::Method::GET, &ver_url, state.policy, PROBE_TIMEOUT, &headers, None)
        .await
    {
        Err(_) => {
            state.hub.publish_event(
                "setup.clients.test.failed",
                json!({"client": "qbittorrent", "error": "version_failed", "errorCode": "version_failed"}),
            );
            ClientTestResult::fail("version_failed")
        }
        Ok(vres) => {
            let vstatus = vres.status();
            let vbody = body_limited(vres, MAX_BODY).await.unwrap_or_default();
            let version = String::from_utf8_lossy(&vbody).trim().to_string();
            if vstatus != 200 || version.is_empty() {
                state.hub.publish_event(
                    "setup.clients.test.failed",
                    json!({"client": "qbittorrent", "error": "version_failed", "errorCode": "version_failed", "statusCode": vstatus.as_u16()}),
                );
                return ClientTestResult::fail("version_failed");
            }
            let mut details = BTreeMap::new();
            details.insert("version".into(), version.clone());
            state.hub.publish_event(
                "setup.clients.test.completed",
                json!({"client": "qbittorrent", "ok": true, "version": version, "details": details}),
            );
            ClientTestResult::ok_with(details)
        }
    }
}

async fn test_nzbget(state: &AppState, cfg: &NZBGetConfig) -> ClientTestResult {
    let url = cfg.url.trim();
    let username = cfg.username.trim();
    let password = cfg.password.trim();
    if url.is_empty() || username.is_empty() || password.is_empty() {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "nzbget", "error": "missing_fields", "errorCode": "missing_fields"}),
        );
        return ClientTestResult::fail("missing_fields");
    }

    // NZBGet JSON-RPC: POST /jsonrpc with basic auth.
    let Some(origin) = base_origin(url) else {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "nzbget", "error": "invalid URL", "errorCode": "invalid_url"}),
        );
        return ClientTestResult::fail("invalid URL");
    };
    let rpc_url = join_url(&origin, "/jsonrpc", None);
    let payload = json!({"method": "version", "params": [], "id": 1}).to_string();
    let auth = B64.encode(format!("{username}:{password}"));
    let auth_header = format!("Basic {auth}");

    state.hub.publish_event(
        "setup.clients.test.querying",
        json!({"client": "nzbget", "query": "version"}),
    );

    match ssrf::request_validated(
        reqwest::Method::POST,
        &rpc_url,
        state.policy,
        PROBE_TIMEOUT,
        &[("content-type", "application/json"), ("authorization", auth_header.as_str())],
        Some(payload),
    )
    .await
    {
        Err(_) => {
            state.hub.publish_event(
                "setup.clients.test.failed",
                json!({"client": "nzbget", "error": "connect_failed", "errorCode": "connect_failed"}),
            );
            ClientTestResult::fail("connect_failed")
        }
        Ok(res) => {
            let status = res.status();
            let body = body_limited(res, MAX_BODY).await.unwrap_or_default();
            if status != 200 {
                state.hub.publish_event(
                    "setup.clients.test.failed",
                    json!({"client": "nzbget", "error": "auth_failed", "errorCode": "auth_failed", "statusCode": status.as_u16()}),
                );
                return ClientTestResult::fail("auth_failed");
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Rpc {
                #[allow(dead_code)]
                result: Option<serde_json::Value>,
                error: Option<serde_json::Value>,
            }
            let parsed: Result<Rpc, _> = serde_json::from_slice(&body);
            match parsed {
                Ok(Rpc { error: Some(_), .. }) | Err(_) => {
                    state.hub.publish_event(
                        "setup.clients.test.failed",
                        json!({"client": "nzbget", "error": "rpc_error", "errorCode": "rpc_error"}),
                    );
                    ClientTestResult::fail("rpc_error")
                }
                Ok(Rpc { result, .. }) => {
                    let ver = result
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .unwrap_or("ok")
                        .to_string();
                    let mut details = BTreeMap::new();
                    details.insert("version".into(), ver.clone());
                    state.hub.publish_event(
                        "setup.clients.test.completed",
                        json!({"client": "nzbget", "ok": true, "version": ver, "details": details}),
                    );
                    ClientTestResult::ok_with(details)
                }
            }
        }
    }
}

async fn test_sabnzbd(state: &AppState, cfg: &SABnzbdConfig) -> ClientTestResult {
    let url = cfg.url.trim();
    let api_key = cfg.apiKey.trim();
    if url.is_empty() || api_key.is_empty() {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "sabnzbd", "error": "missing_fields", "errorCode": "missing_fields"}),
        );
        return ClientTestResult::fail("missing_fields");
    }

    // SABnzbd API: GET /api?apikey=...&mode=version&output=json — plain GET,
    // so this goes through get_validated (redirect hops re-validated + pinned).
    let Some(origin) = base_origin(url) else {
        state.hub.publish_event(
            "setup.clients.test.failed",
            json!({"client": "sabnzbd", "error": "invalid URL", "errorCode": "invalid_url"}),
        );
        return ClientTestResult::fail("invalid URL");
    };
    let query = format!("apikey={}&mode=version&output=json", qenc(api_key));
    let api_url = join_url(&origin, "/api", Some(&query));

    state.hub.publish_event(
        "setup.clients.test.querying",
        json!({"client": "sabnzbd", "query": "version"}),
    );

    match ssrf::get_validated(&api_url, state.policy, PROBE_TIMEOUT, 3).await {
        Err(_) => {
            state.hub.publish_event(
                "setup.clients.test.failed",
                json!({"client": "sabnzbd", "error": "connect_failed", "errorCode": "connect_failed"}),
            );
            ClientTestResult::fail("connect_failed")
        }
        Ok(res) => {
            let status = res.status();
            let body = body_limited(res, MAX_BODY).await.unwrap_or_default();
            if status != 200 {
                state.hub.publish_event(
                    "setup.clients.test.failed",
                    json!({"client": "sabnzbd", "error": "api_failed", "errorCode": "api_failed", "statusCode": status.as_u16()}),
                );
                return ClientTestResult::fail("api_failed");
            }
            #[derive(Deserialize)]
            struct SabVersion {
                version: String,
            }
            match serde_json::from_slice::<SabVersion>(&body) {
                Ok(v) if !v.version.is_empty() => {
                    let mut details = BTreeMap::new();
                    details.insert("version".into(), v.version.clone());
                    state.hub.publish_event(
                        "setup.clients.test.completed",
                        json!({"client": "sabnzbd", "ok": true, "version": v.version, "details": details}),
                    );
                    ClientTestResult::ok_with(details)
                }
                _ => {
                    let code = if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
                        "parse_failed"
                    } else {
                        "api_failed"
                    };
                    state.hub.publish_event(
                        "setup.clients.test.failed",
                        json!({"client": "sabnzbd", "error": code, "errorCode": code}),
                    );
                    ClientTestResult::fail(code)
                }
            }
        }
    }
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
            dir.path().join("clients_test.db").display()
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
            "/api/v1/setup/clients/validate",
            r#"{"sabnzbd":{"url":"http://127.0.0.1:1","apiKey":"k"}}"#,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "unauthorized");
    }

    #[tokio::test]
    async fn requires_at_least_one_client() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;
        let (s, v) = post_admin(
            state,
            "/api/v1/setup/clients/validate",
            r#"{}"#,
            Some(&sess),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], false);
    }

    #[tokio::test]
    async fn unreachable_client_reports_connect_failure_without_panicking() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;
        // Port 1 on loopback: nothing listens; policy dev-mode allows it.
        let body = serde_json::json!({
            "nzbget": {"url": "http://127.0.0.1:1", "username": "u", "password": "p"}
        })
        .to_string();
        let (s, v) = post_admin(state, "/api/v1/setup/clients/validate", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], false);
        assert_eq!(v["nzbget"]["error"], "connect_failed");
    }

    #[tokio::test]
    async fn missing_fields_fail_fast() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;
        let body = serde_json::json!({
            "sabnzbd": {"url": "", "apiKey": ""}
        })
        .to_string();
        let (s, v) = post_admin(state, "/api/v1/setup/clients/validate", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["sabnzbd"]["error"], "missing_fields");
    }

    /// Full save roundtrip against a live local HTTP stub proving both the
    /// probe path (SSRF-pinned) and at-rest encryption.
    #[tokio::test]
    async fn save_against_live_stub_encrypts_secrets() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        let server = MockServer::start().await;
        // SABnzbd: GET /api returns {"version":"4.1.0"}.
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"version":"4.1.0"})))
            .mount(&server)
            .await;
        // qBittorrent: POST login returns "Ok." + SID cookie, GET version returns string.
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("Ok.")
                    .append_header("set-cookie", "SID=abc123; path=/"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/app/version"))
            .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.0"))
            .mount(&server)
            .await;
        // NZBGet: POST /jsonrpc echoes RPC result.
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"result": "22.0", "error": null})),
            )
            .mount(&server)
            .await;

        let base = server.uri();
        let body = serde_json::json!({
            "qbittorrent": {"url": base, "username": "admin", "password": "hunter2-secret"},
            "nzbget": {"url": base, "username": "admin", "password": "nzb-pass-9"},
            "sabnzbd": {"url": base, "apiKey": "sab-key-7"},
        })
        .to_string();

        let (s, v) = post_admin(state.clone(), "/api/v1/setup/clients/save", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK, "all three probes must pass against the stub");
        assert_eq!(v["saved"], true);

        let stored = crate::storage::settings::get(&state.db, "setup.clients")
            .await
            .unwrap()
            .unwrap();
        // ACCEPTANCE: ciphertext != plaintext in DB.
        assert!(!stored.contains("hunter2-secret"), "qBittorrent password stored in cleartext!");
        assert!(!stored.contains("nzb-pass-9"), "NZBGet password stored in cleartext!");
        assert!(!stored.contains("sab-key-7"), "SABnzbd API key stored in cleartext!");
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_ne!(parsed["qbittorrent"]["passwordEnc"], "hunter2-secret");

        // Decryption recovers the exact secrets (shared SecretBox).
        let dec = state
            .secret_box
            .decrypt_string(parsed["qbittorrent"]["passwordEnc"].as_str().unwrap())
            .unwrap();
        assert_eq!(dec, b"hunter2-secret");
    }
}
