//! Shared lookup/build helpers for job handlers: setup paths, decisioning
//! policies, target queries, indexer clients, and download clients.
//!
//! Fixes over the Go original (`internal/jobs/handlers/search_common.go`):
//! - No "ubuntu" fallback query: a missing target or query is an error.
//! - Indexer/client credentials are decrypted through the shared SecretBox;
//!   outbound clients are only ever built through the adapter builders, which
//!   route every request through SSRF validation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;

use crate::adapters::indexers::newznab::redact_apikey;
use crate::adapters::{Downloader, Indexer};
use crate::pipeline::decisioning::{self, Policies};
use crate::state::AppState;
use crate::storage::settings;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SetupPaths {
    #[serde(default, rename = "downloadsCompletedPath")]
    pub downloads_completed_path: String,
    #[serde(default, rename = "libraryRoots")]
    pub library_roots: Vec<String>,
    #[serde(default, rename = "quarantinePath")]
    pub quarantine_path: String,
}

pub async fn load_setup_paths(state: &AppState) -> Result<SetupPaths, String> {
    let raw = settings::get(&state.db, "setup.paths")
        .await
        .map_err(|e| e.to_string())?
        .ok_or("setup_paths_missing")?;
    serde_json::from_str(&raw).map_err(|e| format!("setup_paths_invalid: {e}"))
}

/// Loads decisioning policies; a malformed setting logs a warning and yields
/// defaults (matching the original's leniency) rather than stalling the queue.
pub async fn load_policies(state: &AppState) -> Policies {
    match settings::get(&state.db, "setup.policies").await {
        Ok(Some(raw)) => match decisioning::parse_policies(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "setup.policies invalid; using defaults");
                Policies::default()
            }
        },
        Ok(None) => Policies::default(),
        Err(e) => {
            tracing::warn!(error = %e, "setup.policies unreadable; using defaults");
            Policies::default()
        }
    }
}

