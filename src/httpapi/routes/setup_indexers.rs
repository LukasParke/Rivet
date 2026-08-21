//! `/setup/indexers/*`: Torznab/Newznab indexer validation + persistence
//! (admin only), including the Prowlarr special-casing of the original.
//!
//! Connectivity probes route exclusively through [`crate::security::ssrf`]
//! (`request_validated`: resolve-once, policy-check every IP, pin the
//! resolver). Secrets are encrypted before persisting.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::httpapi::middleware::auth::Admin;
use crate::security::ssrf::{self, SsrfError};
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};

const SETTING_KEY: &str = "setup.indexers";
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
const MAX_BODY: usize = 1024 * 1024;

const QUERY_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn qenc(s: &str) -> String {
    utf8_percent_encode(s, QUERY_SET).to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct IndexerConfig {
    #[serde(default)]
    name: String,
    url: String,
    #[serde(default)]
    apiKey: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SetupIndexersValidateRequest {
    #[serde(default)]
    torznab: Vec<IndexerConfig>,
    #[serde(default)]
    newznab: Vec<IndexerConfig>,
}

#[derive(Debug, Default, Clone, Serialize)]
struct IndexerTestResult {
    ok: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    details: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl IndexerTestResult {
    fn fail(code: &str) -> Self {
        IndexerTestResult { ok: false, error: Some(code.to_string()), ..Default::default() }
    }

    fn fail_with_hint(code: &str, hint: &str) -> Self {
        let mut details = BTreeMap::new();
        details.insert("hint".into(), hint.to_string());
        IndexerTestResult { ok: false, details, error: Some(code.to_string()) }
    }

    fn ok_with(details: BTreeMap<String, String>) -> Self {
        IndexerTestResult { ok: true, details, error: None }
    }
}

#[derive(Debug, Default, Serialize)]
struct SetupIndexersValidateResponse {
    ok: bool,
    torznab: Vec<IndexerTestResult>,
    newznab: Vec<IndexerTestResult>,
}

#[derive(Debug, Serialize)]
struct SetupIndexersSaveResponse {
    saved: bool,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/setup/indexers/validate", axum::routing::post(validate))
        .route("/setup/indexers/save", axum::routing::post(save))
}

async fn validate(
    _user: Admin,
    State(state): State<AppState>,
    body: Result<
        axum::Json<SetupIndexersValidateRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let resp = validate_indexers(&state, &req).await;
    axum::Json(resp).into_response()
}

async fn save(
    user: Admin,
    State(state): State<AppState>,
    body: Result<
        axum::Json<SetupIndexersValidateRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let actor = user.0.user.id.clone();
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };

    let resp = validate_indexers(&state, &req).await;
    if !resp.ok {
        return (StatusCode::BAD_REQUEST, axum::Json(resp)).into_response();
    }

    // Encrypt secrets BEFORE persisting; encryption failure fails the save
    // instead of storing plaintext or empty strings.
    let mut persist = serde_json::Map::new();
    for kind in ["torznab", "newznab"] {
        let list = match kind {
            "torznab" => &req.torznab,
            _ => &req.newznab,
        };
        let mut out = Vec::with_capacity(list.len());
        for ix in list {
            let key_enc = match state.secret_box.encrypt_to_string(ix.apiKey.trim().as_bytes()) {
                Ok(e) => e,
                Err(_) => return save_failed_response(),
            };
            let pw_enc = match state.secret_box.encrypt_to_string(ix.password.trim().as_bytes()) {
                Ok(e) => e,
                Err(_) => return save_failed_response(),
            };
            out.push(json!({
                "name": ix.name.trim(),
                "url": ix.url.trim(),
                "apiKeyEnc": key_enc,
                "username": ix.username.trim(),
                "passwordEnc": pw_enc,
            }));
        }
        persist.insert(kind.to_string(), serde_json::Value::Array(out));
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
            action: "setup.indexers_saved".into(),
            actor_user_id: Some(actor.clone()),
            ..Default::default()
        },
    )
    .await;
    state.hub.publish_event(
        "audit.event",
        json!({"action": "setup.indexers_saved", "actorUserId": actor}),
    );
    axum::Json(SetupIndexersSaveResponse { saved: true }).into_response()
}

fn save_failed_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"error": "save_failed"})),
    )
        .into_response()
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

