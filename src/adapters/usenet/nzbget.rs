//! NZBGet download client: JSON-RPC over HTTP(S) at `{base}/jsonrpc`.
//!
//! Wire format verified against NZBGet's RPC implementation
//! (`daemon/remote/XmlRpc.cpp`): JSON-RPC with **positional** parameters only,
//! HTTP basic authentication, numeric results, boolean `editqueue` results.
//!
//! Every request goes through a client whose DNS resolver was pinned at
//! construction time to the addresses [`crate::security::ssrf::validate_url`]
//! approved, so no request can ever dial an unvalidated address.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use percent_encoding::percent_decode_str;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::{AdapterError, Downloader, TorrentInfo, UsenetDownloader};
use crate::security::ssrf::{validate_url, OutboundPolicy, ValidatedTarget};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MIB: i64 = 1024 * 1024;
const DEFAULT_CATEGORY: &str = "rivet";

#[derive(Debug, Clone, Deserialize)]
struct NzbGetConfig {
    /// Base URL, e.g. `http://nzbget.local:6789`.
    url: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// NZBGet client. Implements [`UsenetDownloader`] and — as a thin alias over
/// the same operations — [`Downloader`], so pipeline code can treat it
/// uniformly with torrent clients.
pub struct NzbGetClient {
    base_url: String,
    username: String,
    password: String,
    http: reqwest::Client,
}

impl NzbGetClient {
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
        let cfg: NzbGetConfig = serde_json::from_value(cfg_json.clone())
            .map_err(|e| AdapterError::InvalidConfig(e.to_string()))?;
        if cfg.url.trim().is_empty() {
            return Err(AdapterError::InvalidConfig("nzbget: url is required".into()));
        }
        let base_url = cfg.url.trim_end_matches('/').to_string();
        let target = validate_url(&base_url, policy)
            .await
            .map_err(|e| AdapterError::InvalidConfig(format!("nzbget url rejected: {e}")))?;
        Ok(Self {
            base_url,
            username: cfg.username,
            password: cfg.password,
            http: pinned_client(&target),
        })
    }

    /// One JSON-RPC call. Returns the envelope's `result` value.
    async fn rpc(&self, method_name: &str, params: Value) -> Result<Value, AdapterError> {
        let url = format!("{}/jsonrpc", self.base_url);
        let payload = json!({ "method": method_name, "params": params, "id": 1 });
        let mut req = self.http.post(&url).json(&payload);
        if !self.username.is_empty() || !self.password.is_empty() {
            req = req.basic_auth(&self.username, Some(&self.password));
        }
        let resp = req.send().await?;
        check_status(resp.status())?;
        let body: Value = serde_json::from_str(&resp.text().await?)
            .map_err(|e| AdapterError::Parse(format!("nzbget returned invalid json: {e}")))?;
        if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
            return Err(AdapterError::Network(format!("nzbget rpc error: {err}")));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| AdapterError::Parse("nzbget rpc response missing result".into()))
    }

    /// `append` by URL (v13+ positional signature; NZBGet fetches `nzb_url`
    /// itself). Returns the assigned NZBID.
    async fn add(&self, nzb_url: &str, category: Option<&str>) -> Result<String, AdapterError> {
        let params = json!([
            nzb_filename_from_url(nzb_url),       // NZBFilename
            nzb_url,                              // NZBContent (URL triggers fetch)
            category.unwrap_or(DEFAULT_CATEGORY), // Category
            0,                                    // Priority
            false,                                // AddToTop
            false,                                // AddPaused
            "",                                   // DupeKey
            0,                                    // DupeScore
            "FORCE",                              // DupeMode: never silently drop
        ]);
        let result = self.rpc("append", params).await?;
        let id = result
            .as_i64()
            .ok_or_else(|| AdapterError::Parse("nzbget append: expected numeric NZBID".into()))?;
        if id <= 0 {
            return Err(AdapterError::Network(format!(
                "nzbget rejected append (NZBID {id})"
            )));
        }
        Ok(id.to_string())
    }

    async fn all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        let result = self.rpc("listgroups", json!([])).await?;
        let groups = result
            .as_array()
            .ok_or_else(|| AdapterError::Parse("nzbget listgroups: expected array".into()))?;
        Ok(groups.iter().filter_map(group_to_info).collect())
    }

    async fn info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        let normalized = parse_id(id)?.to_string();
        let groups = self.all().await?;
        Ok(groups.into_iter().find(|g| g.id == normalized))
    }

    async fn edit(&self, command: &str, nzb_id: i64) -> Result<(), AdapterError> {
        let result = self.rpc("editqueue", json!([command, "", [nzb_id]])).await?;
        if result.as_bool() == Some(true) {
            Ok(())
        } else {
            Err(AdapterError::Network(format!(
                "nzbget editqueue {command} failed"
            )))
        }
    }

    async fn remove(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        let nzb_id = parse_id(id)?;
        // Real API: GroupDelete removes the group from the queue; there is no
        // boolean DeleteFiles flag. GroupFinalDelete is the delete-with-files
        // action (files removed from disk, history entry skipped).
        let command = if delete_files {
            "GroupFinalDelete"
        } else {
            "GroupDelete"
        };
        self.edit(command, nzb_id).await
    }

    async fn ping(&self) -> Result<(), AdapterError> {
        let result = self.rpc("version", json!([])).await?;
        if result.as_str().is_some_and(|v| !v.is_empty()) {
            Ok(())
        } else {
            Err(AdapterError::Parse("nzbget version: expected string".into()))
        }
    }
}

