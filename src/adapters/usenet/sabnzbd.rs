//! SABnzbd download client: GET `{base}/api?output=json&...`.
//!
//! Authentication accepts either an API key (`apikey` query parameter) or
//! HTTP basic auth (username/password). Every request goes through a client
//! whose DNS resolver was pinned at construction time to the addresses
//! [`crate::security::ssrf::validate_url`] approved, so no request can ever
//! dial an unvalidated address.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::adapters::{AdapterError, Downloader, TorrentInfo, UsenetDownloader};
use crate::security::ssrf::{validate_url, OutboundPolicy, ValidatedTarget};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MIB: f64 = 1024.0 * 1024.0;
const DEFAULT_CATEGORY: &str = "rivet";

#[derive(Debug, Clone, Deserialize)]
struct SabNzbdConfig {
    /// Base URL, e.g. `http://sab.local:8080`.
    url: String,
    #[serde(default, alias = "apiKey", alias = "api_key", alias = "apikey")]
    api_key: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// SABnzbd client. Implements [`UsenetDownloader`] and — as a thin alias over
/// the same operations — [`Downloader`], so pipeline code can treat it
/// uniformly with torrent clients.
pub struct SabNzbdClient {
    base_url: String,
    api_key: Option<String>,
    username: Option<String>,
    password: Option<String>,
    http: reqwest::Client,
}

impl SabNzbdClient {
    /// Adapter-registry constructor: parses the decrypted client config and
    /// returns the client behind the usenet trait object.
    pub async fn new(
        cfg_json: &serde_json::Value,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn UsenetDownloader>, AdapterError> {
        Ok(Arc::new(Self::connect(cfg_json, policy).await?) as Arc<dyn UsenetDownloader>)
    }

    /// Typed constructor: validates `cfg.url` against `policy` (resolving and
    /// checking EVERY returned address) and pins the approved IPs into a
    /// dedicated client. Redirects are disabled; no DNS lookups afterwards.
    pub async fn connect(
        cfg_json: &serde_json::Value,
        policy: OutboundPolicy,
    ) -> Result<Self, AdapterError> {
        let cfg: SabNzbdConfig = serde_json::from_value(cfg_json.clone())
            .map_err(|e| AdapterError::InvalidConfig(e.to_string()))?;
        if cfg.url.trim().is_empty() {
            return Err(AdapterError::InvalidConfig(
                "sabnzbd: url is required".into(),
            ));
        }
        let has_api_key = cfg.api_key.as_deref().is_some_and(|k| !k.is_empty());
        let has_basic = cfg.username.as_deref().is_some_and(|u| !u.is_empty())
            && cfg.password.as_deref().is_some_and(|p| !p.is_empty());
        if !has_api_key && !has_basic {
            return Err(AdapterError::InvalidConfig(
                "sabnzbd: apiKey or username+password required".into(),
            ));
        }
        let base_url = cfg.url.trim_end_matches('/').to_string();
        let target = validate_url(&base_url, policy)
            .await
            .map_err(|e| AdapterError::InvalidConfig(format!("sabnzbd url rejected: {e}")))?;
        Ok(Self {
            base_url,
            api_key: cfg.api_key.filter(|k| !k.is_empty()),
            username: cfg.username.filter(|u| !u.is_empty()),
            password: cfg.password.filter(|p| !p.is_empty()),
            http: pinned_client(&target),
        })
    }

    /// One `/api` call. Returns the decoded JSON envelope.
    async fn request(&self, mode: &str, extra: &[(&str, &str)]) -> Result<Value, AdapterError> {
        let url = format!("{}/api", self.base_url);
        let mut params: Vec<(&str, &str)> = vec![("output", "json"), ("mode", mode)];
        if let Some(key) = &self.api_key {
            params.push(("apikey", key));
        }
        params.extend_from_slice(extra);
        let mut req = self.http.get(&url).query(&params);
        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            req = req.basic_auth(user, Some(pass));
        }
        let resp = req.send().await?;
        check_status(resp.status())?;
        let body: Value = serde_json::from_str(&resp.text().await?)
            .map_err(|e| AdapterError::Parse(format!("sabnzbd returned invalid json: {e}")))?;
        if let Some(err) = body.get("error").and_then(Value::as_str) {
            let lowered = err.to_lowercase();
            if lowered.contains("api key") || lowered.contains("apikey") {
                return Err(AdapterError::Auth);
            }
            return Err(AdapterError::Network(format!("sabnzbd error: {err}")));
        }
        Ok(body)
    }