fn is_prowlarr_url(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("prowlarr") || lower.contains(":9696") || lower.contains("/prowlarr")
}

fn all_ok(results: &[IndexerTestResult]) -> bool {
    results.iter().all(|r| r.ok)
}

/// Diagnostic classification mirroring the Go `networkErrorInfo`.
fn network_error_info(err: &str, target: &str) -> (&'static str, String) {
    let e = err.to_ascii_lowercase();
    if e.contains("no route to host") || e.contains("network is unreachable") {
        return ("network", format!("Network connectivity issue: The server cannot reach {target}. This may be because the server is running in Docker and cannot access the host network, or there is a firewall/routing issue."));
    }
    if e.contains("connection refused") {
        return ("network", format!("Connection refused: The server can reach {target} but the connection was refused. This usually means the service is not running on that address/port."));
    }
    if e.contains("timeout") || e.contains("deadline exceeded") {
        return ("network", format!("Connection timeout: The connection attempt to {target} timed out."));
    }
    if e.contains("connection reset") {
        return ("network", format!("Connection reset: The connection to {target} was reset."));
    }
    if e.contains("no such host") || e.contains("dns") || e.contains("failed to lookup") {
        return ("dns", format!("DNS resolution failed: The server cannot resolve the hostname for {target}."));
    }
    if e.contains("tls") || e.contains("ssl") || e.contains("certificate") {
        return ("tls", format!("TLS/SSL error connecting to {target}: check certificate configuration."));
    }
    ("unknown", format!("Connection failed: {err}"))
}

