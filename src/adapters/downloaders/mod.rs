pub mod deluge;
pub mod dryrun;
pub mod qbittorrent;
pub mod transmission;

use std::sync::Arc;

use serde_json::Value;

use crate::adapters::{AdapterError, Downloader, UsenetDownloader};
use crate::security::OutboundPolicy;

/// Builds the torrent download client named by `kind`.
///
/// `kind` is one of `qbittorrent`, `deluge`, `transmission` or `dryrun`;
/// when `dry_run` is set the in-memory simulator is used regardless of kind.
/// `cfg_json` holds the client's plaintext config (e.g. `{url, username,
/// password}` for qBittorrent); credentials must already be decrypted.
/// `fixture_path` only affects the dry-run client and is adopted read-only.
pub async fn build_torrent_client(
    kind: &str,
    cfg_json: &Value,
    policy: OutboundPolicy,
    dry_run: bool,
    fixture_path: Option<&str>,
) -> Result<Arc<dyn Downloader>, AdapterError> {
    if dry_run || kind.eq_ignore_ascii_case("dryrun") {
        return Ok(dryrun::DryRunClient::new(fixture_path));
    }
    match kind {
        "qbittorrent" => qbittorrent::QbittorrentClient::new(cfg_json, policy),
        "deluge" => deluge::DelugeClient::new(cfg_json, policy).await,
        "transmission" => transmission::TransmissionClient::new(cfg_json, policy).await,
        other => Err(AdapterError::Unsupported(format!("unknown download client kind: {other}"))),
    }
}

/// Builds the usenet download client named by `kind` (`nzbget` or `sabnzbd`).
/// There is no dry-run usenet simulator; requesting one is an error.
pub async fn build_usenet_client(
    kind: &str,
    cfg_json: &Value,
    policy: OutboundPolicy,
    dry_run: bool,
    _fixture_path: Option<&str>,
) -> Result<Arc<dyn UsenetDownloader>, AdapterError> {
    if dry_run {
        return Err(AdapterError::Unsupported("dry-run is not supported for usenet clients".into()));
    }
    match kind {
        "nzbget" => super::usenet::nzbget::NzbGetClient::new(cfg_json, policy).await,
        "sabnzbd" => super::usenet::sabnzbd::SabNzbdClient::new(cfg_json, policy).await,
        other => Err(AdapterError::Unsupported(format!("unknown usenet client kind: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_run_flag_and_kind_both_select_simulator() {
        let policy = OutboundPolicy { allow_internal: true };
        let by_kind =
            build_torrent_client("dryrun", &serde_json::json!({}), policy, false, None).await.unwrap();
        assert_eq!(Downloader::name(by_kind.as_ref()), "dryrun");
        let policy = OutboundPolicy { allow_internal: true };
        let by_flag =
            build_torrent_client("qbittorrent", &serde_json::json!({}), policy, true, None).await.unwrap();
        assert_eq!(Downloader::name(by_flag.as_ref()), "dryrun");
    }

    #[tokio::test]
    async fn unknown_kinds_are_rejected() {
        let policy = OutboundPolicy { allow_internal: true };
        let err = build_torrent_client("rtorrent", &serde_json::json!({}), policy, false, None).await.err().expect("expected error");
        assert!(matches!(err, AdapterError::Unsupported(_)));
        let err = build_usenet_client("napster", &serde_json::json!({}), policy, false, None).await.err().expect("expected error");
        assert!(matches!(err, AdapterError::Unsupported(_)));
        let err = build_usenet_client("nzbget", &serde_json::json!({}), policy, true, None).await.err().expect("expected error");
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn dry_run_client_is_process_shared_across_builder_calls() {
        // Handlers build a client per job; the store must be shared or the
        // torrent added by enqueue would vanish for monitor (Go parity).
        let policy = OutboundPolicy { allow_internal: true };
        let a = build_torrent_client("dryrun", &serde_json::json!({}), policy, true, None)
            .await
            .ok()
            .expect("builder ok");
        let b = build_torrent_client("qbittorrent", &serde_json::json!({}), policy, true, None)
            .await
            .ok()
            .expect("builder ok");
        let hash = a
            .add_url("magnet:?xt=urn:btih:sharedcheck")
            .await
            .unwrap()
            .expect("dry run returns hash");
        let seen_by_b = b.get_info(&hash).await.unwrap().expect("found via second handle");
        assert_eq!(seen_by_b.hash.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn fixtureless_dryrun_uses_dryrun_content_scheme() {
        // Without a fixture, content paths surface as dryrun:// URIs (no real
        // filesystem writes); with one, the fixture path itself is used.
        let client = crate::adapters::downloaders::dryrun::DryRunClient::with_duration(
            std::time::Duration::from_secs(0),
            None,
        );
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let hash = rt.block_on(async {
            client
                .add_url("magnet:?xt=urn:btih:fixturecheck")
                .await
                .unwrap()
                .expect("hash")
        });
        let info = rt
            .block_on(async { client.get_info(&hash).await.unwrap().expect("found") });
        assert_eq!(info.content_path.as_deref(), Some("dryrun://fixturecheck"));
    }

}
