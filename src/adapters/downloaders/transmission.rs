//! Transmission download client (Transmission RPC).
//!
//! Wire protocol: `POST {url}/transmission/rpc` with
//! `{"method": ..., "arguments": {...}}`. The daemon guards against CSRF with
//! a rotating `X-Transmission-Session-Id` header: requests without (or with a
//! stale) token are answered `409` carrying the current token, which the
//! client must echo on the retried request. This adapter performs that
//! handshake lazily and transparently retries once per call. Credentials use
//! HTTP basic auth when a username is configured.
//!
//! State mapping to canonical lowercase strings:
//! - status 0 (`stopped`) → `stoppeddl`, or `stoppedup` once fully downloaded;
//! - statuses 1–2 (check wait / checking) → `checking`;
//! - statuses 3–4 (download wait / downloading) → `downloading`;
//! - statuses 5–6 (seed wait / seeding) → `seeding`.
//!
//! All HTTP goes through the SSRF-pinned client pattern from
//! [`crate::security::ssrf`]: the configured URL is resolved and policy-checked
//! once at construction, then every request dials only those pinned IPs.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::adapters::{AdapterError, Downloader, TorrentInfo};
use crate::security::ssrf::{validate_url, OutboundPolicy, ValidatedTarget};

/// Transmission RPC endpoint path.
const RPC_PATH: &str = "transmission/rpc";
/// Session-id header used for the CSRF handshake.
const SESSION_HEADER: &str = "X-Transmission-Session-Id";

/// Fields requested from `torrent-get`.
const TORRENT_FIELDS: [&str; 8] = [
    "id",
    "hashString",
    "name",
    "percentDone",
    "status",
    "totalSize",
    "downloadedEver",
    "downloadDir",
];

/// How long a single RPC round-trip may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct TrResponse {
    result: String,
    #[serde(default)]
    arguments: Value,
}

/// SSRF-pinned Transmission client. Clone-free shared handle handed out as
/// `Arc`.
pub struct TransmissionClient {
    /// Pre-resolved, policy-checked base URL; every request dials these IPs.
    target: ValidatedTarget,
    rpc_url: Url,
    username: String,
    password: String,
    /// Current `X-Transmission-Session-Id` token, if the handshake ran.
    session_id: Mutex<Option<String>>,
}

impl TransmissionClient {
    /// Validates the configured URL against `policy`, resolving and pinning
    /// every address before any request is possible.
    pub async fn new(
        cfg_json: &serde_json::Value,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn Downloader>, AdapterError> {
        let url = cfg_json
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::InvalidConfig("transmission: missing url".into()))?
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
            .map_err(|e| AdapterError::InvalidConfig(format!("transmission url rejected: {e}")))?;
        let rpc_url = target
            .url
            .join(RPC_PATH)
            .map_err(|e| AdapterError::InvalidConfig(format!("transmission url invalid: {e}")))?;

        Ok(Arc::new(TransmissionClient {
            target,
            rpc_url,
            username,
            password,
            session_id: Mutex::new(None),
        }))
    }

    /// Runs one RPC call, performing (or refreshing) the session-id handshake
    /// on `409` and retrying exactly once.
    async fn request(&self, method: &str, arguments: Value) -> Result<Value, AdapterError> {
        for _ in 0..2 {
            let client = build_pinned_client(&self.target, REQUEST_TIMEOUT);
            let mut req = client
                .post(self.rpc_url.clone())
                .json(&json!({"method": method, "arguments": arguments}));
            if !self.username.is_empty() {
                req = req.basic_auth(&self.username, Some(&self.password));
            }
            if let Some(token) = self.session_id.lock().await.clone() {
                req = req.header(SESSION_HEADER, token);
            }
            let resp = req.send().await?;

            let status = resp.status();
            if status == StatusCode::CONFLICT {
                // Handshake: the daemon hands us the fresh token here.
                let token = resp
                    .headers()
                    .get(SESSION_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        AdapterError::Parse(
                            "transmission 409 without X-Transmission-Session-Id".into(),
                        )
                    })?
                    .to_string();
                *self.session_id.lock().await = Some(token);
                continue;
            }
            match status {
                s if s == StatusCode::UNAUTHORIZED || s == StatusCode::FORBIDDEN => {
                    return Err(AdapterError::Auth);
                }
                s if !s.is_success() => {
                    return Err(AdapterError::Network(format!(
                        "transmission returned HTTP {s}"
                    )));
                }
                _ => {}
            }

            let parsed: TrResponse = resp
                .json()
                .await
                .map_err(|e| AdapterError::Parse(format!("transmission rpc response: {e}")))?;
            if parsed.result != "success" {
                return Err(AdapterError::Network(format!(
                    "transmission rpc error: {}",
                    parsed.result
                )));
            }
            return Ok(parsed.arguments);
        }
        Err(AdapterError::Network(
            "transmission session handshake did not settle after one retry".into(),
        ))
    }

    async fn torrent_get(&self, extra_args: Value) -> Result<Vec<TorrentInfo>, AdapterError> {
        let mut args = json!({"fields": TORRENT_FIELDS});
        for (k, v) in extra_args.as_object().into_iter().flatten() {
            args[k] = v.clone();
        }
        let arguments = self.request("torrent-get", args).await?;
        let torrents = arguments
            .get("torrents")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AdapterError::Parse("transmission torrent-get returned no torrents array".into())
            })?;
        torrents.iter().map(torrent_to_info).collect()
    }
}