/// Extracts `attr="value"` from the first `<error` tag (case-insensitive scan,
/// no XML dependency).
fn extract_error_attr(body: &str, attr: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let start = lower.find("<error")?;
    let end = lower[start..].find('>')? + start;
    let tag = &body[start..end];
    let needle = format!("{attr}=\"");
    let pos = tag.to_ascii_lowercase().find(&needle)? + attr.len() + 2;
    let rest = &tag[pos..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// Newznab error responses are `<error code=".." description=".."/>` roots.
fn parse_newznab_error(body: &str) -> Option<String> {
    if !body.to_ascii_lowercase().contains("<error") {
        return None;
    }
    if let Some(desc) = extract_error_attr(body, "description").filter(|d| !d.is_empty()) {
        return Some(format!("error:{desc}"));
    }
    extract_error_attr(body, "code")
        .filter(|c| !c.is_empty())
        .map(|c| format!("error_code:{c}"))
}

/// Counts `<item>` elements in an RSS payload.
fn count_rss_items(body: &str) -> i64 {
    body.matches("<item").count() as i64
}

fn xml_preview(body: &str) -> String {
    if body.len() > 200 {
        format!("{}...", &body[..200])
    } else {
        body.to_string()
    }
}

async fn validate_indexers(
    state: &AppState,
    req: &SetupIndexersValidateRequest,
) -> SetupIndexersValidateResponse {
    let mut resp = SetupIndexersValidateResponse { ok: true, ..Default::default() };

    if req.torznab.is_empty() && req.newznab.is_empty() {
        state.hub.publish_event(
            "setup.indexer.validation.error",
            json!({
                "error": "no_indexers_provided",
                "message": "At least one indexer (Torznab or Newznab) must be provided",
            }),
        );
        return SetupIndexersValidateResponse { ok: false, ..Default::default() };
    }

    let total = req.torznab.len() + req.newznab.len();
    state.hub.publish_event(
        "setup.indexer.validation.started",
        json!({
            "torznabCount": req.torznab.len(),
            "newznabCount": req.newznab.len(),
            "totalCount": total,
        }),
    );

    let mut step = 0usize;
    for (kind, list) in [("torznab", &req.torznab), ("newznab", &req.newznab)] {
        for ix in list.iter() {
            step += 1;
            state.hub.publish_event(
                "setup.indexer.validation.progress",
                json!({
                    "step": step,
                    "total": total,
                    "indexerName": ix.name,
                    "indexerUrl": ix.url,
                    "type": kind,
                    "status": "testing",
                }),
            );
            let r = test_newznab_like(state, ix).await;
            resp.ok = resp.ok && r.ok;
            let done = json!({
                "step": step,
                "total": total,
                "indexerName": ix.name,
                "indexerUrl": ix.url,
                "type": kind,
                "status": "completed",
                "ok": r.ok,
                "error": r.error,
                "details": r.details,
            });
            if kind == "torznab" {
                resp.torznab.push(r);
            } else {
                resp.newznab.push(r);
            }
            state.hub.publish_event("setup.indexer.validation.progress", done);
        }
    }

    state.hub.publish_event(
        "setup.indexer.validation.completed",
        json!({
            "ok": resp.ok,
            "torznabOk": !resp.torznab.is_empty() && all_ok(&resp.torznab),
            "newznabOk": !resp.newznab.is_empty() && all_ok(&resp.newznab),
            "summary": {
                "torznabCount": resp.torznab.len(),
                "newznabCount": resp.newznab.len(),
                "torznabResults": resp.torznab,
                "newznabResults": resp.newznab,
            },
        }),
    );

    resp
}

/// Torznab and Newznab share the same wire format; one probe covers both.
async fn test_newznab_like(state: &AppState, cfg: &IndexerConfig) -> IndexerTestResult {
    let url = cfg.url.trim();
    let api_key = cfg.apiKey.trim();
    let username = cfg.username.trim();
    let password = cfg.password.trim();

    if url.is_empty() || api_key.is_empty() {
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": "missing_fields",
                "errorCode": "missing_fields",
                "details": {"hint": "URL and API key are required"},
            }),
        );
        return IndexerTestResult::fail("missing_fields");
    }

    state.hub.publish_event(
        "setup.indexer.test.started",
        json!({"indexerName": cfg.name, "indexerUrl": cfg.url}),
    );

    let Some(origin) = base_origin(url) else {
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": "invalid URL",
                "errorCode": "invalid_url",
                "details": {"hint": "Invalid or disallowed URL"},
            }),
        );
        return IndexerTestResult::fail("invalid URL");
    };

    // Prowlarr instances need their per-indexer Torznab endpoints.
    if is_prowlarr_url(url) {
        state.hub.publish_event(
            "setup.indexer.prowlarr.detected",
            json!({"indexerName": cfg.name, "indexerUrl": cfg.url}),
        );
        state.hub.publish_event(
            "setup.indexer.prowlarr.finding_indexer",
            json!({"indexerName": cfg.name, "indexerUrl": cfg.url}),
        );

        return match find_prowlarr_indexer(state, &origin, api_key).await {
            Err(FindError::Auth) => {
                let r = IndexerTestResult::fail_with_hint(
                    "invalid_prowlarr_api_key",
                    "Get your API key from Prowlarr Settings → General → API Key",
                );
                state.hub.publish_event(
                    "setup.indexer.test.failed",
                    json!({
                        "indexerName": cfg.name,
                        "indexerUrl": cfg.url,
                        "error": r.error,
                        "errorCode": r.error,
                        "errorType": "auth",
                        "details": r.details,
                    }),
                );
                r
            }
            Err(FindError::NoIndexers) => {
                let r = IndexerTestResult::fail_with_hint(
                    "prowlarr_no_indexers",
                    "No enabled indexers found in Prowlarr. Enable at least one indexer in Prowlarr's web UI, then try again.",
                );
                state.hub.publish_event(
                    "setup.indexer.test.failed",
                    json!({
                        "indexerName": cfg.name,
                        "indexerUrl": cfg.url,
                        "error": r.error,
                        "errorCode": r.error,
                        "errorType": "configuration",
                        "details": r.details,
                    }),
                );
                r
            }
            Err(FindError::Network(err)) => {
                let (error_type, hint) = network_error_info(&err, url);
                let r = IndexerTestResult::fail_with_hint("prowlarr_connection_failed", &hint);
                state.hub.publish_event(
                    "setup.indexer.test.failed",
                    json!({
                        "indexerName": cfg.name,
                        "indexerUrl": cfg.url,
                        "error": r.error,
                        "errorCode": r.error,
                        "errorType": error_type,
                        "rawError": err,
                        "details": r.details,
                    }),
                );
                r
            }
            Ok(indexer_id) => {
                state.hub.publish_event(
                    "setup.indexer.prowlarr.indexer_found",
                    json!({"indexerName": cfg.name, "indexerUrl": cfg.url, "indexerId": indexer_id}),
                );
                retry_prowlarr_torznab(state, &origin, cfg, indexer_id).await
            }
        };
    }

    // Standard Torznab/Newznab endpoint: /api?t=search&q=ubuntu&...
    state.hub.publish_event(
        "setup.indexer.test.connecting",
        json!({"indexerName": cfg.name, "indexerUrl": cfg.url}),
    );
    let query =
        format!("t=search&q=ubuntu&apikey={}&limit=1", qenc(api_key));
    let api_url = join_url(&origin, "/api", Some(&query));

    let mut headers: Vec<(&str, &str)> = vec![("x-api-key", api_key)];
    let auth_header;
    if !username.is_empty() || !password.is_empty() {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine;
        auth_header =
            format!("Basic {}", B64.encode(format!("{username}:{password}")));
        headers.push(("authorization", auth_header.as_str()));
    }

    state.hub.publish_event(
        "setup.indexer.test.querying",
        json!({"indexerName": cfg.name, "indexerUrl": cfg.url, "query": "ubuntu"}),
    );

    match ssrf::request_validated(reqwest::Method::GET, &api_url, state.policy, PROBE_TIMEOUT, &headers, None)
        .await
    {
        Err(e) => {
            let (error_type, hint) = network_error_info(&e.to_string(), url);
            state.hub.publish_event(
                "setup.indexer.test.failed",
                json!({
                    "indexerName": cfg.name,
                    "indexerUrl": cfg.url,
                    "error": "connect_failed",
                    "errorCode": "connect_failed",
                    "errorType": error_type,
                    "rawError": e.to_string(),
                    "details": {"hint": hint},
                }),
            );
            IndexerTestResult::fail("connect_failed")
        }
        Ok(res) => evaluate_capsule(state, cfg, res, None).await,
    }
}

