//! In-memory dry-run download client.
//!
//! Simulates a qBittorrent-like downloader for `DRY_RUN` mode: progress
//! advances deterministically with wall-clock time since each add, all state
//! lives in memory, and nothing is ever written to the filesystem. When a
//! fixture path is supplied it is adopted read-only as the reported content
//! path so downstream verification/import logic can inspect real media files.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::adapters::{is_completed, AdapterError, Downloader, TorrentInfo};

/// Default simulated 0→100% duration, mirroring the Go client.
const DEFAULT_DURATION: Duration = Duration::from_secs(25);

pub struct DryRunClient {
    inner: Mutex<Inner>,
    fixture_path: Option<PathBuf>,
    duration: Duration,
}

struct Inner {
    downloads: Vec<Download>,
    /// Monotonic counter giving "most recently added first" ordering without
    /// depending on clock resolution (two adds within one tick stay ordered).
    next_seq: u64,
}

struct Download {
    hash: String,
    name: String,
    seq: u64,
    started_at: Instant,
    /// Progress accumulated across paused intervals.
    accumulated: Duration,
    paused: bool,
}

impl DryRunClient {
    /// Returns the process-shared simulator. Handlers construct a client per
    /// job; a fresh instance per call would lose the downloads added by the
    /// previous pipeline stage (the Go client kept a package-level store).
    pub fn new(fixture_path: Option<&str>) -> Arc<dyn Downloader> {
        // First call wins for the fixture: production always passes the same
        // DRY_RUN_FIXTURE_PATH; tests use with_duration directly instead.
        let trimmed = fixture_path.map(str::trim).filter(|s| !s.is_empty());
        match trimmed {
            None => Self::shared(None),
            Some(fx) => Self::shared(Some(fx.to_string())),
        }
    }

    fn shared(fixture: Option<String>) -> Arc<dyn Downloader> {
        static SHARED: std::sync::Mutex<Option<Arc<dyn Downloader>>> =
            std::sync::Mutex::new(None);
        let mut guard = SHARED.lock().expect("dryrun shared mutex");
        match &*guard {
            Some(existing) => Arc::clone(existing),
            None => {
                let client: Arc<dyn Downloader> =
                    Arc::new(DryRunClient::with_duration(DEFAULT_DURATION, fixture.as_deref()));
                *guard = Some(Arc::clone(&client));
                client
            }
        }
    }

    /// Full constructor; tests inject a short duration for determinism.
    pub fn with_duration(duration: Duration, fixture_path: Option<&str>) -> Self {
        let fixture_path = fixture_path
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Self {
            inner: Mutex::new(Inner { downloads: Vec::new(), next_seq: 0 }),
            fixture_path,
            duration,
        }
    }

    /// Snapshot of one download at `now`.
    fn snapshot(&self, d: &Download, now: Instant) -> TorrentInfo {
        let mut elapsed = d.accumulated;
        if !d.paused {
            elapsed += now.duration_since(d.started_at);
        }
        let progress = if self.duration.is_zero() {
            1.0
        } else {
            (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0)
        };
        let mut info = TorrentInfo {
            id: d.hash.clone(),
            hash: Some(d.hash.clone()),
            name: d.name.clone(),
            progress,
            state: if d.paused { "pauseddl" } else { "downloading" }.to_string(),
            size_bytes: 0,
            downloaded_bytes: 0,
            content_path: Some(self.content_path(&d.hash)),
        };
        // Reuse the shared completion semantics: fully-progressed downloads
        // report a completed state (`uploading`) recognized by the monitor.
        if !d.paused && is_completed(&info) {
            info.state = "uploading".to_string();
        }
        info
    }

    fn content_path(&self, hash: &str) -> String {
        match &self.fixture_path {
            Some(path) => path.to_string_lossy().into_owned(),
            None => format!("dryrun://{hash}"),
        }
    }
}

#[async_trait]
impl Downloader for DryRunClient {
    fn name(&self) -> &str {
        "dryrun"
    }