    /// `mode=addurl`: enqueue by URL, returns the assigned nzo id.
    async fn add(&self, nzb_url: &str, category: Option<&str>) -> Result<String, AdapterError> {
        let body = self
            .request(
                "addurl",
                &[
                    ("name", nzb_url),
                    ("cat", category.unwrap_or(DEFAULT_CATEGORY)),
                ],
            )
            .await?;
        if body.get("status").and_then(Value::as_bool) != Some(true) {
            return Err(AdapterError::Network("sabnzbd rejected addurl".into()));
        }
        let id = body
            .get("nzo_ids")
            .and_then(Value::as_array)
            .and_then(|ids| ids.first())
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AdapterError::Parse("sabnzbd addurl: no nzo id returned".into()))?;
        Ok(id.to_string())
    }

    async fn all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        let (queue, history) =
            tokio::try_join!(self.request("queue", &[]), self.request("history", &[]))?;
        let mut infos = Vec::new();
        for slot in slots(&queue, "queue") {
            if let Some(info) = queue_slot_to_info(slot) {
                infos.push(info);
            }
        }
        for slot in slots(&history, "history") {
            if let Some(info) = history_slot_to_info(slot) {
                infos.push(info);
            }
        }
        Ok(infos)
    }

    async fn info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        Ok(self.all().await?.into_iter().find(|info| info.id == id))
    }

    /// `mode=pause` / `mode=resume` scoped to one nzo id.
    async fn set_paused(&self, id: &str, paused: bool) -> Result<(), AdapterError> {
        let mode = if paused { "pause" } else { "resume" };
        let body = self.request(mode, &[("nzo_ids", id)]).await?;
        if body.get("status").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(AdapterError::Network(format!("sabnzbd {mode} failed")))
        }
    }

    /// `mode=queue&name=delete`; `del_files=1` also removes downloaded files.
    async fn remove(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        let body = self
            .request(
                "queue",
                &[
                    ("name", "delete"),
                    ("nzo_ids", id),
                    ("del_files", if delete_files { "1" } else { "0" }),
                ],
            )
            .await?;
        if body.get("status").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(AdapterError::Network("sabnzbd delete failed".into()))
        }
    }

    async fn ping(&self) -> Result<(), AdapterError> {
        let body = self.request("version", &[]).await?;
        if body
            .get("version")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        {
            Ok(())
        } else {
            Err(AdapterError::Parse(
                "sabnzbd version: expected string".into(),
            ))
        }
    }
}

#[async_trait]
impl UsenetDownloader for SabNzbdClient {
    fn name(&self) -> &str {
        "sabnzbd"
    }

    async fn add_nzb_url(&self, url: &str, category: Option<&str>) -> Result<String, AdapterError> {
        self.add(url, category).await
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        self.info(id).await
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        self.all().await
    }

    async fn resume(&self, id: &str) -> Result<(), AdapterError> {
        self.set_paused(id, false).await
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        self.set_paused(id, true).await
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        self.remove(id, delete_files).await
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.ping().await
    }
}

#[async_trait]
impl Downloader for SabNzbdClient {
    fn name(&self) -> &str {
        "sabnzbd"
    }

    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError> {
        self.add(url, None).await.map(Some)
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        self.info(id).await
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        self.all().await
    }

    async fn resume(&self, id: &str) -> Result<(), AdapterError> {
        self.set_paused(id, false).await
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        self.set_paused(id, true).await
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        self.remove(id, delete_files).await
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.ping().await
    }
}