enum FindError {
    Auth,
    NoIndexers,
    Network(String),
}

/// Queries Prowlarr's REST API for the first enabled indexer.
async fn find_prowlarr_indexer(
    state: &AppState,
    origin: &url::Url,
    api_key: &str,
) -> Result<i64, FindError> {
    let list_url = join_url(origin, "/api/v1/indexer", None);
    let res = ssrf::request_validated(
        reqwest::Method::GET,
        &list_url,
        state.policy,
        PROBE_TIMEOUT,
        &[("x-api-key", api_key)],
        None,
    )
    .await
    .map_err(|e| FindError::Network(e.to_string()))?;

    let status = res.status();
    if status == 401 || status == 403 {
        state.hub.publish_event(
            "setup.indexer.prowlarr.error",
            json!({
                "error": "prowlarr_api_failed",
                "errorCode": "invalid_prowlarr_api_key",
                "errorType": "auth",
                "statusCode": status.as_u16(),
                "details": {"hint": "Invalid API key or unauthorized"},
            }),
        );
        return Err(FindError::Auth);
    }
    if status != 200 {
        state.hub.publish_event(
            "setup.indexer.prowlarr.error",
            json!({
                "error": "prowlarr_api_failed",
                "errorCode": "prowlarr_api_error",
                "errorType": "http",
                "statusCode": status.as_u16(),
                "details": {"hint": "Prowlarr API returned error status"},
            }),
        );
        return Err(FindError::Network(format!("http status {status}")));
    }

    let body = body_limited(res, MAX_BODY).await.map_err(|e| FindError::Network(e.to_string()))?;
    #[derive(Deserialize)]
    struct ProwlarrIndexer {
        id: i64,
        #[serde(default)]
        enable: bool,
    }
    let indexers: Vec<ProwlarrIndexer> = serde_json::from_slice(&body)
        .map_err(|e| FindError::Network(e.to_string()))?;

    for idx in indexers {
        if idx.enable {
            return Ok(idx.id);
        }
    }

    state.hub.publish_event(
        "setup.indexer.prowlarr.error",
        json!({
            "error": "no_enabled_indexers",
            "errorCode": "prowlarr_no_indexers",
            "errorType": "configuration",
            "details": {"hint": "No enabled indexers found in Prowlarr"},
        }),
    );
    Err(FindError::NoIndexers)
}

