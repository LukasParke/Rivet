//! TVDB metadata provider.
//!
//! Fixes the Go client (`internal/adapters/metadata/tvdb/client.go`), which was
//! a validate-only stub that logged in and discarded the token. Here the token
//! is cached on the client and reused as a Bearer credential for searches.
//!
//! All outbound calls route through [`crate::security::ssrf`]: every URL is
//! resolved and policy-checked via [`validate_url`] before any bytes are sent,
//! and connections are pinned to the validated addresses via [`pinned_client`].

use std::time::Duration;
use tokio::sync::Mutex;

use async_trait::async_trait;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::adapters::{AdapterError, MetadataProvider};
use crate::security::ssrf::{pinned_client, validate_url, OutboundPolicy};

/// Pinned TVDB API origin (v4).
pub const DEFAULT_BASE_URL: &str = "https://api4.thetvdb.com";

const TIMEOUT: Duration = Duration::from_secs(8);

/// Adapter configuration (decrypted settings payload). An API key is required:
/// unlike TMDB there is no shared demo key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TvdbConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    /// v4 returns `tvdb_id`; tolerate `id` too. May be string or number.
    #[serde(alias = "id")]
    tvdb_id: Option<Value>,
    name: Option<String>,
}

impl SearchItem {
    fn into_pair(self) -> Option<(String, String)> {
        let name = self.name?;
        let id = match self.tvdb_id? {
            Value::String(s) => s,
            other => other.to_string(),
        };
        Some((id, name))
    }
}

/// One TVDB series episode, normalized for series enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvdbEpisode {
    /// Episode number within its season.
    pub number: u32,
    pub season_number: u32,
    pub name: Option<String>,
    /// Air date as `YYYY-MM-DD`, when the API returned one.
    pub aired: Option<String>,
    pub runtime: Option<i64>,
    pub overview: Option<String>,
    /// Series title echoed on episode rows, when present.
    pub series_name: Option<String>,
}

