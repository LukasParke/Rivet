//! Diagnostics endpoints (admin): redacted settings bundle and a connectivity
//! test.
//!
//! Review fixes over the original:
//! - `/diagnostics/test-connection` validates the target URL through
//!   [`crate::security::ssrf::validate_url`] FIRST and refuses disallowed
//!   targets; the HTTP probe follows redirects only via
//!   [`crate::security::ssrf::get_validated`] (IP-pinned, per-hop re-check).
//! - The container network-interface disclosure (`networkInterfaces`) is gone.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::error::{ApiResult, AppError};
use crate::httpapi::middleware::auth::Admin;
use crate::security::ssrf::{self, SsrfError};
use crate::state::AppState;
use crate::storage::audit::{self, AuditEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/diagnostics/bundle", get(diagnostics_bundle))
        .route("/diagnostics/test-connection", post(test_connection))
}

// ---------------------------------------------------------------------------
// GET /diagnostics/bundle
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[derive(sqlx::FromRow)]
struct JobErrorRow {
    id: String,
    kind: String,
    state: String,
    attempt: i64,
    #[serde(rename = "lastError")]
    last_error: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

#[derive(Debug, Serialize)]
#[derive(sqlx::FromRow)]
struct AuditRow {
    id: String,
    #[serde(rename = "actorUserId", skip_serializing_if = "Option::is_none")]
    actor_user_id: Option<String>,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(rename = "userAgent", skip_serializing_if = "Option::is_none")]
    user_agent: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Debug, Serialize)]
struct DiagnosticsResponse {
    #[serde(rename = "generatedAt")]
    generated_at: String,
    #[serde(rename = "dryRun")]
    dry_run: bool,
    #[serde(rename = "dryRunFixturePathSet")]
    dry_run_fixture_path_set: bool,
    setup: BTreeMap<String, String>,
    settings: BTreeMap<String, Value>,
    #[serde(rename = "queueDepth")]
    queue_depth: BTreeMap<String, i64>,
    #[serde(rename = "recentJobs")]
    recent_jobs: Vec<JobErrorRow>,
    #[serde(rename = "recentAudit")]
    recent_audit: Vec<AuditRow>,
    metrics: BTreeMap<String, i64>,
}

/// Settings keys surfaced by the bundle, in the original's order.
const SETUP_KEYS: &[&str] = &[
    "setup.paths",
    "setup.clients",
    "setup.indexers",
    "setup.metadata",
    "setup.policies",
    "setup.report.latest",
    "auth.registration_open",
];