/// Re-runs the Torznab probe against a specific Prowlarr indexer endpoint.
async fn retry_prowlarr_torznab(
    state: &AppState,
    origin: &url::Url,
    cfg: &IndexerConfig,
    indexer_id: i64,
) -> IndexerTestResult {
    let endpoint_path = format!("/api/v1/indexer/{indexer_id}/newznab");
    state.hub.publish_event(
        "setup.indexer.prowlarr.using_endpoint",
        json!({
            "indexerName": cfg.name,
            "indexerUrl": cfg.url,
            "indexerId": indexer_id,
            "endpoint": endpoint_path,
        }),
    );

    let api_key = cfg.apiKey.trim();
    let query = format!(
        "t=search&q=ubuntu&apikey={}&limit=1",
        qenc(api_key)
    );
    let api_url = join_url(origin, &endpoint_path, Some(&query));

    let mut headers: Vec<(&str, &str)> = vec![("x-api-key", api_key)];
    let auth_header;
    let username = cfg.username.trim();
    let password = cfg.password.trim();
    if !username.is_empty() || !password.is_empty() {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine;
        auth_header = format!("Basic {}", B64.encode(format!("{username}:{password}")));
        headers.push(("authorization", auth_header.as_str()));
    }

    state.hub.publish_event(
        "setup.indexer.test.querying",
        json!({"indexerName": cfg.name, "indexerUrl": cfg.url, "query": "ubuntu", "type": "prowlarr"}),
    );

    match ssrf::request_validated(reqwest::Method::GET, &api_url, state.policy, PROBE_TIMEOUT, &headers, None)
        .await
    {
        Err(e) => {
            let (error_type, hint) = network_error_info(&e.to_string(), origin.as_str());
            let r = IndexerTestResult::fail_with_hint("prowlarr_connection_failed", &hint);
            state.hub.publish_event(
                "setup.indexer.test.failed",
                json!({
                    "indexerName": cfg.name,
                    "indexerUrl": cfg.url,
                    "error": r.error,
                    "errorCode": r.error,
                    "errorType": error_type,
                    "rawError": e.to_string(),
                    "details": r.details,
                }),
            );
            r
        }
        Ok(res) => evaluate_capsule(state, cfg, res, Some(indexer_id)).await,
    }
}

