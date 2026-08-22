//! Blackhole download clients: save `.torrent` / `.nzb` payloads into a
//! configured watch directory that an external download client monitors
//! (rtorrent/Deluge watch folders, SABnzbd's watched folder, ...).
//!
//! Progress semantics follow the watch-dir convention: there is no live
//! progress feed from the external client, so [`Downloader::get_info`]
//! reports 0% until the payload file exists in the watch directory and
//! completed afterwards. Pause/resume are no-ops; delete removes the payload
//! file. Magnet URIs are saved as `<btih>.magnet` files (the *arr blackhole
//! convention) named after their info hash; http(s) payloads are stored under
//! a fresh UUID.
//!
//! All remote fetches go through [`crate::security::ssrf`] (validated,
//! IP-pinned requests).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::adapters::{AdapterError, Downloader, TorrentInfo, UsenetDownloader};
use crate::security::ssrf::{self, OutboundPolicy};

/// Per-fetch timeout for remote payload downloads.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on any single fetched payload body.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Redirect hops allowed by [`ssrf::get_validated`]; every hop re-validated.
const MAX_REDIRECTS: usize = 5;

/// Payload file extensions this client owns in the watch directory.
const PAYLOAD_EXTENSIONS: [&str; 3] = [".torrent", ".magnet", ".nzb"];

/// Blackhole client over one watch directory. Construct with
/// [`BlackholeClient::new`] (torrent) or [`BlackholeClient::new_usenet`].
pub struct BlackholeClient {
    watch_dir: PathBuf,
    policy: OutboundPolicy,
}

impl BlackholeClient {
    /// Builds the torrent-side (`dyn Downloader`) client from a config object
    /// `{watchDir}`. SSRF policy applies to every payload fetch.
    pub fn new(
        cfg_json: &Value,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn Downloader>, AdapterError> {
        Ok(Self::build(cfg_json, policy)?)
    }

    /// Builds the usenet-side (`dyn UsenetDownloader`) client from a config
    /// object `{watchDir}`.
    pub fn new_usenet(
        cfg_json: &Value,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn UsenetDownloader>, AdapterError> {
        Ok(Self::build(cfg_json, policy)?)
    }

    fn build(cfg_json: &Value, policy: OutboundPolicy) -> Result<Arc<Self>, AdapterError> {
        let watch_dir = cfg_json
            .get("watchDir")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AdapterError::InvalidConfig("missing_fields: watchDir".into()))?;
        Ok(Arc::new(Self {
            watch_dir: PathBuf::from(watch_dir),
            policy,
        }))
    }

    /// Ensures the watch directory exists. Created on demand so a configured
    /// path on a not-yet-mounted volume heals itself instead of failing every
    /// add forever.
    fn ensure_dir(&self) -> Result<(), AdapterError> {
        std::fs::create_dir_all(&self.watch_dir)
            .map_err(|e| AdapterError::InvalidConfig(format!("watch_dir unavailable: {e}")))
    }

    /// Fetches a payload over http(s) through the SSRF-validated pipeline.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, AdapterError> {
        let resp = ssrf::get_validated(url, self.policy, FETCH_TIMEOUT, MAX_REDIRECTS)
            .await
            .map_err(|_| AdapterError::Network("fetch_failed".into()))?;
        if !resp.status().is_success() {
            return Err(AdapterError::Network(format!(
                "http_{}",
                resp.status().as_u16()
            )));
        }
        if resp.content_length().unwrap_or(0) > MAX_BODY_BYTES as u64 {
            return Err(AdapterError::Parse("payload_too_large".into()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|_| AdapterError::Network("read_failed".into()))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(AdapterError::Parse("payload_too_large".into()));
        }
        Ok(bytes.to_vec())
    }

    /// Writes via temp-file + rename so a watching external client never
    /// observes a partially written payload.
    fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AdapterError> {
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, bytes)
            .map_err(|e| AdapterError::Network(format!("watch_dir write failed: {e}")))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| AdapterError::Network(format!("watch_dir rename failed: {e}")))
    }

    /// Extracts the lowercased btih info hash from a magnet URI, if any.
    fn magnet_hash(url: &str) -> Option<String> {
        let query = url.strip_prefix("magnet:")?.split_once('?')?.1;
        for kv in query.split('&') {
            if let Some(hash) = kv.strip_prefix("xt=urn:btih:") {
                let hash = hash.trim();
                if !hash.is_empty() {
                    return Some(hash.to_ascii_lowercase());
                }
            }
        }
        None
    }

    /// Watch-dir progress semantics: 0% until the payload file exists, then
    /// completed (the external client consumes files directly from disk, so
    /// presence is the only observable signal). Always resolves to an entry
    /// for the given id; unknown ids simply read as still pending at 0%.
    fn info_for(&self, id: &str) -> TorrentInfo {
        for ext in PAYLOAD_EXTENSIONS {
            let path = self.watch_dir.join(format!("{id}{ext}"));
            if let Ok(md) = std::fs::metadata(&path) {
                return TorrentInfo {
                    id: id.to_string(),
                    // Magnets are identified by their btih hash.
                    hash: (ext == ".magnet").then(|| id.to_string()),
                    name: format!("{id}{ext}"),
                    progress: 1.0,
                    state: "completed".into(),
                    size_bytes: md.len() as i64,
                    downloaded_bytes: md.len() as i64,
                    content_path: Some(path.to_string_lossy().into_owned()),
                };
            }
        }
        TorrentInfo {
            id: id.to_string(),
            hash: None,
            name: id.to_string(),
            progress: 0.0,
            state: "downloading".into(),
            size_bytes: 0,
            downloaded_bytes: 0,
            content_path: None,
        }
    }

    /// Removes every payload file variant for `id`; missing files are fine.
    fn remove_payloads(&self, id: &str) -> Result<(), AdapterError> {
        for ext in PAYLOAD_EXTENSIONS {
            let _ = std::fs::remove_file(self.watch_dir.join(format!("{id}{ext}")));
        }
        let _ = std::fs::remove_file(self.watch_dir.join(format!("{id}.part")));
        Ok(())
    }

    /// Shared add logic: magnets are saved as `<btih>.magnet`, http(s)
    /// payloads are fetched and saved as `<uuid>.torrent`. Returns the id.
    async fn add_torrent_payload(&self, url: &str) -> Result<String, AdapterError> {
        self.ensure_dir()?;
        if let Some(hash) = Self::magnet_hash(url) {
            Self::write_atomic(
                &self.watch_dir.join(format!("{hash}.magnet")),
                url.as_bytes(),
            )?;
            return Ok(hash);
        }
        let id = Uuid::new_v4().to_string();
        let bytes = self.fetch_bytes(url).await?;
        Self::write_atomic(&self.watch_dir.join(format!("{id}.torrent")), &bytes)?;
        Ok(id)
    }

    /// Fetches an NZB payload and saves it as `<uuid>.nzb`; returns the id.
    async fn add_nzb_payload(&self, url: &str) -> Result<String, AdapterError> {
        self.ensure_dir()?;
        let id = Uuid::new_v4().to_string();
        let bytes = self.fetch_bytes(url).await?;
        Self::write_atomic(&self.watch_dir.join(format!("{id}.nzb")), &bytes)?;
        Ok(id)
    }

    /// Lists every owned payload currently sitting in the watch directory.
    fn list_payload_ids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.watch_dir) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = PAYLOAD_EXTENSIONS
                .iter()
                .find_map(|ext| name.strip_suffix(ext))
            {
                if !ids.iter().any(|seen| seen == id) {
                    ids.push(id.to_string());
                }
            }
        }
        ids.sort();
        ids
    }
}

