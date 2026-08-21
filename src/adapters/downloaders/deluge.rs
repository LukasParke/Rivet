//! Deluge download client (JSON-RPC v2 over HTTP).
//!
//! Wire protocol: `POST {url}/json` with `{"method": ..., "params": [...],
//! "id": n}`. Authentication is `auth.login`; a successful login sets a
//! `_session_id` cookie that MUST accompany every subsequent request. An
//! expired session surfaces as RPC error code 1 (`NotAuthenticated`); this
//! adapter transparently re-authenticates once and replays the call.
//!
//! Torrent addition deliberately supports **magnet URIs only**
//! (`core.add_torrent_magnet`). Adding `.torrent` *files* would require
//! shipping file bytes through `core.add_torrent_file` with base64 payloads;
//! Rivet's pipeline only ever holds release URLs/magnets, so file URLs fail
//! fast with [`AdapterError::Unsupported`] instead of pretending support.
//!
//! All HTTP goes through the SSRF-pinned client pattern from
//! [`crate::security::ssrf`]: the configured URL is resolved and policy-checked
//! once at construction, then every request dials only those pinned IPs.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{COOKIE, SET_COOKIE};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use crate::adapters::{AdapterError, Downloader, TorrentInfo};
use crate::security::ssrf::{validate_url, OutboundPolicy, ValidatedTarget};

/// Deluge WebUi RPC endpoint path.
const RPC_PATH: &str = "json";
/// Deluge RPC error code signalling a missing/expired session.
const NOT_AUTHENTICATED: i64 = 1;

/// Fields requested from `core.get_torrents_status`.
const TORRENT_KEYS: [&str; 6] = [
    "name",
    "progress",
    "state",
    "save_path",
    "total_size",
    "total_done",
];

/// How long a single RPC round-trip may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

/// SSRF-pinned Deluge client. Clone-free shared handle handed out as `Arc`.
pub struct DelugeClient {
    /// Pre-resolved, policy-checked base URL; every request dials these IPs.
    target: ValidatedTarget,
    rpc_url: Url,
    username: String,
    password: String,
    /// Current `_session_id` cookie value (`Cookie:` form), if logged in.
    session_cookie: tokio::sync::Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl DelugeClient {
    /// Validates the configured URL against `policy`, resolving and pinning
    /// every address before any request is possible.
    pub async fn new(
        cfg_json: &serde_json::Value,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn Downloader>, AdapterError> {
        let url = cfg_json
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::InvalidConfig("deluge: missing url".into()))?
            .trim()
            .trim_end_matches('/')
            .to_string();
        let username = cfg_json
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let password = cfg_json
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let target = validate_url(&url, policy)
            .await
            .map_err(|e| AdapterError::InvalidConfig(format!("deluge url rejected: {e}")))?;
        let rpc_url = target
            .url
            .join(RPC_PATH)
            .map_err(|e| AdapterError::InvalidConfig(format!("deluge url invalid: {e}")))?;

        Ok(Arc::new(DelugeClient {
            target,
            rpc_url,
            username,
            password,
            session_cookie: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        }))
    }

    /// Runs one authenticated RPC call, transparently re-authenticating once
    /// when Deluge reports the session expired.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        if self.session_cookie.lock().await.is_none() {
            self.login().await?;
        }
        match self.call(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(AdapterError::Auth) => {
                tracing::debug!(method, "deluge session expired; re-authenticating");
                *self.session_cookie.lock().await = None;
                self.login().await?;
                self.call(method, params).await
            }
            Err(e) => Err(e),
        }
    }

    /// Sends a single request carrying the current session cookie (if any).
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"method": method, "params": params, "id": id});

        let client = build_pinned_client(&self.target, REQUEST_TIMEOUT);
        let mut req = client.post(self.rpc_url.clone()).json(&body);
        if let Some(cookie) = self.session_cookie.lock().await.clone() {
            req = req.header(COOKIE, cookie);
        }
        let resp = req.send().await?;

        Self::check_http_status(resp.status())?;
        let parsed: JsonRpcResponse = resp
            .json()
            .await
            .map_err(|e| AdapterError::Parse(format!("deluge rpc response: {e}")))?;
        Self::unwrap_result(parsed)
    }

    /// Authenticates and stores the `_session_id` cookie.
    async fn login(&self) -> Result<(), AdapterError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "method": "auth.login",
            "params": [self.username, self.password],
            "id": id,
        });

        let client = build_pinned_client(&self.target, REQUEST_TIMEOUT);
        let resp = client.post(self.rpc_url.clone()).json(&body).send().await?;

        Self::check_http_status(resp.status())?;
        let cookie = extract_session_cookie(resp.headers());
        let parsed: JsonRpcResponse = resp
            .json()
            .await
            .map_err(|e| AdapterError::Parse(format!("deluge login response: {e}")))?;
        let logged_in = Self::unwrap_result(parsed)?
            .as_bool()
            .ok_or_else(|| AdapterError::Parse("deluge login returned non-bool".into()))?;
        if !logged_in {
            return Err(AdapterError::Auth);
        }
        let Some(cookie) = cookie else {
            return Err(AdapterError::Parse(
                "deluge login succeeded without a _session_id cookie".into(),
            ));
        };

        *self.session_cookie.lock().await = Some(cookie);
        Ok(())
    }

    fn check_http_status(status: StatusCode) -> Result<(), AdapterError> {
        match status {
            s if s == StatusCode::UNAUTHORIZED || s == StatusCode::FORBIDDEN => {
                Err(AdapterError::Auth)
            }
            s if s.is_success() => Ok(()),
            s => Err(AdapterError::Network(format!("deluge returned HTTP {s}"))),
        }
    }

    fn unwrap_result(parsed: JsonRpcResponse) -> Result<Value, AdapterError> {
        match parsed.error {
            None => Ok(parsed.result),
            Some(err) if err.code == NOT_AUTHENTICATED => Err(AdapterError::Auth),
            Some(err) => Err(AdapterError::Network(format!(
                "deluge rpc error {}: {}",
                err.code,
                err.message.unwrap_or_else(|| "unknown".into())
            ))),
        }
    }
}