    async fn add_url(&self, url: &str) -> Result<Option<String>, AdapterError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(AdapterError::InvalidConfig("torrent_url_required".into()));
        }
        let hash = fake_hash(url);
        let name = derive_name(url);

        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.downloads.iter().any(|d| d.hash == hash) {
            // Already added; treat as success.
            return Ok(Some(hash));
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.downloads.push(Download {
            hash: hash.clone(),
            name,
            seq,
            started_at: Instant::now(),
            accumulated: Duration::ZERO,
            paused: false,
        });
        Ok(Some(hash))
    }

    async fn get_info(&self, id: &str) -> Result<Option<TorrentInfo>, AdapterError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        Ok(inner
            .downloads
            .iter()
            .find(|d| d.hash.eq_ignore_ascii_case(id))
            .map(|d| self.snapshot(d, now)))
    }

    async fn get_all(&self) -> Result<Vec<TorrentInfo>, AdapterError> {
        let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        // Most recently added first.
        let mut items: Vec<&Download> = inner.downloads.iter().collect();
        items.sort_by_key(|d| std::cmp::Reverse(d.seq));
        Ok(items.into_iter().map(|d| self.snapshot(d, now)).collect())
    }

    async fn resume(&self, id: &str) -> Result<(), AdapterError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(d) = inner.downloads.iter_mut().find(|d| d.hash.eq_ignore_ascii_case(id)) {
            if d.paused {
                d.paused = false;
                d.started_at = Instant::now();
            }
        }
        Ok(())
    }

    async fn pause(&self, id: &str) -> Result<(), AdapterError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(d) = inner.downloads.iter_mut().find(|d| d.hash.eq_ignore_ascii_case(id)) {
            if !d.paused {
                d.accumulated += Instant::now().duration_since(d.started_at);
                d.paused = true;
            }
        }
        Ok(())
    }

    /// No-op in dry run.
    async fn reannounce(&self, _id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    /// No-op in dry run.
    async fn recheck(&self, _id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn delete(&self, id: &str, _delete_files: bool) -> Result<(), AdapterError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(AdapterError::InvalidConfig("hash_required".into()));
        }
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.downloads.retain(|d| !d.hash.eq_ignore_ascii_case(id));
        Ok(())
    }

    async fn test(&self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Deterministic client-side identifier: btih from magnet URLs, otherwise a
/// SHA-256 digest of the URL (the Go version used SHA-1; SHA-256 ships with
/// this crate and avoids pulling in another dependency).
fn fake_hash(url: &str) -> String {
    if let Some(btih) = super::qbittorrent::extract_btih(url) {
        return btih;
    }
    hex::encode(Sha256::digest(url.as_bytes()))
}

fn derive_name(u: &str) -> String {
    let u = u.trim();
    if u.to_ascii_lowercase().starts_with("magnet:?") {
        if let Ok(parsed) = url::Url::parse(u) {
            for (key, value) in parsed.query_pairs() {
                if key == "dn" && !value.trim().is_empty() {
                    return value.into_owned();
                }
            }
        }
    }
    "dryrun-download".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(duration_ms: u64) -> DryRunClient {
        DryRunClient::with_duration(Duration::from_millis(duration_ms), None)
    }

    #[tokio::test]
    async fn full_lifecycle_progress_pause_resume_complete_delete() {
        let c = client(200); // completes in 200ms

        // Empty initially.
        assert!(c.get_all().await.unwrap().is_empty());
        c.test().await.unwrap();

        let hash = c.add_url("magnet:?xt=urn:btih:abc123&dn=Ubuntu").await.unwrap().unwrap();
        assert_eq!(hash, "abc123");

        let early = c.get_info(&hash).await.unwrap().expect("present");
        assert_eq!(early.name, "Ubuntu");
        assert_eq!(early.state, "downloading");
        assert!(early.progress < 0.9);
        assert_eq!(early.content_path.as_deref(), Some("dryrun://abc123"));

        // Pause freezes progress.
        c.pause(&hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frozen_a = c.get_info(&hash).await.unwrap().unwrap();
        assert_eq!(frozen_a.state, "pauseddl");
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frozen_b = c.get_info(&hash).await.unwrap().unwrap();
        assert!((frozen_a.progress - frozen_b.progress).abs() < f64::EPSILON, "paused progress must not advance");

        // Resume continues toward completion.
        c.resume(&hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let done = c.get_info(&hash).await.unwrap().unwrap();
        assert_eq!(done.state, "uploading");
        assert!((done.progress - 1.0).abs() < 1e-9);
        assert!(crate::adapters::is_completed(&done));

        // Delete removes it.
        c.delete(&hash, true).await.unwrap();
        assert!(c.get_info(&hash).await.unwrap().is_none());
        assert!(c.get_all().await.unwrap().is_empty());
        assert!(matches!(c.get_info("").await, Err(AdapterError::InvalidConfig(_))));
    }

    #[tokio::test]
    async fn get_all_sorted_newest_first_and_readd_is_idempotent() {
        let c = client(60_000);
        let h1 = c.add_url("magnet:?xt=urn:btih:first&dn=One").await.unwrap().unwrap();
        let h2 = c.add_url("magnet:?xt=urn:btih:second&dn=Two").await.unwrap().unwrap();
        // Re-adding an existing torrent neither duplicates nor resets it.
        let again = c.add_url("magnet:?xt=urn:btih:first&dn=One").await.unwrap().unwrap();
        assert_eq!(again, h1);

        let all = c.get_all().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, h2, "most recently added comes first");
        assert_eq!(all[1].id, h1);
    }

    #[tokio::test]
    async fn http_urls_get_deterministic_sha256_hash() {
        let c = client(60_000);
        let url = "https://example.com/linux.iso.torrent";
        let h1 = c.add_url(url).await.unwrap().unwrap();
        let h2 = c.add_url(url).await.unwrap().unwrap();
        assert_eq!(h1, h2, "same URL mints the same hash");
        assert_eq!(h1.len(), 64, "sha256 hex digest");

        let info = c.get_info(&h1).await.unwrap().unwrap();
        assert_eq!(info.name, "dryrun-download", "non-magnet URLs use fallback name");
    }

    #[tokio::test]
    async fn fixture_path_adopted_read_only_as_content_path() {
        let dir = tempfile::tempdir().unwrap();
        let c = DryRunClient::with_duration(
            Duration::from_secs(60),
            Some(dir.path().to_str().unwrap()),
        );
        let hash = c.add_url("magnet:?xt=urn:btih:fixtured").await.unwrap().unwrap();
        let info = c.get_info(&hash).await.unwrap().unwrap();
        assert_eq!(
            info.content_path.as_deref(),
            Some(dir.path().to_str().unwrap()),
            "fixture path becomes the content path"
        );
        // No filesystem writes: the directory must still be empty.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_invalid_arguments() {
        let c = client(60_000);
        assert!(matches!(c.add_url("").await, Err(AdapterError::InvalidConfig(_))));
        assert!(matches!(c.add_url("   ").await, Err(AdapterError::InvalidConfig(_))));
        assert!(c.add_url("magnet:?xt=urn:btih:x").await.is_ok());
        assert!(matches!(c.get_info("").await, Err(AdapterError::InvalidConfig(_))));
        assert!(matches!(c.pause(" ").await, Err(AdapterError::InvalidConfig(_))));
        assert!(matches!(c.resume("").await, Err(AdapterError::InvalidConfig(_))));
        assert!(matches!(c.delete("", false).await, Err(AdapterError::InvalidConfig(_))));
        // Unknown ids are silently ignored (parity with Go).
        c.resume("unknown").await.unwrap();
        c.pause("unknown").await.unwrap();
        c.delete("unknown", false).await.unwrap();
        c.reannounce("x").await.unwrap();
        c.recheck("x").await.unwrap();
    }


    #[test]
    fn derive_name_extracts_display_name() {
        assert_eq!(derive_name("magnet:?xt=urn:btih:a&dn=My%20Show"), "My Show");
        assert_eq!(derive_name("MAGNET:?dn=CaseInsensitive"), "CaseInsensitive");
        assert_eq!(derive_name("magnet:?xt=urn:btih:a"), "dryrun-download");
        assert_eq!(derive_name("https://example.com/a.torrent"), "dryrun-download");
    }
}