/// Loads the wanted target's search query. There is NO fallback query: a
/// target without a usable query fails the run (the original silently
/// searched "ubuntu").
pub async fn load_target_query(state: &AppState, target_id: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default, rename = "query")]
        query: serde_json::Value,
    }
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT payload_json FROM wanted_targets WHERE id = ?")
            .bind(target_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    let (payload_json,) = row.ok_or("target_not_found")?;
    let payload_json = payload_json.ok_or("target_query_missing")?;
    let payload: Payload =
        serde_json::from_str(&payload_json).map_err(|e| format!("target_payload_invalid: {e}"))?;
    match payload.query {
        serde_json::Value::String(q) if !q.trim().is_empty() => Ok(q.trim().to_string()),
        _ => Err("target_query_missing".to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct PersistedIndexer {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "apiKeyEnc")]
    api_key_enc: String,
    #[serde(default)]
    username: String,
    #[serde(default, rename = "passwordEnc")]
    password_enc: String,
}

#[derive(Debug, Deserialize, Default)]
struct PersistedIndexers {
    #[serde(default)]
    torznab: Vec<PersistedIndexer>,
    #[serde(default)]
    newznab: Vec<PersistedIndexer>,
}

/// Builds live indexer clients from the encrypted `setup.indexers` setting.
/// Credentials are decrypted with the shared SecretBox; entries whose secrets
/// cannot be decrypted or whose config the adapter rejects are reported as
/// events and skipped, matching the original's per-indexer resilience.
pub async fn load_indexers(
    state: &AppState,
    run_id: &str,
    target_id: &str,
) -> Result<Vec<Arc<dyn Indexer>>, String> {
    let raw = settings::get(&state.db, "setup.indexers")
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no_indexers_configured")?;
    let persisted: PersistedIndexers =
        serde_json::from_str(&raw).map_err(|e| format!("indexers_config_invalid: {e}"))?;

    let mut out = Vec::new();
    for (kind, entries) in [
        ("torznab", &persisted.torznab),
        ("newznab", &persisted.newznab),
    ] {
        for ix in entries {
            let mut cfg = serde_json::Map::new();
            // IndexerConfig has no `kind` field: serde would silently drop
            // it and every indexer would load as newznab/usenet. Pass the
            // flag the adapter actually reads.
            if kind == "torznab" {
                cfg.insert("torznab".into(), serde_json::Value::Bool(true));
            }
            cfg.insert("name".into(), serde_json::Value::String(ix.name.clone()));
            cfg.insert("url".into(), serde_json::Value::String(ix.url.clone()));
            match state.secret_box.decrypt_string(&ix.api_key_enc) {
                Ok(key) => {
                    cfg.insert(
                        "api_key".into(),
                        serde_json::Value::String(String::from_utf8_lossy(&key).into_owned()),
                    );
                }
                Err(e) => {
                    crate::jobs::handlers::publish(
                        state,
                        "search.indexer.error",
                        serde_json::json!({
                            "runId": run_id,
                            "targetId": target_id,
                            "indexer": ix.name,
                            "error": redact_apikey(&format!("decrypt_failed: {e}"))
                        }),
                    );
                    continue;
                }
            }
            if !ix.username.is_empty() {
                cfg.insert(
                    "username".into(),
                    serde_json::Value::String(ix.username.clone()),
                );
            }
            if !ix.password_enc.is_empty() {
                match state.secret_box.decrypt_string(&ix.password_enc) {
                    Ok(pw) => {
                        cfg.insert(
                            "password".into(),
                            serde_json::Value::String(String::from_utf8_lossy(&pw).into_owned()),
                        );
                    }
                    Err(e) => {
                        crate::jobs::handlers::publish(
                            state,
                            "search.indexer.error",
                            serde_json::json!({
                                "runId": run_id,
                                "targetId": target_id,
                                "indexer": ix.name,
                                "error": redact_apikey(&format!("decrypt_failed: {e}"))
                            }),
                        );
                        continue;
                    }
                }
            }
            match crate::adapters::indexers::build_indexer(
                &serde_json::Value::Object(cfg),
                state.policy,
            ) {
                Ok(cli) => out.push(cli),
                Err(e) => {
                    crate::jobs::handlers::publish(
                        state,
                        "search.indexer.error",
                        serde_json::json!({
                            "runId": run_id,
                            "targetId": target_id,
                            "indexer": ix.name,
                            "error": redact_apikey(&format!("client_create_failed: {e}"))
                        }),
                    );
                }
            }
        }
    }
    Ok(out)
}

/// Builds the torrent download client for `client_kind` from `setup.clients`.
///
/// Any pipeline-supported kind works (qbittorrent/deluge/transmission/blackhole);
/// the client's password is decrypted with the shared SecretBox. In dry-run the
/// in-memory simulator is used and no credentials are needed.
pub async fn load_torrent_client(
    state: &AppState,
    client_kind: &str,
) -> Result<Arc<dyn Downloader>, String> {
    let mut cfg = serde_json::Map::new();
    if !state.config.dry_run {
        #[derive(Clone, Deserialize)]
        struct ClientCfg {
            #[serde(default)]
            url: String,
            #[serde(default)]
            username: String,
            #[serde(default, rename = "passwordEnc")]
            password_enc: String,
        }
        #[derive(Deserialize)]
        struct Clients {
            #[serde(flatten)]
            entries: std::collections::BTreeMap<String, Option<ClientCfg>>,
        }
        let raw = settings::get(&state.db, "setup.clients")
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no_clients_configured")?;
        let clients: Clients =
            serde_json::from_str(&raw).map_err(|e| format!("clients_config_invalid: {e}"))?;
        let entry = clients
            .entries
            .get(client_kind)
            .cloned()
            .flatten()
            .ok_or_else(|| format!("{client_kind}_not_configured"))?;
        let password = if entry.password_enc.is_empty() {
            String::new()
        } else {
            String::from_utf8_lossy(
                &state
                    .secret_box
                    .decrypt_string(&entry.password_enc)
                    .map_err(|_| "decrypt_failed".to_string())?,
            )
            .into_owned()
        };
        cfg.insert("url".into(), serde_json::Value::String(entry.url));
        cfg.insert("username".into(), serde_json::Value::String(entry.username));
        cfg.insert("password".into(), serde_json::Value::String(password));
    }
    crate::adapters::downloaders::build_torrent_client(
        client_kind,
        &serde_json::Value::Object(cfg),
        state.policy,
        state.config.dry_run,
        state.config.dry_run_fixture_path.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Extracts the btih info hash from a magnet link, if present.
pub fn extract_btih(url: &str) -> Option<String> {
    let u = url.trim();
    if !u.to_ascii_lowercase().starts_with("magnet:?") {
        return None;
    }
    let lower = u.to_ascii_lowercase();
    let marker = "xt=urn:btih:";
    let idx = lower.find(marker)?;
    let start = idx + marker.len();
    let end = u[start..]
        .find(['&', '#'])
        .map(|i| start + i)
        .unwrap_or(u.len());
    let h = u[start..end].trim();
    if h.is_empty() {
        None
    } else {
        Some(h.to_string())
    }
}

/// Loads the set of already-tried external ids (hashes) for a target.
pub async fn tried_external_ids(
    state: &AppState,
    target_id: &str,
) -> Result<HashSet<String>, String> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT external_id FROM acquisitions WHERE target_id = ? AND external_id IS NOT NULL",
    )
    .bind(target_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .filter_map(|(v,)| v)
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .collect())
}

/// Loads the blacklist (global + target scope) as match_type → value set.
pub async fn blacklist_set(
    state: &AppState,
    target_id: &str,
) -> Result<HashMap<String, HashSet<String>>, String> {
    crate::storage::blacklist::load_set(&state.db, target_id)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;

    #[test]
    fn extract_btih_variants() {
        assert_eq!(
            extract_btih("magnet:?xt=urn:btih:ABCDEF&dn=x").as_deref(),
            Some("ABCDEF")
        );
        assert_eq!(
            extract_btih("MAGNET:?XT=URN:BTIH:abcdef").as_deref(),
            Some("abcdef")
        );
        assert_eq!(extract_btih("magnet:?dn=only"), None);
        assert_eq!(extract_btih("https://example.com/file.torrent"), None);
        assert_eq!(extract_btih("magnet:?xt=urn:btih:"), None);
    }

    #[tokio::test]
    async fn load_target_query_requires_query() {
        let (_dir, state) = testutil::test_state().await;
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, payload_json, created_at, updated_at)
             VALUES ('t1', 'movie', 'tmdb', '550', 'wanted', ?, ?, ?)",
        )
        .bind(r#"{"query":"Test Movie"}"#)
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
        assert_eq!(load_target_query(&state, "t1").await.unwrap(), "Test Movie");

        sqlx::query("UPDATE wanted_targets SET payload_json = NULL WHERE id = 't1'")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            load_target_query(&state, "t1").await.unwrap_err(),
            "target_query_missing"
        );
        assert_eq!(
            load_target_query(&state, "missing").await.unwrap_err(),
            "target_not_found"
        );
    }

    #[tokio::test]
    async fn load_indexers_requires_setting() {
        let (_dir, state) = testutil::test_state().await;
        assert_eq!(
            load_indexers(&state, "r", "t")
                .await
                .err()
                .expect("expected error"),
            "no_indexers_configured"
        );
    }

    #[tokio::test]
    async fn load_setup_paths_requires_setting() {
        let (_dir, state) = testutil::test_state().await;
        assert_eq!(
            load_setup_paths(&state).await.unwrap_err(),
            "setup_paths_missing"
        );
    }

    #[tokio::test]
    async fn dry_run_builds_client_without_credentials() {
        let (_dir, state) = testutil::test_state().await;
        // test_state uses dry_run=true: no setup.clients needed.
        load_torrent_client(&state, "qbittorrent")
            .await
            .expect("dry-run client");
    }
}
