pub mod newznab;
pub mod torznab;

use self::newznab::IndexerConfig;
use crate::adapters::{AdapterError, Indexer};
use crate::security::OutboundPolicy;
use std::sync::Arc;

/// Builds an indexer from a settings JSON blob:
/// `{name, url, apikey, torznab?, prowlarr?}`.
pub fn build_indexer(
    cfg_json: &serde_json::Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn Indexer>, AdapterError> {
    let cfg: IndexerConfig = serde_json::from_value(cfg_json.clone())
        .map_err(|e| AdapterError::InvalidConfig(e.to_string()))?;
    if cfg.torznab {
        super::indexers::torznab::TorznabClient::new(&cfg, policy)
    } else {
        super::indexers::newznab::NewznabClient::new(&cfg, policy)
    }
}