#[async_trait]
impl UsenetDownloader for NzbGetClient {
    fn name(&self) -> &str {
        "nzbget"
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
        self.edit("GroupResume", parse_id(id)?).await
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        self.edit("GroupPause", parse_id(id)?).await
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        self.remove(id, delete_files).await
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.ping().await
    }
}

#[async_trait]
impl Downloader for NzbGetClient {
    fn name(&self) -> &str {
        "nzbget"
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
        self.edit("GroupResume", parse_id(id)?).await
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        self.edit("GroupPause", parse_id(id)?).await
    }

    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError> {
        self.remove(id, delete_files).await
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.ping().await
    }
}

fn parse_id(id: &str) -> Result<i64, AdapterError> {
    id.trim()
        .parse::<i64>()
        .map_err(|_| AdapterError::Parse(format!("nzbget: invalid NZBID {id:?}")))
}

/// Derives the `NZBFilename` argument from the download URL: the last path
/// segment, percent-decoded, forced to an `.nzb` extension. Indexer download
/// links often carry no `.nzb` segment (`/get?t=xyz`) — there the query string
/// identifies the download, so it becomes the candidate; with neither segment
/// nor query the generic `download.nzb` is used.
fn nzb_filename_from_url(url: &str) -> String {
    let parsed = url::Url::parse(url).ok();
    let segment = parsed.as_ref().and_then(|u| {
        u.path_segments()
            .and_then(|mut segs| segs.next_back())
            .filter(|s| !s.is_empty())
            .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
    });
    let candidate = match segment {
        Some(seg) if seg.to_lowercase().ends_with(".nzb") => seg,
        segment => {
            let query = parsed
                .as_ref()
                .and_then(|u| u.query())
                .filter(|q| !q.is_empty());
            match query {
                Some(q) => q.to_string(),
                None => segment.unwrap_or_else(|| "download".to_string()),
            }
        }
    };
    if candidate.to_lowercase().ends_with(".nzb") {
        candidate
    } else {
        format!("{candidate}.nzb")
    }
}