/// Extracts a `Cookie:`-ready `_session_id=...` value from `Set-Cookie`
/// headers.
fn extract_session_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for value in headers.get_all(SET_COOKIE) {
        let s = value.to_str().ok()?;
        for attr in s.split(';') {
            let attr = attr.trim();
            if let Some(id) = attr.strip_prefix("_session_id=") {
                if !id.is_empty() {
                    return Some(format!("_session_id={id}"));
                }
            }
        }
    }
    None
}

/// Deluge reports progress as 0–100; canonical snapshots use 0–1.
fn deluge_progress_to_fraction(pct: f64) -> f64 {
    (pct / 100.0).clamp(0.0, 1.0)
}

/// Canonicalizes Deluge state labels (`Downloading`, `Seeding`, `Paused`,
/// `Checking`, ...) to their lowercased form; unknown states pass through
/// lowercased rather than being dropped.
fn map_state(state: &str) -> String {
    state.trim().to_ascii_lowercase()
}

/// Deluge torrent ids are info-hashes for libtorrent-backed daemons; flag
/// them so the monitor can correlate with release info-hashes.
fn looks_like_info_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn entry_to_info(id: &str, entry: &Value) -> Result<TorrentInfo, AdapterError> {
    let obj = entry
        .as_object()
        .ok_or_else(|| AdapterError::Parse("deluge torrent entry is not an object".into()))?;
    let num = |key: &str| obj.get(key).and_then(Value::as_f64);
    let pct = num("progress")
        .ok_or_else(|| AdapterError::Parse("deluge torrent missing progress".into()))?;
    let int_or = |key: &str| obj.get(key).and_then(Value::as_i64).unwrap_or(0);

    Ok(TorrentInfo {
        id: id.to_string(),
        hash: looks_like_info_hash(id).then(|| id.to_string()),
        name: obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        progress: deluge_progress_to_fraction(pct),
        state: map_state(
            obj.get("state")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        size_bytes: int_or("total_size"),
        downloaded_bytes: int_or("total_done"),
        content_path: obj
            .get("save_path")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[async_trait]
impl Downloader for DelugeClient {
    fn name(&self) -> &str {
        "deluge"
    }

    /// Magnet URIs only; see the module docs for why `.torrent` file URLs are
    /// rejected outright.
    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError> {
        if !url.starts_with("magnet:") {
            return Err(AdapterError::Unsupported(
                "deluge: only magnet URIs are supported; .torrent file URLs cannot be added remotely".into(),
            ));
        }
        let result = self
            .rpc("core.add_torrent_magnet", json!([url, {}]))
            .await?;
        // Success yields the torrent id; duplicates/failures yield null.
        Ok(result.as_str().map(str::to_string))
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        let result = self
            .rpc(
                "core.get_torrents_status",
                json!([{ "id": [id] }, TORRENT_KEYS]),
            )
            .await?;
        let obj = result.as_object().ok_or_else(|| {
            AdapterError::Parse("deluge get_torrents_status returned non-object".into())
        })?;
        match obj.iter().next() {
            None => Ok(None),
            Some((torrent_id, entry)) => Ok(Some(entry_to_info(torrent_id, entry)?)),
        }
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        let result = self
            .rpc("core.get_torrents_status", json!([{}, TORRENT_KEYS]))
            .await?;
        let obj = result.as_object().ok_or_else(|| {
            AdapterError::Parse("deluge get_torrents_status returned non-object".into())
        })?;
        obj.iter()
            .map(|(id, entry)| entry_to_info(id, entry))
            .collect()
    }

    async fn resume(&self, id: &str) -> Result<(), AdapterError> {
        self.rpc("core.resume_torrent", json!([[id]]))
            .await
            .map(|_| ())
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        self.rpc("core.pause_torrent", json!([[id]]))
            .await
            .map(|_| ())
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        self.rpc("core.remove_torrent", json!([id, delete_files]))
            .await
            .map(|_| ())
    }

    /// Wizard connectivity check: authenticate, then ping the daemon.
    async fn test(&self) -> Result<(), AdapterError> {
        self.login().await?;
        self.rpc("daemon.info", json!([])).await.map(|_| ())
    }
}

/// Resolver that only ever yields the addresses pinned at construction time.
///
/// Mirrors `crate::security::ssrf`'s private `PinnedResolver`; kept local so
/// adapters stay self-contained until the helper is hoisted into `ssrf`.
#[derive(Clone)]
struct PinnedResolver {
    host: String,
    ips: Vec<IpAddr>,
    port: u16,
}

impl Resolve for PinnedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = self.host.clone();
        let ips = self.ips.clone();
        let port = self.port;
        Box::pin(async move {
            if name.as_str().eq_ignore_ascii_case(&host) {
                let addrs: Addrs =
                    Box::new(ips.into_iter().map(move |ip| SocketAddr::new(ip, port)));
                Ok(addrs)
            } else {
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "hostname not pinned by SSRF policy",
                ))
                    as Box<dyn std::error::Error + Send + Sync>)
            }
        })
    }
}