/// Maps a Transmission numeric status plus progress onto canonical state
/// strings; see the module docs for the table.
fn map_status(status: i64, progress: f64) -> String {
    match status {
        0 if progress >= 0.999 => "stoppedup".to_string(),
        0 => "stoppeddl".to_string(),
        1 | 2 => "checking".to_string(),
        3 | 4 => "downloading".to_string(),
        5 | 6 => "seeding".to_string(),
        other => format!("unknown{other}"),
    }
}

fn torrent_to_info(t: &Value) -> Result<TorrentInfo, AdapterError> {
    let obj = t
        .as_object()
        .ok_or_else(|| AdapterError::Parse("transmission torrent entry is not an object".into()))?;
    let id = obj
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| AdapterError::Parse("transmission torrent missing id".into()))?;
    let progress = obj
        .get("percentDone")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let status = obj.get("status").and_then(Value::as_i64).unwrap_or(-1);

    Ok(TorrentInfo {
        id: id.to_string(),
        hash: obj
            .get("hashString")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        progress,
        state: map_status(status, progress),
        size_bytes: obj.get("totalSize").and_then(Value::as_i64).unwrap_or(0),
        downloaded_bytes: obj
            .get("downloadedEver")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        content_path: obj
            .get("downloadDir")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[async_trait]
impl Downloader for TransmissionClient {
    fn name(&self) -> &str {
        "transmission"
    }

    /// `torrent-add` accepts both magnet URIs and http(s) `.torrent` URLs via
    /// its `filename` argument, so both are supported here.
    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError> {
        let arguments = self
            .request("torrent-add", json!({ "filename": url }))
            .await?;
        // New adds land under torrent-added; duplicates under torrent-duplicate.
        for key in ["torrent-added", "torrent-duplicate"] {
            if let Some(id) = arguments
                .get(key)
                .and_then(|t| t.get("id"))
                .and_then(Value::as_i64)
            {
                return Ok(Some(id.to_string()));
            }
        }
        Ok(None)
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        let numeric = parse_torrent_id(id)?;
        let all = self.torrent_get(json!({ "ids": [numeric] })).await?;
        Ok(all.into_iter().find(|t| t.id == id))
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        self.torrent_get(json!({})).await
    }

    async fn resume(&self, id: &str) -> Result<(), AdapterError> {
        let numeric = parse_torrent_id(id)?;
        self.request("torrent-start", json!({ "ids": [numeric] }))
            .await
            .map(|_| ())
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        let numeric = parse_torrent_id(id)?;
        self.request("torrent-stop", json!({ "ids": [numeric] }))
            .await
            .map(|_| ())
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        let numeric = parse_torrent_id(id)?;
        self.request(
            "torrent-remove",
            json!({ "ids": [numeric], "delete-local-data": delete_files }),
        )
        .await
        .map(|_| ())
    }

    /// Wizard connectivity check: run the session handshake and fetch
    /// session-get.
    async fn test(&self) -> Result<(), AdapterError> {
        self.request("session-get", json!({})).await.map(|_| ())
    }
}

fn parse_torrent_id(id: &str) -> Result<i64, AdapterError> {
    id.parse::<i64>()
        .map_err(|_| AdapterError::Parse(format!("transmission torrent id is not numeric: {id}")))
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
        .unwrap_or(if target.url.scheme() == "https" {
            443
        } else {
            80
        });
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
    use base64::Engine;
    use wiremock::matchers::{body_string_contains, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const USER: &str = "admin";
    const PASS: &str = "hunter2";
    const TOKEN: &str = "sess-token-1";

    fn cfg(url: &str) -> serde_json::Value {
        json!({"url": url, "username": USER, "password": PASS})
    }

    fn basic_auth_header() -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}"))
        )
    }

    fn tr_body(arguments: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"result": "success", "arguments": arguments}))
    }

    fn conflict() -> ResponseTemplate {
        ResponseTemplate::new(409).append_header(SESSION_HEADER, TOKEN)
    }

    #[tokio::test]
    async fn handshake_add_and_get_flow() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"method\":\"torrent-add\""))
            .respond_with(tr_body(json!({
                "torrent-added": {"id": 5, "name": "Debian ISO"}
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"method\":\"torrent-get\""))
            .and(header("authorization", basic_auth_header()))
            .respond_with(tr_body(json!({
                "torrents": [
                    {"id": 5, "hashString": "abc123", "name": "Debian ISO",
                     "percentDone": 0.5, "status": 4, "totalSize": 1000,
                     "downloadedEver": 500, "downloadDir": "/data/incoming"},
                    {"id": 6, "hashString": "def456", "name": "Done ISO",
                     "percentDone": 1.0, "status": 0, "totalSize": 800,
                     "downloadedEver": 800, "downloadDir": "/data/library"},
                    {"id": 7, "hashString": "ghi789", "name": "Seeding ISO",
                     "percentDone": 1.0, "status": 6, "totalSize": 700,
                     "downloadedEver": 700, "downloadDir": "/data/library"},
                    {"id": 8, "hashString": "jkl012", "name": "Verifying ISO",
                     "percentDone": 0.1, "status": 2, "totalSize": 600,
                     "downloadedEver": 60, "downloadDir": "/data/incoming"}
                ]
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"method\":\"session-get\""))
            .respond_with(tr_body(json!({"rpc-version": 15})))
            .mount(&server)
            .await;

        // Fallback, mounted last: any POST without (or with a stale) session
        // token gets a 409 handshake challenge. wiremock answers with the
        // FIRST matching mock in registration order, so token-carrying
        // requests hit the per-method mocks above.
        Mock::given(method("POST"))
            .respond_with(conflict())
            .mount(&server)
            .await;

        let client = TransmissionClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .expect("construction validates url");

        // First call performs the 409 handshake and retries transparently.
        let added = client
            .add_url("magnet:?xt=urn:btih:abc123")
            .await
            .expect("add after handshake");
        assert_eq!(added.as_deref(), Some("5"));

        let all = client.get_all().await.expect("list torrents");
        assert_eq!(all.len(), 4);
        let by_id = |id: &str| all.iter().find(|t| t.id == id).unwrap();
        assert_eq!(by_id("5").state, "downloading");
        assert_eq!(by_id("5").progress, 0.5);
        assert_eq!(by_id("5").hash.as_deref(), Some("abc123"));
        assert_eq!(by_id("5").content_path.as_deref(), Some("/data/incoming"));
        // Stopped + fully downloaded maps to stoppedup...
        assert_eq!(by_id("6").state, "stoppedup");
        // ...stopped but incomplete maps to stoppeddl.
        assert_eq!(by_id("7").state, "seeding");
        assert_eq!(by_id("8").state, "checking");

        // get_info narrows server-side by ids.
        let info = client.get_info("6").await.expect("get_info").unwrap();
        assert_eq!(info.name, "Done ISO");

        client.test().await.expect("session-get succeeds");
    }

    #[tokio::test]
    async fn wrong_credentials_surface_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = TransmissionClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .unwrap();
        let err = client.test().await.expect_err("bad creds must yield Auth");
        assert!(matches!(err, AdapterError::Auth));
    }

    #[tokio::test]
    async fn pause_resume_delete_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"method\":\"torrent-stop\""))
            .respond_with(tr_body(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"method\":\"torrent-start\""))
            .respond_with(tr_body(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"delete-local-data\":true"))
            .respond_with(tr_body(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(conflict())
            .mount(&server)
            .await;

        let client = TransmissionClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .unwrap();
        client.pause("9").await.unwrap();
        client.resume("9").await.unwrap();
        client.delete("9", true).await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_add_returns_existing_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header(SESSION_HEADER, TOKEN))
            .and(body_string_contains("\"method\":\"torrent-add\""))
            .respond_with(tr_body(json!({
                "torrent-duplicate": {"id": 5, "name": "Debian ISO"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(conflict())
            .mount(&server)
            .await;

        let client = TransmissionClient::new(&cfg(&server.uri()), OutboundPolicy::dev())
            .await
            .unwrap();
        let added = client
            .add_url("https://example.org/debian.torrent")
            .await
            .unwrap();
        assert_eq!(added.as_deref(), Some("5"));
    }

    #[test]
    fn status_mapping_table() {
        assert_eq!(map_status(0, 0.5), "stoppeddl");
        assert_eq!(map_status(0, 1.0), "stoppedup");
        assert_eq!(map_status(1, 0.0), "checking");
        assert_eq!(map_status(2, 0.0), "checking");
        assert_eq!(map_status(3, 0.0), "downloading");
        assert_eq!(map_status(4, 0.0), "downloading");
        assert_eq!(map_status(5, 1.0), "seeding");
        assert_eq!(map_status(6, 1.0), "seeding");
    }
}
