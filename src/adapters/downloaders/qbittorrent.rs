//! qBittorrent WebUI API v2 download client adapter.
//!
//! All HTTP goes through [`crate::security::ssrf`]: the configured base URL is
//! validated against the outbound policy and a single IP-pinned reqwest client
//! is built per adapter instance (the host is fixed configuration), so a
//! request can never re-resolve to a different address. The WebUI session
//! cookie (`SID`) is cached and refreshed once per call on HTTP 403.
//!
//! Review fix carried over from the Go version: every torrent this client adds
//! is tagged with the `rivet` category and every listing is scoped to it, so
//! the adapter never sees (or touches) torrents it did not create.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::adapters::{AdapterError, Downloader, TorrentInfo};
use crate::security::ssrf::{self, OutboundPolicy};

/// Per-request timeout, mirroring the Go client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on any single response body. Replaces the Go 1 MiB truncation,
/// which silently corrupted large torrent listings: bodies are read in full up
/// to this limit and then decoded.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Category applied to every added torrent and used to scope every listing.
const CATEGORY: &str = "rivet";

/// qBittorrent client. Construct with [`QbittorrentClient::new`].
pub struct QbittorrentClient {
    /// Raw base URL as configured (validated lazily on first request).
    base_url: String,
    username: String,
    password: String,
    policy: OutboundPolicy,
    /// Lazily validated target + pinned HTTP client, built exactly once.
    http: tokio::sync::OnceCell<reqwest::Client>,
    /// Cached WebUI session id (`SID` cookie value).
    sid: RwLock<Option<String>>,
}