/// Reads an optional trimmed string field; empty or non-string values are absent.
fn opt_str(item: &Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn opt_i64(item: &Value, key: &str) -> Option<i64> {
    match item.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// v4 episode ids/numbers arrive as numbers but tolerate strings (same
/// leniency as [`SearchItem`]).
fn opt_u32(item: &Value, key: &str) -> Option<u32> {
    match item.get(key) {
        Some(Value::Number(n)) => n.as_u64().map(|n| n as u32),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Maps one TVDB episode to [`TvdbEpisode`], skipping malformed items.
fn parse_episode(item: &Value) -> Option<TvdbEpisode> {
    Some(TvdbEpisode {
        number: opt_u32(item, "number")?,
        season_number: opt_u32(item, "seasonNumber")?,
        name: opt_str(item, "name"),
        aired: opt_str(item, "aired"),
        runtime: opt_i64(item, "runtime"),
        overview: opt_str(item, "overview"),
        series_name: opt_str(item, "seriesName"),
    })
}

/// TVDB API client with a lazily-obtained, cached bearer token.
#[derive(Debug)]
pub struct TvdbClient {
    base_url: String,
    api_key: String,
    policy: OutboundPolicy,
    token: Mutex<Option<String>>,
}

impl TvdbClient {
    /// Builds a client; fails fast when no API key is configured.
    pub fn new(config: &TvdbConfig, policy: OutboundPolicy) -> Result<Self, AdapterError> {
        let api_key = config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| AdapterError::InvalidConfig("tvdb api key required".into()))?;
        Ok(Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.to_string(),
            policy,
            token: Mutex::new(None),
        })
    }

    /// Test seam: point the client at a different origin (wiremock).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn build_url(&self, path: &str, extra: &[(&str, &str)]) -> Result<Url, AdapterError> {
        let mut url = Url::parse(&format!("{}{}", self.base_url.trim_end_matches('/'), path))
            .map_err(|e| AdapterError::InvalidConfig(format!("invalid base URL: {e}")))?;
        for (k, v) in extra {
            url.query_pairs_mut().append_pair(k, v);
        }
        Ok(url)
    }

    /// Performs a validated, address-pinned request via `security::ssrf`:
    /// `validate_url` resolves and policy-checks the URL, `pinned_client`
    /// pins the connection to the vetted addresses. Redirects are never
    /// followed (a 3xx surfaces as a network error, not an unvalidated hop).
    async fn request_validated(
        &self,
        method: Method,
        url: Url,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, AdapterError> {
        let target = validate_url(url.as_str(), self.policy)
            .await
            .map_err(|e| AdapterError::Network(e.to_string()))?;
        let client = pinned_client(&target, TIMEOUT);

        let mut req = client.request(method, target.url);
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        match status.as_u16() {
            401 | 403 => return Err(AdapterError::Auth),
            _ if !status.is_success() => {
                return Err(AdapterError::Network(format!("tvdb http_error: {status}")))
            }
            _ => {}
        }
        serde_json::from_str(&text).map_err(|e| AdapterError::Parse(e.to_string()))
    }

    /// POSTs `/v4/login` with the configured key and caches the returned token.
    pub async fn login(&self) -> Result<(), AdapterError> {
        self.ensure_token().await.map(|_| ())
    }

    async fn login_token(&self) -> Result<String, AdapterError> {
        let url = self.build_url("/v4/login", &[])?;
        let json = self
            .request_validated(
                Method::POST,
                url,
                None,
                Some(&serde_json::json!({ "apikey": self.api_key })),
            )
            .await?;
        let token = json
            .pointer("/data/token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            return Err(AdapterError::Parse("unexpected_response".into()));
        }
        Ok(token)
    }

    /// Returns the cached token or logs in again and caches it.
    async fn ensure_token(&self) -> Result<String, AdapterError> {
        if let Some(token) = self.token.lock().await.clone() {
            return Ok(token);
        }
        let token = self.login_token().await?;
        *self.token.lock().await = Some(token.clone());
        Ok(token)
    }

    /// Drops any cached token (after an auth failure) so the next call re-logins.
    async fn invalidate_token(&self) {
        *self.token.lock().await = None;
    }

    /// Searches series by name, returning `(id, name)` pairs.
    pub async fn search_series(&self, name: &str) -> Result<Vec<(String, String)>, AdapterError> {
        if name.trim().is_empty() {
            return Err(AdapterError::InvalidConfig("empty search query".into()));
        }
        let token = self.ensure_token().await?;
        let url = self.build_url("/v4/search", &[("query", name), ("type", "series")])?;
        match self
            .request_validated(Method::GET, url, Some(&token), None)
            .await
        {
            Ok(json) => {
                let data: Vec<(String, String)> = serde_json::from_value::<SearchResponse>(json)
                    .map_err(|e| AdapterError::Parse(e.to_string()))?
                    .data
                    .into_iter()
                    .filter_map(SearchItem::into_pair)
                    .collect();
                Ok(data)
            }
            Err(AdapterError::Auth) => {
                // Token may have expired mid-session; force a fresh login next time.
                self.invalidate_token().await;
                Err(AdapterError::Auth)
            }
            Err(other) => Err(other),
        }
    }

    /// Connectivity/credential check: a successful login.
    pub async fn validate(&self) -> Result<(), AdapterError> {
        self.login().await
    }

    /// Upper bound on `links.next` pagination hops (v4 pages of 100).
    const MAX_EPISODE_PAGES: usize = 20;

    /// Fetches all episodes (`GET /v4/series/{id}/episodes/default`), following
    /// the `data.links.next` cursor up to [`MAX_EPISODE_PAGES`] pages. Entries
    /// without a parseable episode/season number are skipped.
    pub async fn series_episodes(&self, series_id: &str) -> Result<Vec<TvdbEpisode>, AdapterError> {
        let token = self.ensure_token().await?;
        let mut out: Vec<TvdbEpisode> = Vec::new();
        // First hop is a base-relative path; later hops come from the API's
        // `next` link, which may be relative or absolute.
        let mut next: Option<String> = Some(format!("/v4/series/{series_id}/episodes/default"));
        for _ in 0..Self::MAX_EPISODE_PAGES {
            let Some(link) = next.take() else { break };
            let url = if link.starts_with("http://") || link.starts_with("https://") {
                Url::parse(&link)
                    .map_err(|e| AdapterError::InvalidConfig(format!("bad next link: {e}")))?
            } else {
                self.build_url(&link, &[])?
            };
            let json = match self
                .request_validated(Method::GET, url, Some(&token), None)
                .await
            {
                Ok(json) => json,
                Err(AdapterError::Auth) => {
                    // Token may have expired mid-pagination; force re-login next time.
                    self.invalidate_token().await;
                    return Err(AdapterError::Auth);
                }
                Err(other) => return Err(other),
            };
            let episodes = json
                .pointer("/data/episodes")
                .and_then(Value::as_array)
                .ok_or_else(|| AdapterError::Parse("missing episodes array".into()))?;
            out.extend(episodes.iter().filter_map(parse_episode));
            next = json
                .pointer("/data/links/next")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        Ok(out)
    }
}

#[async_trait]
impl MetadataProvider for TvdbClient {
    fn name(&self) -> &str {
        "tvdb"
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.validate().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{
        body_json, header, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(uri: String) -> TvdbClient {
        TvdbClient::new(
            &TvdbConfig {
                api_key: Some("key".into()),
            },
            OutboundPolicy::dev(),
        )
        .unwrap()
        .with_base_url(uri)
    }

    fn login_mock() -> Mock {
        Mock::given(method("POST"))
            .and(path("/v4/login"))
            .and(body_json(serde_json::json!({"apikey": "key"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": {"token": "tok"}})),
            )
    }

    #[tokio::test]
    async fn missing_api_key_rejected_at_construction() {
        for cfg in [
            TvdbConfig::default(),
            TvdbConfig {
                api_key: Some("  ".into()),
            },
        ] {
            let err = TvdbClient::new(&cfg, OutboundPolicy::dev()).unwrap_err();
            assert!(matches!(err, AdapterError::InvalidConfig(_)));
        }
    }

    #[tokio::test]
    async fn test_is_login_success() {
        let server = MockServer::start().await;
        login_mock().expect(1).mount(&server).await;
        client(server.uri()).test().await.unwrap();
    }

    #[tokio::test]
    async fn bad_api_key_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v4/login"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(server.uri()).test().await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth));
    }

    #[tokio::test]
    async fn empty_token_is_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v4/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": {"token": ""}})),
            )
            .mount(&server)
            .await;
        let err = client(server.uri()).test().await.unwrap_err();
        assert!(matches!(err, AdapterError::Parse(_)));
    }

    #[tokio::test]
    async fn search_uses_cached_bearer_and_parses_results() {
        let server = MockServer::start().await;
        login_mock().expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/v4/search"))
            .and(query_param("query", "Friends"))
            .and(query_param("type", "series"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"tvdb_id": "79168", "name": "Friends", "type": "series"},
                    {"tvdb_id": 12345, "name": "Numeric Id Show"}
                ]
            })))
            .mount(&server)
            .await;

        let client = client(server.uri());
        let hits = client.search_series("Friends").await.unwrap();
        assert_eq!(
            hits,
            vec![
                ("79168".to_string(), "Friends".to_string()),
                ("12345".to_string(), "Numeric Id Show".to_string())
            ]
        );

        // Second search must reuse the cached token: still exactly one login.
        assert!(client.search_series("Friends").await.is_ok());
    }

    #[tokio::test]
    async fn expired_token_invalidated_on_auth_failure() {
        let server = MockServer::start().await;
        login_mock().mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/v4/search"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = client(server.uri());
        let err = client.search_series("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth));
        assert!(client.token.lock().await.is_none());
    }

    #[tokio::test]
    async fn empty_query_rejected_without_network_call() {
        let server = MockServer::start().await;
        let err = client(server.uri()).search_series(" ").await.unwrap_err();
        assert!(matches!(err, AdapterError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn server_error_maps_to_network() {
        let server = MockServer::start().await;
        login_mock().mount(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = client(server.uri()).search_series("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));
    }

    fn episode(n: u32, season: u32) -> serde_json::Value {
        serde_json::json!({
            "number": n.to_string(), // v4 tolerates numeric strings
            "seasonNumber": season,
            "name": format!("Episode {n}"),
            "aired": format!("2026-01-{n:02}"),
            "runtime": 45,
            "overview": format!("Overview {n}"),
            "seriesName": "Friends"
        })
    }

    #[tokio::test]
    async fn series_episodes_follows_next_link_and_parses_defensively() {
        let server = MockServer::start().await;
        login_mock().expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/v4/series/79168/episodes/default"))
            .and(header("authorization", "Bearer tok"))
            .and(query_param_is_missing("page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "episodes": [episode(1, 1), {"broken": true}, episode(2, 1)],
                    "links": {"next": "/v4/series/79168/episodes/default?page=2"}
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v4/series/79168/episodes/default"))
            .and(query_param("page", "2"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "episodes": [episode(1, 2)],
                    "links": {"next": null}
                }
            })))
            .mount(&server)
            .await;

        let client = client(server.uri());
        let eps = client.series_episodes("79168").await.unwrap();
        assert_eq!(eps.len(), 3, "malformed entries are skipped");
        assert_eq!(eps[0].number, 1);
        assert_eq!(eps[0].season_number, 1);
        assert_eq!(eps[0].name.as_deref(), Some("Episode 1"));
        assert_eq!(eps[0].aired.as_deref(), Some("2026-01-01"));
        assert_eq!(eps[0].runtime, Some(45));
        assert_eq!(eps[0].series_name.as_deref(), Some("Friends"));
        assert_eq!(eps[2].season_number, 2);
    }

    #[tokio::test]
    async fn series_episodes_missing_array_is_parse_error() {
        let server = MockServer::start().await;
        login_mock().mount(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"links": {}}
            })))
            .mount(&server)
            .await;
        let err = client(server.uri())
            .series_episodes("79168")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Parse(_)));
    }

    #[tokio::test]
    async fn series_episodes_auth_failure_invalidates_token() {
        let server = MockServer::start().await;
        login_mock().mount(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = client(server.uri());
        let err = client.series_episodes("79168").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth));
        assert!(client.token.lock().await.is_none());
    }

    #[tokio::test]
    async fn series_episodes_server_error_maps_to_network() {
        let server = MockServer::start().await;
        login_mock().mount(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = client(server.uri())
            .series_episodes("79168")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));
    }
}