/// Extracts the `slots` array from a queue/history envelope.
fn slots<'a>(envelope: &'a Value, section: &str) -> &'a [Value] {
    envelope
        .get(section)
        .and_then(|s| s.get("slots"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Numeric fields arrive as JSON numbers on modern SABnzbd and as strings on
/// older ones; accept both.
fn num_field(obj: &Value, key: &str) -> f64 {
    obj.get(key)
        .and_then(Value::as_f64)
        .or_else(|| {
            obj.get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0.0)
}

fn str_field<'a>(obj: &'a Value, key: &str) -> &'a str {
    obj.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// State mapping: Paused → pauseddl, Completed → completed, anything else
/// passes through lowercased (downloading, queued, failed, ...).
fn state_for(status: &str) -> String {
    match status.to_lowercase().as_str() {
        "paused" => "pauseddl".into(),
        "completed" => "completed".into(),
        other => other.to_string(),
    }
}

fn progress_for(mb: f64, mb_left: f64, state: &str) -> f64 {
    if state == "completed" {
        return 1.0;
    }
    if mb > 0.0 {
        ((mb - mb_left) / mb).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn queue_slot_to_info(slot: &Value) -> Option<TorrentInfo> {
    let id = str_field(slot, "nzo_id");
    if id.is_empty() {
        return None;
    }
    let name = {
        let n = str_field(slot, "filename");
        if n.is_empty() {
            str_field(slot, "name")
        } else {
            n
        }
    };
    let status = str_field(slot, "status");
    let mb = num_field(slot, "mb");
    let mb_left = num_field(slot, "mbleft");
    let state = state_for(status);
    let size_bytes = (mb * MIB) as i64;
    let downloaded_bytes = (((mb - mb_left).max(0.0)) * MIB) as i64;
    Some(TorrentInfo {
        id: id.to_string(),
        hash: Some(id.to_string()),
        name: name.to_string(),
        progress: progress_for(mb, mb_left, &state),
        state,
        size_bytes,
        downloaded_bytes,
        content_path: None,
    })
}

fn history_slot_to_info(slot: &Value) -> Option<TorrentInfo> {
    let id = str_field(slot, "nzo_id");
    if id.is_empty() {
        return None;
    }
    let name = str_field(slot, "name");
    let status = str_field(slot, "status");
    let mb = num_field(slot, "mb");
    let mb_left = num_field(slot, "mbleft");
    let state = state_for(status);
    let storage = str_field(slot, "storage");
    Some(TorrentInfo {
        id: id.to_string(),
        hash: Some(id.to_string()),
        name: name.to_string(),
        progress: progress_for(mb, mb_left, &state),
        state,
        size_bytes: (mb * MIB) as i64,
        downloaded_bytes: (((mb - mb_left).max(0.0)) * MIB) as i64,
        content_path: if storage.is_empty() {
            None
        } else {
            Some(storage.to_string())
        },
    })
}

fn check_status(status: StatusCode) -> Result<(), AdapterError> {
    if status.is_success() {
        return Ok(());
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(AdapterError::Auth);
    }
    Err(AdapterError::Network(format!(
        "sabnzbd: unexpected http status {status}"
    )))
}

/// Mirrors `crate::security::ssrf::build_pinned_client` (which is private
/// there): a client whose resolver yields ONLY the IPs that passed policy
/// validation at construction, with redirects disabled.
fn pinned_client(target: &ValidatedTarget) -> reqwest::Client {
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
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(resolver)
        .build()
        .expect("static client configuration")
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(uri: &str) -> SabNzbdClient {
        SabNzbdClient::connect(
            &json!({"url": uri, "apiKey": "secret"}),
            OutboundPolicy::dev(),
        )
        .await
        .expect("client connects")
    }

    /// Decoded query pairs of every request the mock server received.
    async fn received_queries(server: &MockServer) -> Vec<Vec<(String, String)>> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|req| {
                req.url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .collect()
    }

    fn has(pairs: &[(String, String)], key: &str, value: &str) -> bool {
        pairs.iter().any(|(k, v)| k == key && v == value)
    }

    #[tokio::test]
    async fn version_check_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .and(query_param("mode", "version"))
            .and(query_param("output", "json"))
            .and(query_param("apikey", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version": "4.3.2"})))
            .expect(1u64)
            .mount(&server)
            .await;
        crate::adapters::UsenetDownloader::test(&client(&server.uri()).await)
            .await
            .expect("version ok");
        server.verify().await;
    }

    #[tokio::test]
    async fn bad_api_key_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"error": "API Key incorrect"})),
            )
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        assert!(matches!(
            crate::adapters::UsenetDownloader::test(&client).await,
            Err(AdapterError::Auth)
        ));
    }

    #[tokio::test]
    async fn http_unauthorized_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = SabNzbdClient::connect(
            &json!({"url": server.uri(), "username": "u", "password": "p"}),
            OutboundPolicy::dev(),
        )
        .await
        .expect("client connects");
        assert!(matches!(
            crate::adapters::UsenetDownloader::test(&client).await,
            Err(AdapterError::Auth)
        ));
    }

    #[tokio::test]
    async fn missing_credentials_rejected_at_construction() {
        let err = SabNzbdClient::connect(
            &json!({"url": "http://sab.example:8080"}),
            OutboundPolicy::dev(),
        )
        .await
        .err()
        .expect("config rejected");
        assert!(matches!(err, AdapterError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn add_sends_addurl_with_cat_and_apikey() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("mode", "addurl"))
            .and(query_param("name", "http://idx.example/dl/Big.Show.nzb"))
            .and(query_param("cat", "tv"))
            .and(query_param("apikey", "secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"status": true, "nzo_ids": ["SABNZO1"]})),
            )
            .expect(1u64)
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let id = client
            .add("http://idx.example/dl/Big.Show.nzb", Some("tv"))
            .await
            .expect("addurl ok");
        assert_eq!(id, "SABNZO1");
        server.verify().await;
    }

    #[tokio::test]
    async fn add_defaults_category_and_fails_without_nzo_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("cat", DEFAULT_CATEGORY))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"status": true, "nzo_ids": []})),
            )
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let err = client
            .add("http://idx.example/a.nzb", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Parse(_)));
    }

    #[tokio::test]
    async fn add_rejects_false_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": false})))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let err = client
            .add("http://idx.example/a.nzb", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));
    }

    #[tokio::test]
    async fn get_all_merges_queue_and_history_with_state_mapping() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("mode", "queue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queue": {"status": "Downloading", "slots": [
                    {
                        "nzo_id": "Q1", "filename": "Paused.Show", "status": "Paused",
                        "mb": 100.0, "mbleft": 100.0
                    },
                    {
                        "nzo_id": "Q2", "filename": "Active.Show", "status": "Downloading",
                        "mb": 200.0, "mbleft": 50.0
                    }
                ]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("mode", "history"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "history": {"slots": [
                    {
                        "nzo_id": "H1", "name": "Done.Show", "status": "Completed",
                        "mb": 300.0, "mbleft": 0.0, "storage": "/data/done/Done.Show"
                    },
                    {
                        "nzo_id": "H2", "name": "Broke.Show", "status": "Failed",
                        "mb": 40.0, "mbleft": 20.0
                    }
                ]}
            })))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let infos = client.all().await.expect("queue+history parse");
        assert_eq!(infos.len(), 4);

        let paused = infos.iter().find(|i| i.id == "Q1").unwrap();
        assert_eq!(paused.state, "pauseddl");
        assert_eq!(paused.progress, 0.0);
        assert!(crate::adapters::is_paused(paused));

        let active = infos.iter().find(|i| i.id == "Q2").unwrap();
        assert_eq!(active.state, "downloading");
        assert!((active.progress - 0.75).abs() < f64::EPSILON);
        assert_eq!(active.size_bytes, 200 * 1024 * 1024);

        let done = infos.iter().find(|i| i.id == "H1").unwrap();
        assert_eq!(done.state, "completed");
        assert_eq!(done.progress, 1.0);
        assert_eq!(done.content_path.as_deref(), Some("/data/done/Done.Show"));
        assert!(crate::adapters::is_completed(done));

        let failed = infos.iter().find(|i| i.id == "H2").unwrap();
        assert_eq!(failed.state, "failed");
        assert!((failed.progress - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn get_info_finds_queue_slot_by_nzo_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("mode", "queue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queue": {"slots": [
                    {"nzo_id": "Q9", "filename": "X.Show", "status": "Queued",
                     "mb": 10.0, "mbleft": 10.0}
                ]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("mode", "history"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"history": {"slots": []}})),
            )
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let hit = client.info("Q9").await.expect("lookup ok");
        assert_eq!(hit.map(|i| i.name), Some("X.Show".into()));
        let miss = client.info("Q0").await.expect("lookup ok");
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn pause_and_resume_scope_to_nzo_ids() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("mode", "pause"))
            .and(query_param("nzo_ids", "Q1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": true})))
            .expect(1u64)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("mode", "resume"))
            .and(query_param("nzo_ids", "Q1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": true})))
            .expect(1u64)
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        crate::adapters::UsenetDownloader::pause(&client, "Q1")
            .await
            .expect("pause ok");
        crate::adapters::UsenetDownloader::resume(&client, "Q1")
            .await
            .expect("resume ok");
        server.verify().await;

        let queries = received_queries(&server).await;
        assert!(queries.iter().all(|q| has(q, "apikey", "secret")));
    }

    #[tokio::test]
    async fn delete_sets_del_files_flag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("mode", "queue"))
            .and(query_param("name", "delete"))
            .and(query_param("nzo_ids", "Q2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": true})))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        client.remove("Q2", true).await.expect("delete ok");
        client.remove("Q2", false).await.expect("delete ok");

        let queries = received_queries(&server).await;
        assert!(has(&queries[0], "del_files", "1"));
        assert!(has(&queries[1], "del_files", "0"));
    }

    #[tokio::test]
    async fn delete_failure_surfaces_as_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": false})))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let err = client.remove("Q3", false).await.unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));
    }

    #[test]
    fn num_field_accepts_numbers_and_numeric_strings() {
        assert_eq!(num_field(&json!({"mb": 12.5}), "mb"), 12.5);
        assert_eq!(num_field(&json!({"mb": "12.5"}), "mb"), 12.5);
        assert_eq!(num_field(&json!({}), "mb"), 0.0);
    }

    #[test]
    fn state_mapping_covers_paused_completed_and_passthrough() {
        assert_eq!(state_for("Paused"), "pauseddl");
        assert_eq!(state_for("Completed"), "completed");
        assert_eq!(state_for("Downloading"), "downloading");
        assert_eq!(state_for("Propagating"), "propagating");
    }
}