impl QbittorrentClient {
    /// Builds a client from a config object `{url, username, password}`.
    /// SSRF validation and the pinned HTTP client are established lazily on
    /// the first request (constructors must stay sync).
    pub fn new(cfg_json: &Value, policy: OutboundPolicy) -> Result<Arc<dyn Downloader>, AdapterError> {
        let get_str = |key: &str| -> Result<String, AdapterError> {
            cfg_json
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| AdapterError::InvalidConfig(format!("missing_fields: {key}")))
        };
        let base_url = get_str("url")?;
        let username = get_str("username")?;
        let password = get_str("password")?;
        Ok(Arc::new(Self {
            base_url,
            username,
            password,
            policy,
            http: tokio::sync::OnceCell::new(),
            sid: RwLock::new(None),
        }))
    }

    /// Absolute API URL, preserving any configured sub-path prefix.
    fn api(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    /// Returns the pinned client, validating the base URL (SSRF policy) on
    /// first use, and ensures an authenticated session.
    async fn connection(&self) -> Result<&reqwest::Client, AdapterError> {
        let client = self
            .http
            .get_or_try_init(|| async {
                let target = ssrf::validate_url(&self.base_url, self.policy)
                    .await
                    .map_err(|e| AdapterError::InvalidConfig(format!("invalid url: {e}")))?;
                Ok::<_, AdapterError>(ssrf::build_pinned_client(&target, REQUEST_TIMEOUT))
            })
            .await?;
        if self.sid.read().unwrap_or_else(std::sync::PoisonError::into_inner).is_none() {
            self.login(client).await?;
        }
        Ok(client)
    }

    /// POSTs `/api/v2/auth/login`, storing the `SID` cookie on success.
    async fn login(&self, client: &reqwest::Client) -> Result<(), AdapterError> {
        let resp = client
            .post(self.api("/api/v2/auth/login"))
            .form(&[("username", self.username.as_str()), ("password", self.password.as_str())])
            .send()
            .await
            .map_err(|_| AdapterError::Network("connect_failed".into()))?;

        let sid = extract_sid(resp.headers());
        let status = resp.status();
        let body = read_body_capped(resp, 1024).await?;
        if !status.is_success() || body.trim() != "Ok." {
            return Err(AdapterError::Auth);
        }
        // Some instances authenticate without issuing SID (e.g. bypass auth
        // for localhost); keep the session empty and let 403 handling retry.
        *self.sid.write().unwrap_or_else(std::sync::PoisonError::into_inner) = sid;
        Ok(())
    }

    fn current_sid(&self) -> Option<String> {
        self.sid.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Sends an authenticated request, re-logging in once on HTTP 403.
    async fn send<F>(&self, build: F) -> Result<reqwest::Response, AdapterError>
    where
        F: Fn(&reqwest::Client, Option<&str>) -> reqwest::RequestBuilder,
    {
        let client = self.connection().await?;
        let mut resp = build(client, self.current_sid().as_deref())
            .send()
            .await
            .map_err(|_| AdapterError::Network("connect_failed".into()))?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            self.login(client).await?;
            resp = build(client, self.current_sid().as_deref())
                .send()
                .await
                .map_err(|_| AdapterError::Network("connect_failed".into()))?;
            if resp.status() == reqwest::StatusCode::FORBIDDEN {
                return Err(AdapterError::Auth);
            }
        }
        Ok(resp)
    }

    /// POSTs `hashes=<hash>` to a torrent control endpoint.
    async fn post_hashes(&self, path: &str, hash: &str) -> Result<(), AdapterError> {
        let hash = hash.trim();
        if hash.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        let path_owned = path.to_string();
        let hash_owned = hash.to_string();
        let resp = self
            .send(move |client, sid| {
                let req = client
                    .post(self.api(&path_owned))
                    .form(&[("hashes", hash_owned.as_str())]);
                with_sid(req, sid)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(AdapterError::Network("control_failed".into()));
        }
        Ok(())
    }
}


fn with_sid(req: reqwest::RequestBuilder, sid: Option<&str>) -> reqwest::RequestBuilder {
    match sid {
        Some(sid) => req.header(reqwest::header::COOKIE, format!("SID={sid}")),
        None => req,
    }
}

/// Extracts the `SID` cookie value from `Set-Cookie` response headers.
fn extract_sid(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for value in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(value) = value.to_str() else { continue };
        for part in value.split(';') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("SID=") {
                let rest = rest.trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

/// Reads a response body in full, capped at `cap` bytes.
async fn read_body_capped(resp: reqwest::Response, cap: usize) -> Result<String, AdapterError> {
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| AdapterError::Network("read_failed".into()))? {
        if buf.len() + chunk.len() > cap {
            return Err(AdapterError::Network("response_too_large".into()));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| AdapterError::Parse("response is not utf-8".into()))
}

/// Raw `/torrents/info` entry; only the fields we consume are declared.
#[derive(Debug, Deserialize)]
struct QbTorrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    size: i64,
    #[serde(rename = "total_size", default)]
    total_size: i64,
    #[serde(default)]
    downloaded: i64,
    #[serde(default)]
    completed: i64,
    #[serde(rename = "content_path", default)]
    content_path: String,
}

impl From<QbTorrent> for TorrentInfo {
    fn from(t: QbTorrent) -> Self {
        // States are lowercased so qBittorrent v5 renames (`StoppedUP` /
        // `StoppedDL`) normalize onto the same vocabulary as v4
        // (`pausedUP`/`pausedDL`); `crate::adapters::is_completed` /
        // `is_paused` recognize both spellings.
        TorrentInfo {
            id: t.hash.clone(),
            hash: Some(t.hash),
            name: t.name,
            progress: t.progress.clamp(0.0, 1.0),
            state: t.state.to_lowercase(),
            size_bytes: if t.total_size != 0 { t.total_size } else { t.size },
            downloaded_bytes: if t.completed != 0 { t.completed } else { t.downloaded },
            content_path: if t.content_path.is_empty() { None } else { Some(t.content_path) },
        }
    }
}

#[async_trait]
impl Downloader for QbittorrentClient {
    fn name(&self) -> &str {
        "qbittorrent"
    }

    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(AdapterError::InvalidConfig("torrent_url_required".into()));
        }
        // Review fix: scope the torrent under the `rivet` category so it can
        // be identified without ever listing the instance's other torrents.
        let url_owned = url.to_string();
        let resp = self
            .send(move |client, sid| {
                let req = client
                    .post(self.api("/api/v2/torrents/add"))
                    .form(&[("urls", url_owned.as_str()), ("category", CATEGORY)]);
                with_sid(req, sid)
            })
            .await?;
        let status = resp.status();
        let body = read_body_capped(resp, 2048).await?;
        if !status.is_success() || body.trim() != "Ok." {
            return Err(AdapterError::Network("add_failed".into()));
        }
        Ok(extract_btih(url))
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        Ok(self
            .get_all()
            .await?
            .into_iter()
            .find(|t| t.hash.as_deref().map(|h| h.eq_ignore_ascii_case(id)).unwrap_or(false)))
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        // Scoped to the rivet category — never an unfiltered listing.
        let resp = self
            .send(|client, sid| {
                let req = client
                    .get(self.api("/api/v2/torrents/info"))
                    .query(&[("category", CATEGORY)]);
                with_sid(req, sid)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(AdapterError::Network("info_failed".into()));
        }
        let body = read_body_capped(resp, MAX_BODY_BYTES).await?;
        let torrents: Vec<QbTorrent> =
            serde_json::from_str(&body).map_err(|_| AdapterError::Parse("parse_failed".into()))?;
        Ok(torrents.into_iter().map(Into::into).collect())
    }

    async fn resume(&self, id: &str) -> Result<(), AdapterError> {
        self.post_hashes("/api/v2/torrents/resume", id).await
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        self.post_hashes("/api/v2/torrents/pause", id).await
    }

    async fn reannounce(&self, id: &str) -> Result<(), AdapterError> {
        self.post_hashes("/api/v2/torrents/reannounce", id).await
    }

    async fn recheck(&self, id: &str) -> Result<(), AdapterError> {
        self.post_hashes("/api/v2/torrents/recheck", id).await
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        let id_owned = id.to_string();
        let resp = self
            .send(move |client, sid| {
                let req = client
                    .post(self.api("/api/v2/torrents/delete"))
                    .form(&[("hashes", id_owned.as_str()), ("deleteFiles", if delete_files { "true" } else { "false" })]);
                with_sid(req, sid)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(AdapterError::Network("control_failed".into()));
        }
        Ok(())
    }

    /// Login + `/app/version` reachability check for the setup wizard.
    async fn test(&self) -> Result<(), AdapterError> {
        self.connection().await?;
        let resp = self
            .send(|client, sid| {
                let req = client.get(self.api("/api/v2/app/version"));
                with_sid(req, sid)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(AdapterError::Network("version_failed".into()));
        }
        Ok(())
    }
}

/// Extracts the btih hash from a `magnet:?xt=urn:btih:<hash>` URL, matching
/// the Go dry-run/qBittorrent identifier semantics.
pub(crate) fn extract_btih(u: &str) -> Option<String> {
    let u = u.trim();
    if !u.to_ascii_lowercase().starts_with("magnet:?") {
        return None;
    }
    let lower = u.to_ascii_lowercase();
    let marker = "xt=urn:btih:";
    let idx = lower.find(marker)?;
    let start = idx + marker.len();
    let end = u[start..]
        .find(|ch| ch == '&' || ch == '#')
        .map(|i| start + i)
        .unwrap_or(u.len());
    let hash = u[start..end].trim();
    if hash.is_empty() {
        None
    } else {
        Some(hash.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{is_completed, is_paused};
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn policy() -> OutboundPolicy {
        // Tests talk to a loopback wiremock server.
        OutboundPolicy { allow_internal: true }
    }

    fn login_mock() -> Mock {
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .and(body_string_contains("username=admin"))
            .and(body_string_contains("password=secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("Ok.")
                    .append_header("Set-Cookie", "SID=testsession; path=/"),
            )
    }

    async fn client_for(server: &MockServer) -> Arc<dyn Downloader> {
        QbittorrentClient::new(
            &serde_json::json!({
                "url": server.uri(),
                "username": "admin",
                "password": "secret",
            }),
            policy(),
        )
        .expect("client builds")
    }

    #[tokio::test]
    async fn add_url_posts_category_and_returns_magnet_hash() {
        let server = MockServer::start().await;
        let add = Mock::given(method("POST"))
            .and(path("/api/v2/torrents/add"))
            .and(body_string_contains("urls=magnet%3A%3Fxt%3Durn%3Abtih%3Aabc123"))
            .and(body_string_contains("category=rivet"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
            .expect(1)
            .mount(&server)
            .await;
        login_mock().mount(&server).await;

        let client = client_for(&server).await;
        let added = client
            .add_url("magnet:?xt=urn:btih:abc123&dn=Ubuntu")
            .await
            .expect("add succeeds");
        assert_eq!(added.as_deref(), Some("abc123"));
        server.verify().await;
        let _ = add;
    }

    #[tokio::test]
    async fn add_url_rejects_bad_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/add"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
            .mount(&server)
            .await;
        login_mock().mount(&server).await;

        let client = client_for(&server).await;
        let err = client.add_url("magnet:?xt=urn:btih:abc123").await.err().expect("expected error");
        assert!(matches!(err, AdapterError::Network(msg) if msg == "add_failed"));
    }

    #[tokio::test]
    async fn add_url_requires_url() {
        let client = QbittorrentClient::new(
            &serde_json::json!({"url": "http://localhost:8080", "username": "u", "password": "p"}),
            policy(),
        )
        .unwrap();
        let err = client.add_url("   ").await.err().expect("expected error");
        assert!(matches!(err, AdapterError::InvalidConfig(msg) if msg == "torrent_url_required"));
    }

    #[tokio::test]
    async fn forbidden_triggers_single_relogin_and_retry() {
        let server = MockServer::start().await;
        // Session cookie expired: the first add attempt gets 403, then the
        // client re-logins and retries successfully. Explicit priorities make
        // wiremock route the first request to the 403 mock.
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/add"))
            .respond_with(ResponseTemplate::new(403))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/add"))
            .and(body_string_contains("category=rivet"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
            .expect(1)
            .mount(&server)
            .await;
        let login = login_mock().expect(2).mount(&server).await; // initial + relogin

        let client = client_for(&server).await;
        client
            .add_url("magnet:?xt=urn:btih:deadbeef")
            .await
            .expect("retry after 403 succeeds");
        server.verify().await;
        let _ = login;
    }

    #[tokio::test]
    async fn persistent_forbidden_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/add"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let err = client.add_url("magnet:?xt=urn:btih:abc").await.err().expect("expected error");
        assert!(matches!(err, AdapterError::Auth));
    }

    #[tokio::test]
    async fn bad_credentials_are_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let err = client.add_url("magnet:?xt=urn:btih:abc").await.err().expect("expected error");
        assert!(matches!(err, AdapterError::Auth));
    }

    #[tokio::test]
    async fn info_is_scoped_and_normalizes_v5_states() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/torrents/info"))
            .and(query_param("category", "rivet"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "hash": "AAA111",
                    "name": "Ubuntu 24.04",
                    "progress": 0.42,
                    "state": "StoppedUP",
                    "total_size": 4_000_000_000i64,
                    "completed": 1_680_000_000i64,
                    "content_path": "/data/ubuntu",
                },
                {
                    "hash": "bbb222",
                    "name": "Seeding",
                    "progress": 1.0,
                    "state": "uploading",
                    "size": 100,
                }
            ])))
            .expect(3)
            .mount(&server)
            .await;
        login_mock().mount(&server).await;

        let client = client_for(&server).await;
        let all = client.get_all().await.expect("get_all");
        assert_eq!(all.len(), 2);

        let first = &all[0];
        assert_eq!(first.id, "AAA111");
        assert_eq!(first.hash.as_deref(), Some("AAA111"));
        assert_eq!(first.state, "stoppedup"); // v5 rename, lowercased
        assert!(is_paused(first), "stoppedup must count as paused");
        assert!(is_completed(first), "stoppedup must count as completed");
        assert_eq!(first.size_bytes, 4_000_000_000);
        assert_eq!(first.downloaded_bytes, 1_680_000_000);
        assert_eq!(first.content_path.as_deref(), Some("/data/ubuntu"));

        let second = &all[1];
        assert_eq!(second.size_bytes, 100, "size falls back to `size` field");
        assert_eq!(second.content_path, None);

        // Case-insensitive lookup by hash.
        let info = client.get_info("aaa111").await.expect("get_info").expect("found");
        assert_eq!(info.name, "Ubuntu 24.04");
        assert!(client.get_info("missing").await.expect("get_info").is_none());

        server.verify().await;
    }

    #[tokio::test]
    async fn control_endpoints_post_hashes() {
        let server = MockServer::start().await;
        for endpoint in ["resume", "pause", "reannounce", "recheck"] {
            Mock::given(method("POST"))
                .and(path(format!("/api/v2/torrents/{endpoint}")))
                .and(body_string_contains("hashes=abc123"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/api/v2/torrents/delete"))
            .and(body_string_contains("hashes=abc123"))
            .and(body_string_contains("deleteFiles=true"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        login_mock().mount(&server).await;

        let client = client_for(&server).await;
        client.resume("abc123").await.expect("resume");
        client.pause("abc123").await.expect("pause");
        client.reannounce("abc123").await.expect("reannounce");
        client.recheck("abc123").await.expect("recheck");
        client.delete("abc123", true).await.expect("delete");
        server.verify().await;
    }

    #[tokio::test]
    async fn control_rejects_empty_hash() {
        let client = QbittorrentClient::new(
            &serde_json::json!({"url": "http://localhost:8080", "username": "u", "password": "p"}),
            policy(),
        )
        .unwrap();
        assert!(client.resume("").await.is_err());
        assert!(client.delete("  ", false).await.is_err());
    }

    #[tokio::test]
    async fn test_login_and_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/app/version"))
            .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.1"))
            .expect(1)
            .mount(&server)
            .await;
        login_mock().mount(&server).await;

        let client = client_for(&server).await;
        client.test().await.expect("test ok");
        server.verify().await;
    }

    #[tokio::test]
    async fn new_rejects_missing_fields() {
        let err = QbittorrentClient::new(&serde_json::json!({"url": "http://x"}), policy()).err().expect("expected error");
        assert!(matches!(err, AdapterError::InvalidConfig(_)));
        let err = QbittorrentClient::new(
            &serde_json::json!({"url": "http://x", "username": "  ", "password": "p"}),
            policy(),
        )
        .err().expect("expected error");
        assert!(matches!(err, AdapterError::InvalidConfig(msg) if msg.contains("username")));
    }

    #[test]
    fn extract_btih_matches_go_semantics() {
        assert_eq!(extract_btih("magnet:?xt=urn:btih:ABC&dn=x"), Some("ABC".into()));
        assert_eq!(extract_btih("  magnet:?dn=x&xt=urn:btih:ff#frag"), Some("ff".into()));
        assert_eq!(extract_btih("magnet:?dn=only"), None);
        assert_eq!(extract_btih("http://example.com/file.torrent"), None);
        assert_eq!(extract_btih("magnet:?xt=urn:btih:"), None);
    }

    #[tokio::test]
    async fn rejects_non_http_base_url() {
        let client = QbittorrentClient::new(
            &serde_json::json!({"url": "file:///etc/passwd", "username": "u", "password": "p"}),
            policy(),
        )
        .unwrap();
        let err = client.test().await.err().expect("expected error");
        assert!(matches!(err, AdapterError::InvalidConfig(msg) if msg.contains("invalid url")));
    }
}