/// Reads a 64-bit size field serialized as `{prefix}Lo`/`{prefix}Hi` (the
/// NZBGet convention), falling back to the coarse `{prefix}MB` variant.
fn field_bytes(obj: &Value, prefix: &str) -> i64 {
    let lo = obj
        .get(format!("{prefix}Lo"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let hi = obj.get(format!("{prefix}Hi")).and_then(Value::as_i64);
    if let Some(hi) = hi.filter(|h| *h >= 0) {
        (hi << 32) | lo
    } else {
        obj.get(format!("{prefix}MB"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            * MIB
    }
}

/// Maps a `listgroups` entry onto the normalized snapshot. Completed means no
/// bytes remain; paused is derived from the group status; everything else is
/// treated as actively downloading.
fn group_to_info(group: &Value) -> Option<TorrentInfo> {
    let id = group.get("NZBID").and_then(Value::as_i64)?;
    let name = group
        .get("NZBName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let status = group
        .get("Status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let size = field_bytes(group, "FileSize");
    let remaining = field_bytes(group, "RemainingSize");
    let complete = remaining == 0;
    let downloaded = size.saturating_sub(remaining);
    let progress = if complete {
        1.0
    } else if size > 0 {
        (downloaded as f64 / size as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let state = if complete {
        "completed"
    } else if status.eq_ignore_ascii_case("PAUSED") {
        "pauseddl"
    } else {
        "downloading"
    };
    Some(TorrentInfo {
        id: id.to_string(),
        hash: Some(id.to_string()),
        name,
        progress,
        state: state.to_string(),
        size_bytes: size,
        downloaded_bytes: downloaded,
        content_path: group
            .get("DestDir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
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
        "nzbget: unexpected http status {status}"
    )))
}

/// Mirrors `crate::security::ssrf::build_pinned_client` (which is private
/// there): a client whose resolver yields ONLY the IPs that passed policy
/// validation at construction, with redirects disabled.
fn pinned_client(target: &ValidatedTarget) -> reqwest::Client {
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
                )) as Box<dyn std::error::Error + Send + Sync>)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(uri: &str) -> NzbGetClient {
        NzbGetClient::connect(
            &json!({"url": uri, "username": "user", "password": "pass"}),
            OutboundPolicy::dev(),
        )
        .await
        .expect("client connects")
    }

    /// Decoded bodies of every request the mock server received.
    async fn received_bodies(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|req| serde_json::from_slice(&req.body).expect("recorded body parses"))
            .collect()
    }

    #[tokio::test]
    async fn version_check_succeeds_on_string_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": "23.0"})))
            .expect(1u64)
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        crate::adapters::UsenetDownloader::test(&client).await.expect("version check passes");
        server.verify().await;

        let body = &received_bodies(&server).await[0];
        assert_eq!(body["method"], "version");
    }

    #[tokio::test]
    async fn auth_failure_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        assert!(matches!(crate::adapters::UsenetDownloader::test(&client).await, Err(AdapterError::Auth)));
    }

    #[tokio::test]
    async fn rpc_fault_surfaces_as_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"error": {"code": 2, "message": "Invalid parameter"}}),
                ),
            )
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let err = crate::adapters::UsenetDownloader::test(&client).await.unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));
    }

    #[tokio::test]
    async fn add_appends_positional_params_and_returns_nzbid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "append"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 42})))
            .expect(1u64)
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let id = client
            .add("http://idx.example/dl/Big.Show.nzb", Some("tv"))
            .await
            .expect("append succeeds");
        assert_eq!(id, "42");
        server.verify().await;

        let body = &received_bodies(&server).await[0];
        let params = body["params"].as_array().expect("params array");
        assert_eq!(params.len(), 9);
        assert_eq!(params[0], "Big.Show.nzb");
        assert_eq!(params[1], "http://idx.example/dl/Big.Show.nzb");
        assert_eq!(params[2], "tv");
        assert_eq!(params[5], false); // AddPaused must be false
    }

    #[tokio::test]
    async fn add_defaults_category_to_rivet_and_rejects_zero_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 0})))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let err = client.add("http://idx.example/a.nzb", None).await.unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));

        let body = &received_bodies(&server).await[0];
        assert_eq!(body["params"][2], DEFAULT_CATEGORY);
    }

    #[tokio::test]
    async fn listgroups_maps_paused_downloading_completed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "listgroups"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "result": [
                        {
                            "NZBID": 1, "NZBName": "Paused.Show", "Status": "PAUSED",
                            "FileSizeMB": 100, "RemainingSizeMB": 60, "DestDir": "/data/p"
                        },
                        {
                            "NZBID": 2, "NZBName": "Active.Show", "Status": "DOWNLOADING",
                            "FileSizeMB": 200, "RemainingSizeMB": 50
                        },
                        {
                            "NZBID": 7, "NZBName": "Done.Show", "Status": "PP_FINISHED",
                            "FileSizeMB": 300, "RemainingSizeMB": 0
                        }
                    ]
                })),
            )
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let infos = client.all().await.expect("listgroups parses");
        assert_eq!(infos.len(), 3);

        let paused = &infos[0];
        assert_eq!(paused.id, "1");
        assert_eq!(paused.state, "pauseddl");
        assert_eq!(paused.progress, 0.4);
        assert_eq!(paused.size_bytes, 100 * MIB);
        assert_eq!(paused.downloaded_bytes, 40 * MIB);
        assert_eq!(paused.content_path.as_deref(), Some("/data/p"));

        let active = &infos[1];
        assert_eq!(active.state, "downloading");
        assert!((active.progress - 0.75).abs() < f64::EPSILON);

        let done = &infos[2];
        assert_eq!(done.state, "completed");
        assert_eq!(done.progress, 1.0);
        assert!(crate::adapters::is_completed(done));
        assert!(crate::adapters::is_paused(paused));
    }

    #[tokio::test]
    async fn get_info_filters_by_nzbid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "result": [{"NZBID": 9, "NZBName": "X", "Status": "QUEUED",
                                "FileSizeMB": 10, "RemainingSizeMB": 10}]
                })),
            )
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let hit = client.info("9").await.expect("lookup ok");
        assert_eq!(hit.map(|i| i.name), Some("X".into()));
        let miss = client.info("8").await.expect("lookup ok");
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn resume_sends_group_resume_editqueue_command() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "editqueue"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": true})))
            .expect(1u64)
            .mount(&server)
            .await;
        crate::adapters::UsenetDownloader::resume(&client(&server.uri()).await, "5").await.expect("resume ok");
        server.verify().await;

        let body = &received_bodies(&server).await[0];
        let params = body["params"].as_array().expect("params array");
        assert_eq!(params[0], "GroupResume");
        assert_eq!(params[1], "");
        assert_eq!(params[2], json!([5]));
    }

    #[tokio::test]
    async fn pause_sends_group_pause_editqueue_command() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": true})))
            .expect(1u64)
            .mount(&server)
            .await;
        crate::adapters::UsenetDownloader::pause(&client(&server.uri()).await, "6").await.expect("pause ok");

        let body = &received_bodies(&server).await[0];
        assert_eq!(body["params"][0], "GroupPause");
        assert_eq!(body["params"][2], json!([6]));
    }

    #[tokio::test]
    async fn delete_with_files_uses_group_final_delete() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": true})))
            .expect(1u64)
            .mount(&server)
            .await;
        client(&server.uri())
            .await
            .remove("11", true)
            .await
            .expect("delete with files ok");

        let body = &received_bodies(&server).await[0];
        assert_eq!(body["params"][0], "GroupFinalDelete");
        assert_eq!(body["params"][2], json!([11]));
    }

    #[tokio::test]
    async fn delete_without_files_uses_group_delete_and_flags_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": false})))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let err = client.remove("12", false).await.unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));

        let body = &received_bodies(&server).await[0];
        assert_eq!(body["params"][0], "GroupDelete");
    }

    #[tokio::test]
    async fn downloader_trait_delegates_add_url_to_append() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 77})))
            .mount(&server)
            .await;
        let client = client(&server.uri()).await;
        let hash = Downloader::add_url(&client, "https://idx.example/file.nzb")
            .await
            .expect("add via Downloader trait");
        assert_eq!(hash.as_deref(), Some("77"));
    }

    #[test]
    fn filename_derivation_handles_segments_and_extension() {
        assert_eq!(
            nzb_filename_from_url("https://idx.example/get?t=xyz"),
            "t=xyz.nzb"
        );
        assert_eq!(
            nzb_filename_from_url("https://idx.example/dl/My%20Show.nzb"),
            "My Show.nzb"
        );
        assert_eq!(
            nzb_filename_from_url("https://idx.example/"),
            "download.nzb"
        );
    }

    #[test]
    fn field_bytes_prefers_lo_hi_over_mb() {
        let obj = json!({"FileSizeLo": 0, "FileSizeHi": 1, "FileSizeMB": 999});
        assert_eq!(field_bytes(&obj, "FileSize"), 1i64 << 32);
        let mb_only = json!({"FileSizeMB": 2});
        assert_eq!(field_bytes(&mb_only, "FileSize"), 2 * MIB);
        assert_eq!(field_bytes(&json!({}), "FileSize"), 0);
    }

    #[test]
    fn parse_id_rejects_garbage() {
        assert!(matches!(parse_id("abc"), Err(AdapterError::Parse(_))));
        assert_eq!(parse_id(" 17 ").unwrap(), 17);
    }
}