async fn diagnostics_bundle(Admin(_user): Admin, State(state): State<AppState>) -> ApiResult<Json<DiagnosticsResponse>> {
    let mut setup = BTreeMap::new();
    let mut settings = BTreeMap::new();
    for key in SETUP_KEYS {
        match crate::storage::settings::get(&state.db, key).await {
            Err(_) => {
                setup.insert((*key).to_string(), "error".to_string());
                continue;
            }
            Ok(None) => {
                setup.insert((*key).to_string(), "missing".to_string());
                continue;
            }
            Ok(Some(raw)) => {
                setup.insert((*key).to_string(), "set".to_string());
                match serde_json::from_str::<Value>(&raw) {
                    // Redact sensitive fields before exposing anything.
                    Ok(parsed) => {
                        settings.insert((*key).to_string(), redact(parsed));
                    }
                    Err(_) => {
                        let leaky = key.contains("secret") || key.contains("key");
                        settings.insert(
                            (*key).to_string(),
                            Value::String(if leaky { "***".to_string() } else { raw }),
                        );
                    }
                }
            }
        }
    }

    let queue_depth = job_counts_by_state(&state.db)
        .await
        .unwrap_or_default();
    let recent_jobs = recent_job_errors(&state.db, 25)
        .await
        .unwrap_or_default();
    let recent_audit = recent_audit_rows(&state.db, 25)
        .await
        .unwrap_or_default();

    let m = &state.metrics;
    let mut metrics = BTreeMap::new();
    metrics.insert(
        "jobs_enqueued_total".to_string(),
        m.jobs_enqueued_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    metrics.insert(
        "jobs_claimed_total".to_string(),
        m.jobs_claimed_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    metrics.insert(
        "jobs_completed_total".to_string(),
        m.jobs_completed_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    metrics.insert(
        "jobs_retry_scheduled_total".to_string(),
        m.jobs_retry_scheduled_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );
    metrics.insert(
        "ws_events_published_total".to_string(),
        m.ws_events_published_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
    );

    Ok(Json(DiagnosticsResponse {
        generated_at: crate::timeutil::fmt(chrono::Utc::now()),
        dry_run: state.config.dry_run,
        dry_run_fixture_path_set: state
            .config
            .dry_run_fixture_path
            .as_deref()
            .map(str::trim)
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        setup,
        settings,
        queue_depth,
        recent_jobs,
        recent_audit,
        metrics,
    }))
}

/// Recursively replaces sensitive-looking leaf values with `***`:
/// keys ending in `enc`, or containing `password`, `apikey`, `token`,
/// `secret`. Objects/arrays are always descended into so redaction never
/// destroys diagnostic structure (e.g. a `tokens` list keeps its entries).
fn redact(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, vv) in map {
                let lk = k.to_ascii_lowercase();
                let sensitive = lk.ends_with("enc")
                    || lk.contains("password")
                    || lk.contains("apikey")
                    || lk.contains("token")
                    || lk.contains("secret");
                if sensitive && !vv.is_object() && !vv.is_array() {
                    out.insert(k, Value::String("***".to_string()));
                } else {
                    out.insert(k, redact(vv));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

async fn job_counts_by_state(
    pool: &sqlx::SqlitePool,
) -> Result<BTreeMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT state, COUNT(1) FROM jobs GROUP BY state")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

async fn recent_job_errors(
    pool: &sqlx::SqlitePool,
    limit: i64,
) -> Result<Vec<JobErrorRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 25 } else { limit };
    sqlx::query_as::<_, JobErrorRow>(
        "SELECT id, kind, state, attempt, COALESCE(last_error,'') AS last_error, updated_at
         FROM jobs
         WHERE last_error IS NOT NULL AND last_error != ''
         ORDER BY updated_at DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

async fn recent_audit_rows(
    pool: &sqlx::SqlitePool,
    limit: i64,
) -> Result<Vec<AuditRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 25 } else { limit };
    sqlx::query_as::<_, AuditRow>(
        "SELECT id, actor_user_id, action, ip, user_agent, created_at
         FROM audit_events
         ORDER BY created_at DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// POST /diagnostics/test-connection
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
struct TestConnectionRequest {
    #[serde(default)]
    url: String,
    #[serde(default, rename = "apiKey")]
    api_key: String,
}

#[derive(Debug, Serialize)]
struct TestConnectionResponse {
    url: String,
    #[serde(rename = "dnsResolution")]
    dns_resolution: DnsInfo,
    tcp_test: TcpTestResult,
    #[serde(rename = "httpTestDefault")]
    http_test_default: HttpTestResult,
    #[serde(rename = "httpTestCustom")]
    http_test_custom: HttpTestResult,
    summary: String,
}

#[derive(Debug, Serialize)]
struct DnsInfo {
    hostname: String,
    ips: Vec<String>,
    #[serde(rename = "error", skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct TcpTestResult {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<String>,
}

#[derive(Debug, Serialize)]
struct HttpTestResult {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<String>,
}


impl HttpTestResult {
    fn failure(error: String, duration: Duration) -> Self {
        Self {
            success: false,
            status_code: None,
            error: Some(error),
            duration: Some(fmt_duration(duration)),
        }
    }
}

/// Formats a duration like Go's `time.Duration.String()` ("1.234ms").
fn fmt_duration(d: Duration) -> String {
    if d.as_nanos() < 1_000 {
        format!("{}ns", d.as_nanos())
    } else if d.as_micros() < 1_000 {
        format!("{}µs", d.as_micros())
    } else {
        format!("{}ms", d.as_millis())
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REDIRECT_HOPS: usize = 5;

async fn test_connection(
    Admin(user): Admin,
    State(state): State<AppState>,
    Json(req): Json<TestConnectionRequest>,
) -> ApiResult<Response> {
    do_test_connection(req, &state, Some(&user.user.id)).await
}

fn ssrf_reject(e: SsrfError) -> AppError {
    match e {
        SsrfError::Network => AppError::BadRequest("network_error".to_string()),
        SsrfError::BadScheme | SsrfError::NoHost => AppError::BadRequest("invalid_url".to_string()),
        SsrfError::DisallowedAddress => AppError::BadRequest("disallowed_target".to_string()),
        SsrfError::DnsFailure => AppError::BadRequest("dns_failure".to_string()),
    }
}

async fn do_test_connection(
    req: TestConnectionRequest,
    state: &AppState,
    actor_user_id: Option<&str>,
) -> ApiResult<Response> {
    // REVIEW FIX: validate BEFORE any network activity. A URL that fails the
    // SSRF policy never reaches DNS resolution or a socket here.
    let validated = ssrf::validate_url(req.url.trim(), state.policy)
        .await
        .map_err(ssrf_reject)?;

    // Audit the attempt (best-effort, like every other audit write).
    let _ = audit::insert(
        &state.db,
        AuditEvent {
            actor_user_id: actor_user_id.map(str::to_string),
            action: "diagnostics.test_connection".to_string(),
            target_type: Some("url".to_string()),
            target_id: Some(validated.url.host_str().unwrap_or("").to_string()),
            details_json: Some(json!({ "url": validated.url.as_str() }).to_string()),
            ..AuditEvent::default()
        },
    )
    .await;

    let host = validated.url.host_str().unwrap_or_default().to_string();
    let port = validated.url.port_or_known_default().unwrap_or(80);

    // DNS resolution info comes straight from the pinned validation result —
    // exactly the addresses any later connection may use.
    let dns_info = DnsInfo {
        hostname: host.clone(),
        ips: validated.ips.iter().map(|ip| ip.to_string()).collect(),
        error: None,
    };

    // TCP connect against one of the PINNED IPs (no fresh resolution).
    let tcp_start = Instant::now();
    let mut tcp_result = TcpTestResult {
        success: false,
        error: None,
        duration: None,
    };
    let mut last_tcp_err = None;
    for ip in &validated.ips {
        let addr = SocketAddr::new(*ip, port);
        match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                drop(stream);
                tcp_result.success = true;
                break;
            }
            Ok(Err(e)) => last_tcp_err = Some(e.to_string()),
            Err(_) => last_tcp_err = Some("timed out".to_string()),
        }
    }
    tcp_result.duration = Some(fmt_duration(tcp_start.elapsed()));
    if !tcp_result.success {
        tcp_result.error = Some(last_tcp_err.unwrap_or_else(|| "no resolved address".into()));
    }

    // HTTP probe through get_validated only: every redirect hop is
    // re-resolved and re-pinned. NOTE: the API key is intentionally NOT sent;
    // see the module docs / integration notes.
    let http_start = Instant::now();
    let probe_url = join_probe_path(validated.url.as_str());
    let http_result = match ssrf::get_validated(&probe_url, state.policy, PROBE_TIMEOUT, MAX_REDIRECT_HOPS)
        .await
    {
        Ok(resp) => {
            let code = resp.status().as_u16();
            HttpTestResult {
                // Any well-formed HTTP response proves reachability; indexers
                // answer 401 without a key, which still means "connected".
                success: resp.status().is_success() || code == 401,
                status_code: Some(code),
                error: None,
                duration: Some(fmt_duration(http_start.elapsed())),
            }
        }
        Err(e) => HttpTestResult::failure(e.to_string(), http_start.elapsed()),
    };

    // Wire compatibility: the original reported two probes (default client +
    // source-pinned transport). Only the pinned transport survives the SSRF
    // review — an unpinned "default client" request was the vulnerability.
    // Both fields report the single validated probe.
    let http_test_custom = clone_http_result(&http_result);

    let mut summary = if tcp_result.success && http_result.success {
        "All tests passed - connection is working".to_string()
    } else if tcp_result.success && !http_result.success {
        format!(
            "TCP connection works but HTTP failed: {}",
            http_result.error.clone().unwrap_or_default()
        )
    } else if !tcp_result.success {
        format!(
            "TCP connection failed: {}",
            tcp_result.error.clone().unwrap_or_default()
        )
    } else {
        "Connection test completed with mixed results".to_string()
    };
    if http_result.status_code == Some(401) && http_result.success {
        summary.push_str(" (HTTP reachable; indexer returned 401 — check API key)");
    }

    Ok((
        StatusCode::OK,
        Json(TestConnectionResponse {
            url: req.url,
            dns_resolution: dns_info,
            tcp_test: tcp_result,
            http_test_default: http_result,
            http_test_custom,
            summary,
        }),
    )
        .into_response())
}

fn clone_http_result(r: &HttpTestResult) -> HttpTestResult {
    HttpTestResult {
        success: r.success,
        status_code: r.status_code,
        error: r.error.clone(),
        duration: r.duration.clone(),
    }
}

/// Appends the indexer probe path, tolerating trailing slashes.
fn join_probe_path(base: &str) -> String {
    format!("{}/api/v1/indexer", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Role;

    #[test]
    fn redacts_sensitive_keys_recursively() {
        let v = json!({
            "qbittorrent": {"url": "http://x", "passwordEnc": "cipher", "apiKey": "k"},
            "tokens": [{"token": "t"}, {"ok": "keep"}],
            "Secret": "s",
            "note": "plain"
        });
        let out = redact(v);
        assert_eq!(out["qbittorrent"]["url"], "http://x");
        assert_eq!(out["qbittorrent"]["passwordEnc"], "***");
        assert_eq!(out["qbittorrent"]["apiKey"], "***");
        assert_eq!(out["tokens"][0]["token"], "***");
        assert_eq!(out["tokens"][1]["ok"], "keep");
        assert_eq!(out["Secret"], "***");
        assert_eq!(out["note"], "plain");
    }

    #[test]
    fn non_enc_suffix_is_not_redacted() {
        let out = redact(json!({"enclosureUrl": "http://magnet"}));
        assert_eq!(out["enclosureUrl"], "http://magnet");
    }

    #[test]
    fn durations_render_like_go() {
        assert_eq!(fmt_duration(Duration::from_nanos(500)), "500ns");
        assert_eq!(fmt_duration(Duration::from_micros(12)), "12µs");
        assert_eq!(fmt_duration(Duration::from_millis(1234)), "1234ms");
    }

    #[tokio::test]
    async fn test_connection_rejects_disallowed_target_before_network() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("d.db").display()))
            .await
            .unwrap();
        crate::storage::migrations::apply(&pool).await.unwrap();
        let state = AppState {
            db: pool,
            secret_box: std::sync::Arc::new(
                crate::security::secrets::SecretBox::new(None).unwrap(),
            ),
            policy: crate::security::OutboundPolicy::production(),
            hub: std::sync::Arc::new(crate::ws::hub::Hub::new()),
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
                allow_internal_urls: false,
            }),
        };
        let req = TestConnectionRequest {
            url: "http://127.0.0.1:8080/".to_string(),
            api_key: String::new(),
        };
        let err = do_test_connection(req, &state, Some("u1"))
            .await
            .expect_err("loopback must be rejected under production policy");
        let (code, status) = err.code();
        assert_eq!(code.to_string(), "disallowed_target");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    const _: Option<Role> = None;
}