/// Builds a client that can only connect to the pre-validated addresses of
/// `target` (mirrors `security::ssrf::build_pinned_client`).
fn build_pinned_client(target: &ValidatedTarget, timeout: Duration) -> reqwest::Client {
    let port = target
        .url
        .port_or_known_default()
        .unwrap_or(if target.url.scheme() == "https" { 443 } else { 80 });
    let resolver = Arc::new(PinnedResolver {
        host: target.url.host_str().unwrap_or_default().to_string(),
        ips: target.ips.clone(),
        port,
    });
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(resolver)
        .build()
        .expect("static client configuration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(url: &str) -> serde_json::Value {
        json!({"url": url, "username": "user", "password": "secret"})
    }

    fn rpc_body(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"id": 1, "result": result, "error": null}))
    }

    async fn spawn_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"method\":\"auth.login\""))
            .respond_with(
                rpc_body(json!(true)).append_header(
                    "Set-Cookie",
                    "_session_id=abc123; Path=/; HttpOnly",
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"method\":\"daemon.info\""))
            .respond_with(rpc_body(json!("daemon-id-1")))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn login_add_magnet_and_get_all_flow() {
        let server = spawn_server().await;

        // add_torrent_magnet requires the session cookie from login.
        Mock::given(method("POST"))
            .and(body_string_contains("\"method\":\"core.add_torrent_magnet\""))
            .and(header("cookie", "_session_id=abc123"))
            .respond_with(rpc_body(json!("e2467cbf021192c241367c8926dc875f95133158")))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(body_string_contains("\"method\":\"core.get_torrents_status\""))
            .and(header("cookie", "_session_id=abc123"))
            .respond_with(rpc_body(json!({
                "e2467cbf021192c241367c8926dc875f95133158": {
                    "name": "Debian ISO",
                    "progress": 55.5,
                    "state": "Downloading",
                    "save_path": "/data/incoming",
                    "total_size": 1000,
                    "total_done": 555
                },
                "not-a-hash-but-an-id": {
                    "name": "Ubuntu ISO",
                    "progress": 100.0,
                    "state": "Seeding",
                    "save_path": "/data/library",
                    "total_size": 800,
                    "total_done": 800
                }
            })))
            .mount(&server)
            .await;

        let client = DelugeClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .expect("construction validates url");

        let added = client
            .add_url("magnet:?xt=urn:btih:e2467cbf021192c241367c8926dc875f95133158")
            .await
            .expect("add magnet");
        assert_eq!(
            added.as_deref(),
            Some("e2467cbf021192c241367c8926dc875f95133158")
        );

        let all = client.get_all().await.expect("list torrents");
        assert_eq!(all.len(), 2);

        let debian = all
            .iter()
            .find(|t| t.id == "e2467cbf021192c241367c8926dc875f95133158")
            .expect("debian torrent");
        assert_eq!(debian.name, "Debian ISO");
        assert_eq!(debian.state, "downloading"); // lowercased canonical state
        assert!((debian.progress - 0.555).abs() < 1e-9); // deluge 0-100 -> 0-1
        assert_eq!(debian.size_bytes, 1000);
        assert_eq!(debian.downloaded_bytes, 555);
        assert_eq!(
            debian.hash.as_deref(),
            Some("e2467cbf021192c241367c8926dc875f95133158")
        );
        assert_eq!(debian.content_path.as_deref(), Some("/data/incoming"));

        let ubuntu = all.iter().find(|t| t.id == "not-a-hash-but-an-id").unwrap();
        assert_eq!(ubuntu.state, "seeding");
        assert_eq!(ubuntu.hash, None); // id is not an info-hash

        client.test().await.expect("test() re-authenticates");
    }

    #[tokio::test]
    async fn torrent_file_url_is_rejected_as_unsupported() {
        let server = spawn_server().await;
        let client = DelugeClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .unwrap();

        let err = client
            .add_url("https://example.org/debian.torrent")
            .await
            .expect_err(".torrent urls must fail fast");
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn bad_credentials_surface_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"method\":\"auth.login\""))
            .respond_with(rpc_body(json!(false)))
            .mount(&server)
            .await;

        let client = DelugeClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .unwrap();
        let err = client
            .test()
            .await
            .expect_err("wrong password must yield Auth");
        assert!(matches!(err, AdapterError::Auth));
    }

    #[tokio::test]
    async fn expired_session_reauthenticates_once() {
        let server = spawn_server().await;

        fn not_authenticated() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "result": null,
                "error": {"code": 1, "message": "NotAuthenticated"}
            }))
        }

        // wiremock answers with the FIRST matching mock in registration
        // order: the first listing request is told its session expired
        // (consumed once), after which every cookie-carrying call succeeds.
        Mock::given(method("POST"))
            .and(header("cookie", "_session_id=abc123"))
            .and(body_string_contains("\"method\":\"core.get_torrents_status\""))
            .respond_with(not_authenticated())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("cookie", "_session_id=abc123"))
            .respond_with(rpc_body(json!({})))
            .mount(&server)
            .await;

        let client = DelugeClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .unwrap();
        // The daemon invalidates the first authenticated call; the client
        // re-authenticates and replays it exactly once.
        let all = client.get_all().await.expect("retry after re-login");
        assert!(all.is_empty());

        // Pause/resume/remove round-trip through the refreshed session.
        client.pause("e2467cbf021192c241367c8926dc875f95133158").await.unwrap();
        client.resume("e2467cbf021192c241367c8926dc875f95133158").await.unwrap();
        client.delete("e2467cbf021192c241367c8926dc875f95133158", true).await.unwrap();
    }

    #[test]
    fn state_mapping_is_lowercase_passthrough() {
        assert_eq!(map_state("Downloading"), "downloading");
        assert_eq!(map_state("Seeding"), "seeding");
        assert_eq!(map_state("Paused"), "paused");
        assert_eq!(map_state("Checking"), "checking");
        assert_eq!(map_state("Moving"), "moving");
    }
}