/// Shared response evaluation for the plain and Prowlarr probes.
async fn evaluate_capsule(
    state: &AppState,
    cfg: &IndexerConfig,
    res: reqwest::Response,
    prowlarr_id: Option<i64>,
) -> IndexerTestResult {
    let prowlarr = prowlarr_id.is_some();
    let status = res.status();
    let body_bytes = body_limited(res, MAX_BODY).await.unwrap_or_default();

    if status != 200 {
        if status == 401 || status == 403 {
            let code = if prowlarr {
                "invalid_prowlarr_api_key"
            } else {
                "invalid_api_key"
            };
            let r = IndexerTestResult::fail_with_hint(
                code,
                if prowlarr {
                    "Get your API key from Prowlarr Settings → General → API Key"
                } else {
                    "Check your API key and ensure it's correct."
                },
            );
            state.hub.publish_event(
                "setup.indexer.test.failed",
                json!({
                    "indexerName": cfg.name,
                    "indexerUrl": cfg.url,
                    "error": r.error,
                    "errorCode": r.error,
                    "errorType": "auth",
                    "details": r.details,
                    "statusCode": status.as_u16(),
                }),
            );
            return r;
        }
        let r = IndexerTestResult::fail(&format!("http_{}", status.as_u16()));
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": r.error,
                "errorCode": "http_error",
                "errorType": "http",
                "statusCode": status.as_u16(),
            }),
        );
        return r;
    }

    let body = String::from_utf8_lossy(&body_bytes).trim().to_string();
    if body.is_empty() {
        let code = if prowlarr { "prowlarr_empty_response" } else { "empty_response" };
        let r = IndexerTestResult::fail_with_hint(
            code,
            if prowlarr {
                "Prowlarr returned an empty response. Ensure the indexer is properly configured."
            } else {
                "Indexer returned empty response"
            },
        );
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": r.error,
                "errorCode": code,
                "details": r.details,
            }),
        );
        return r;
    }

    // JSON is unexpected for Torznab/Newznab.
    if body.starts_with('{') || body.starts_with('[') {
        let mut details = BTreeMap::new();
        details.insert("preview".into(), xml_preview(&body));
        let r = IndexerTestResult {
            ok: false,
            details,
            error: Some(if prowlarr { "prowlarr_invalid_format".into() } else { "json_response_expected_xml".into() }),
        };
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": r.error,
                "errorCode": r.error,
                "details": r.details,
            }),
        );
        return r;
    }

    // Root <error/> capsules.
    if let Some(desc) = parse_newznab_error(&body) {
        let code = if prowlarr { "prowlarr_error" } else { "newznab_error" };
        let mut details = BTreeMap::new();
        details.insert(
            "hint".into(),
            if prowlarr {
                format!("Prowlarr returned an error. {desc}")
            } else {
                desc.clone()
            },
        );
        if prowlarr {
            details.insert("rawError".into(), desc);
        }
        let r = IndexerTestResult { ok: false, details, error: Some(code.into()) };
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": r.error,
                "errorCode": code,
                "details": r.details,
            }),
        );
        return r;
    }

    // Must look like XML/RSS.
    if !(body.starts_with("<?xml") || body.starts_with("<rss") || body.starts_with("<error")) {
        let code = if prowlarr { "prowlarr_invalid_format" } else { "invalid_format" };
        let mut details = BTreeMap::new();
        details.insert(
            "hint".into(),
            if prowlarr {
                "Prowlarr response is not valid XML/RSS. Ensure the indexer is properly configured.".to_string()
            } else {
                "Expected XML/RSS response from the indexer.".to_string()
            },
        );
        details.insert("preview".into(), xml_preview(&body));
        let r = IndexerTestResult { ok: false, details, error: Some(code.into()) };
        state.hub.publish_event(
            "setup.indexer.test.failed",
            json!({
                "indexerName": cfg.name,
                "indexerUrl": cfg.url,
                "error": r.error,
                "errorCode": code,
                "details": r.details,
            }),
        );
        return r;
    }

    state.hub.publish_event(
        "setup.indexer.test.parsing",
        json!({"indexerName": cfg.name, "indexerUrl": cfg.url}),
    );

    let items = count_rss_items(&body).max(0);
    let mut details = BTreeMap::new();
    details.insert("itemsParsed".into(), items.to_string());
    if prowlarr {
        details.insert("type".into(), "prowlarr".into());
        details.insert(
            "note".into(),
            format!("Prowlarr detected - using indexer ID {}", prowlarr_id.unwrap_or_default()),
        );
    }

    state.hub.publish_event(
        "setup.indexer.test.completed",
        json!({
            "indexerName": cfg.name,
            "indexerUrl": cfg.url,
            "ok": true,
            "itemsFound": items,
            "details": details,
        }),
    );
    IndexerTestResult::ok_with(details)
}

fn base_origin(raw: &str) -> Option<url::Url> {
    let mut u = url::Url::parse(raw.trim()).ok()?;
    u.set_path("");
    u.set_query(None);
    u.set_fragment(None);
    Some(u)
}

