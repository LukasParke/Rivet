//! Adapter contracts: indexers, download clients, usenet clients, metadata.
//!
//! Every outbound call made by adapters MUST route through
//! [`crate::security::ssrf`] (validated, IP-pinned requests). Credentials are
//! decrypted from settings via the shared SecretBox at construction time.

pub mod downloaders;
pub mod indexers;
pub mod metadata;
pub mod usenet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseKind {
    Torrent,
    Usenet,
}

/// A normalized search result from any indexer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub indexer_name: String,
    pub title: String,
    pub download_url: String,
    pub info_url: Option<String>,
    pub kind: ReleaseKind,
    pub size_bytes: Option<i64>,
    pub seeders: Option<i64>,
    pub peers: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    /// Extracted info hash (btih) when present in the result/magnet.
    pub info_hash: Option<String>,
    /// Raw provider payload for evidence/debugging (redact secrets first).
    pub raw: serde_json::Value,
    /// Normalized quality token, e.g. "2160p"|"1080p"|"720p"|"web"|"hdtv".
    pub quality: Option<String>,
    /// Tokenized tags (proper, repack, web-dl, ...).
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: String,
    pub categories: Vec<i64>,
    pub limit: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("auth failed")]
    Auth,
    #[error("parse failed: {0}")]
    Parse(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl From<reqwest::Error> for AdapterError {
    fn from(e: reqwest::Error) -> Self {
        AdapterError::Network(e.to_string())
    }
}

/// Search-only indexer contract (Newznab/Torznab share this wire format).
#[async_trait]
pub trait Indexer: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, query: &SearchQuery) -> Result<Vec<Release>, AdapterError>;
    /// Connectivity/credential check for the setup wizard.
    async fn test(&self) -> Result<(), AdapterError>;
}

/// Torrent download client contract (qBittorrent, Deluge, Transmission, dry-run).
#[async_trait]
pub trait Downloader: Send + Sync {
    fn name(&self) -> &str;
    /// Adds a torrent by URL (http(s) or magnet). Returns the client-side
    /// identifier when immediately known.
    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError>;
    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError>;
    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError>;
    async fn resume(&self, id: &str) -> Result<(), AdapterError>;
    async fn pause(&self, id: &str) -> Result<(), AdapterError>;
    /// Forces reannounce/recheck where supported; unsupported clients return
    /// [`AdapterError::Unsupported`].
    async fn reannounce(&self, id: &str) -> Result<(), AdapterError> {
        let _ = id;
        Err(AdapterError::Unsupported("reannounce".into()))
    }
    async fn recheck(&self, id: &str) -> Result<(), AdapterError> {
        let _ = id;
        Err(AdapterError::Unsupported("recheck".into()))
    }
    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError>;
    /// Connectivity/credential check for the setup wizard.
    async fn test(&self) -> Result<(), AdapterError>;
}

/// Usenet download client contract (NZBGet, SABnzbd).
#[async_trait]
pub trait UsenetDownloader: Send + Sync {
    fn name(&self) -> &str;
    async fn add_nzb_url(&self, url: &str, category: Option<&str>) -> Result<String, AdapterError>;
    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError>;
    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError>;
    async fn resume(&self, id: &str) -> Result<(), AdapterError>;
    async fn pause(&self, id: &str) -> Result<(), AdapterError>;
    async fn delete(&self, id: &str, delete_files: bool) -> Result<(), AdapterError>;
    async fn test(&self) -> Result<(), AdapterError>;
}

/// Client-agnostic progress snapshot (mirrors qBittorrent's TorrentInfo shape).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentInfo {
    pub id: String,
    /// For torrents: btih hash; for usenet: client NZB id.
    pub hash: Option<String>,
    pub name: String,
    /// 0.0 - 1.0
    pub progress: f64,
    /// Client state string, lowercased (downloading, stalledup, stoppeddl, ...).
    pub state: String,
    pub size_bytes: i64,
    pub downloaded_bytes: i64,
    /// Content save path when the client reports it.
    pub content_path: Option<String>,
}

/// Completion semantics shared by the monitor: progress complete or a
/// seeding/finished state.
pub fn is_completed(info: &TorrentInfo) -> bool {
    info.progress >= 0.999
        || matches!(
            info.state.as_str(),
            "uploading"
                | "stalledup"
                | "pausedup"
                | "stoppedup"
                | "checkingup"
                | "forcedup"
                | "completed"
        )
}

/// Paused semantics including qBittorrent v5 renamed states.
pub fn is_paused(info: &TorrentInfo) -> bool {
    matches!(
        info.state.as_str(),
        "pauseddl" | "pausedup" | "stoppeddl" | "stoppedup" | "paused"
    )
}

/// Metadata provider contract (TMDB/TVDB).
#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn name(&self) -> &str;
    /// Connectivity/credential check for the setup wizard.
    async fn test(&self) -> Result<(), AdapterError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_and_pause_states() {
        let mk = |progress: f64, state: &str| TorrentInfo {
            progress,
            state: state.into(),
            ..Default::default()
        };
        assert!(is_completed(&mk(1.0, "downloading")));
        assert!(is_completed(&mk(0.5, "uploading")));
        // qBittorrent v5 renamed states recognized.
        assert!(is_completed(&mk(0.5, "stoppedup")));
        assert!(is_paused(&mk(0.1, "stoppeddl")));
        assert!(is_paused(&mk(0.1, "pauseddl")));
        assert!(is_paused(&mk(0.1, "paused")), "deluge generic paused state");
        assert!(!is_paused(&mk(0.1, "downloading")));
        assert!(!is_completed(&mk(0.1, "downloading")));
    }
}
