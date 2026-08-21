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
    use wiremock::matchers::{body_json, header, method, path, query_param};
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
}
