//! Torznab re-exports the shared Newznab-protocol engine with torrent
//! semantics (see [`crate::adapters::indexers::newznab`]).

pub use super::newznab::{IndexerConfig, TorznabClient};