#[async_trait]
impl Downloader for BlackholeClient {
    fn name(&self) -> &str {
        "blackhole"
    }

    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError> {
        self.add_torrent_payload(url).await.map(Some)
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        Ok(Some(self.info_for(id)))
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        self.ensure_dir()?;
        Ok(self
            .list_payload_ids()
            .into_iter()
            .map(|id| self.info_for(&id))
            .collect())
    }

    /// Watch directories have no pause semantics; the external client starts
    /// consuming as soon as it polls. A no-op keeps the monitor contract.
    async fn resume(&self, _id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn pause(&self, _id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn delete(&self, id: &str, _delete_files: bool) -> Result<(), AdapterError> {
        self.remove_payloads(id)
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.ensure_dir()?;
        if !self.watch_dir.is_dir() {
            return Err(AdapterError::InvalidConfig(
                "watchDir is not a directory".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl UsenetDownloader for BlackholeClient {
    fn name(&self) -> &str {
        "blackhole"
    }

    async fn add_nzb_url(
        &self,
        url: &str,
        _category: Option<&str>,
    ) -> Result<String, AdapterError> {
        self.add_nzb_payload(url).await
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        Ok(Some(self.info_for(id)))
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        self.ensure_dir()?;
        Ok(self
            .list_payload_ids()
            .into_iter()
            .map(|id| self.info_for(&id))
            .collect())
    }

    async fn resume(&self, _id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn pause(&self, _id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn delete(&self, id: &str, _delete_files: bool) -> Result<(), AdapterError> {
        self.remove_payloads(id)
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.ensure_dir()?;
        if !self.watch_dir.is_dir() {
            return Err(AdapterError::InvalidConfig(
                "watchDir is not a directory".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::adapters::is_completed;

    fn cfg(dir: &Path) -> serde_json::Value {
        serde_json::json!({ "watchDir": dir.join("watch").display().to_string() })
    }

    fn policy() -> OutboundPolicy {
        // Tests talk to a loopback wiremock server.
        OutboundPolicy {
            allow_internal: true,
        }
    }

    #[tokio::test]
    async fn fetches_torrent_url_into_watch_dir_and_completes() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/f/ubuntu.torrent"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"d4:infod1:e0ee"))
            .mount(&server)
            .await;

        let client = BlackholeClient::new(&cfg(dir.path()), policy()).unwrap();
        let id = client
            .add_url(&format!("{}/f/ubuntu.torrent", server.uri()))
            .await
            .unwrap()
            .expect("blackhole always knows its id");

        let saved = dir.path().join("watch").join(format!("{id}.torrent"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"d4:infod1:e0ee");

        let info = client.get_info(&id).await.unwrap().unwrap();
        assert_eq!(info.progress, 1.0);
        assert_eq!(info.state, "completed");
        assert!(is_completed(&info));
        assert_eq!(info.size_bytes, 14);
        assert_eq!(client.get_all().await.unwrap().len(), 1);
        client.delete(&id, true).await.unwrap();
        assert!(!saved.exists());
        assert_eq!(client.get_info(&id).await.unwrap().unwrap().progress, 0.0);
    }

    #[tokio::test]
    async fn usenet_nzb_url_saved_and_completed() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/n/x.nzb"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"<nzb/>"))
            .mount(&server)
            .await;

        let client = BlackholeClient::new_usenet(&cfg(dir.path()), policy()).unwrap();
        let id = client
            .add_nzb_url(&format!("{}/n/x.nzb", server.uri()), None)
            .await
            .unwrap();

        let saved = dir.path().join("watch").join(format!("{id}.nzb"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"<nzb/>");
        let info = client.get_info(&id).await.unwrap().unwrap();
        assert_eq!(info.progress, 1.0);
        assert!(client.test().await.is_ok());

        client.delete(&id, false).await.unwrap();
        assert!(!saved.exists());
    }

    #[tokio::test]
    async fn magnet_saved_as_btih_magnet_file_without_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let client = BlackholeClient::new(&cfg(dir.path()), policy()).unwrap();
        let magnet = "magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567&dn=x";
        let id = client.add_url(magnet).await.unwrap().unwrap();

        // Hash is normalized to lowercase and used verbatim as the id.
        assert_eq!(id, "0123456789abcdef0123456789abcdef01234567");
        let saved = dir.path().join("watch").join(format!("{id}.magnet"));
        assert_eq!(std::fs::read(&saved).unwrap(), magnet.as_bytes());
        assert_eq!(
            client.get_info(&id).await.unwrap().unwrap().hash.as_deref(),
            Some(id.as_str())
        );
    }

    #[tokio::test]
    async fn unknown_id_reads_as_pending_at_zero_progress() {
        let dir = tempfile::tempdir().unwrap();
        let client = BlackholeClient::new(&cfg(dir.path()), policy()).unwrap();
        let info = client.get_info("never-added").await.unwrap().unwrap();
        assert_eq!(info.progress, 0.0);
        assert_eq!(info.state, "downloading");
        assert!(!is_completed(&info));
        // pause/resume are documented no-ops.
        assert!(client.pause("never-added").await.is_ok());
        assert!(client.resume("never-added").await.is_ok());
    }

    #[tokio::test]
    async fn missing_watch_dir_config_rejected_and_created_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let err = BlackholeClient::new(&serde_json::json!({}), policy())
            .err()
            .expect("empty config must be rejected");
        assert!(matches!(err, AdapterError::InvalidConfig(_)));

        // The watch directory itself does not exist yet; test()/add create it.
        assert!(!dir.path().join("watch").exists());
        let client = BlackholeClient::new(&cfg(dir.path()), policy()).unwrap();
        client.test().await.unwrap();
        assert!(dir.path().join("watch").is_dir());

        // Unreachable fetch target surfaces as Network, not a panic.
        let res = client.add_url("http://127.0.0.1:1/nope.torrent").await;
        assert!(matches!(res, Err(AdapterError::Network(_))));
    }

    #[tokio::test]
    async fn oversized_and_errored_responses_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; MAX_BODY_BYTES + 1]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = BlackholeClient::new(&cfg(dir.path()), policy()).unwrap();
        let res = client.add_url(&format!("{}/big", server.uri())).await;
        assert!(matches!(res, Err(AdapterError::Parse(_))));
        let res = client.add_url(&format!("{}/gone", server.uri())).await;
        assert!(matches!(res, Err(AdapterError::Network(_))));
        assert!(BlackholeClient::build(&cfg(dir.path()), policy())
            .unwrap()
            .list_payload_ids()
            .is_empty());
    }
}