fn join_url(origin: &url::Url, path: &str, query: Option<&str>) -> String {
    let mut u = origin.clone();
    u.set_path(path);
    u.set_query(query);
    u.set_fragment(None);
    u.to_string()
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
            dir.path().join("indexers_test.db").display()
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

    const RSS_BODY: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><item><title>a</title></item><item><title>b</title></item></channel></rss>"#;

    #[tokio::test]
    async fn admin_gate_blocks_anonymous() {
        let (_dir, state) = app_state().await;
        let (s, v) = post_admin(
            state,
            "/api/v1/setup/indexers/validate",
            r#"{"torznab":[],"newznab":[]}"#,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "unauthorized");
    }

    #[tokio::test]
    async fn requires_at_least_one_indexer() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;
        let (s, v) = post_admin(
            state,
            "/api/v1/setup/indexers/validate",
            r#"{"torznab":[],"newznab":[]}"#,
            Some(&sess),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], false);
    }

    #[tokio::test]
    async fn live_torznab_probe_against_stub_and_encrypted_save() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("t", "search"))
            .and(header("x-api-key", "correct-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(RSS_BODY))
            .mount(&server)
            .await;
        // Wrong key gets a 401 to prove header propagation matters.
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base = server.uri();
        let body = serde_json::json!({
            "torznab": [{"name": "good", "url": base, "apiKey": "correct-key"}],
        })
        .to_string();
        let (s, v) = post_admin(
            state.clone(),
            "/api/v1/setup/indexers/validate",
            &body,
            Some(&sess),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert_eq!(v["torznab"][0]["details"]["itemsParsed"], "2");

        // Bad key => invalid_api_key.
        let bad = serde_json::json!({
            "torznab": [{"name": "bad", "url": base, "apiKey": "wrong"}],
        })
        .to_string();
        let (_, v) = post_admin(state.clone(), "/api/v1/setup/indexers/validate", &bad, Some(&sess)).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["torznab"][0]["error"], "invalid_api_key");

        // Save persists encrypted credentials only.
        let save_body = serde_json::json!({
            "torznab": [{"name": "good", "url": base, "apiKey": "correct-key", "password": "ix-secret"}],
            "newznab": [],
        })
        .to_string();
        let (s, v) = post_admin(state.clone(), "/api/v1/setup/indexers/save", &save_body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["saved"], true);

        let stored = crate::storage::settings::get(&state.db, "setup.indexers")
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.contains("correct-key"), "API key stored in cleartext!");
        assert!(!stored.contains("ix-secret"), "password stored in cleartext!");
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed["newznab"], serde_json::json!([]));
        let enc = parsed["torznab"][0]["apiKeyEnc"].as_str().unwrap();
        let dec = state.secret_box.decrypt_string(enc).unwrap();
        assert_eq!(dec, b"correct-key");
    }

    #[tokio::test]
    async fn json_response_is_flagged_as_wrong_format() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"oops": true})),
            )
            .mount(&server)
            .await;

        let body = serde_json::json!({
            "newznab": [{"name": "j", "url": server.uri(), "apiKey": "k"}],
        })
        .to_string();
        let (_, v) = post_admin(state, "/api/v1/setup/indexers/validate", &body, Some(&sess)).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["newznab"][0]["error"], "json_response_expected_xml");
    }

    #[tokio::test]
    async fn unreachable_host_reports_connect_failed() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;
        let body = serde_json::json!({
            "torznab": [{"name": "dead", "url": "http://127.0.0.1:1", "apiKey": "k"}],
        })
        .to_string();
        let (_, v) = post_admin(state, "/api/v1/setup/indexers/validate", &body, Some(&sess)).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["torznab"][0]["error"], "connect_failed");
    }

    #[test]
    fn newznab_error_parsing() {
        let root = r#"<?xml version="1.0"?><error code="910" description="Unauthorized"/>"#;
        assert_eq!(
            parse_newznab_error(root).as_deref(),
            Some("error:Unauthorized")
        );
        let wrapped = r#"<rss><error code="910"/></rss>"#;
        assert_eq!(parse_newznab_error(wrapped).as_deref(), Some("error_code:910"));
        assert_eq!(parse_newznab_error("<rss></rss>"), None);
        assert_eq!(count_rss_items(RSS_BODY), 2);
        assert!(is_prowlarr_url("http://host:9696/api"));
        assert!(!is_prowlarr_url("http://host:8080/api"));
    }

    #[test]
    fn network_error_classification() {
        let (t, _) = network_error_info("dial tcp: connection refused", "http://x");
        assert_eq!(t, "network");
        let (t, _) = network_error_info("lookup: no such host", "http://x");
        assert_eq!(t, "dns");
    }
}
