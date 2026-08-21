//! TMDB metadata provider.
//!
//! Parity with `internal/adapters/metadata/tmdb/client.go`: `/3/configuration`
//! doubles as the credential check, and an empty configured key falls back to
//! the shared demo key for an out-of-box experience. All outbound calls route
//! through [`crate::security::ssrf`] (validated, IP-pinned requests).

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::adapters::{AdapterError, MetadataProvider};
use crate::security::ssrf::{get_validated, OutboundPolicy};

/// Documented shared demo key from the Go client (`fallbackTMDBAPIKey`).
/// Rate-limited and shared across users; users should register their own free
/// key at <https://www.themoviedb.org/settings/api> for better rate limits and
/// ToS compliance. Kept for out-of-box experience only.
pub const DEMO_API_KEY: &str = "c9856d0cb57c3f14bf75bdc6c063b8f3";

/// Pinned TMDB API origin.
pub const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org";

const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_HOPS: usize = 3;

/// Adapter configuration (decrypted settings payload).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TmdbConfig {
    /// When absent/empty, [`DEMO_API_KEY`] is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// A normalized movie/show search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TmdbSearchResult {
    pub id: String,
    /// Movie title or show name, whichever the searched collection uses.
    pub title: String,
    /// Release/first-air year, when the API returned a parseable date.
    pub year: Option<i32>,
}

/// TMDB v3 API client.
#[derive(Debug, Clone)]
pub struct TmdbClient {
    base_url: String,
    api_key: String,
    policy: OutboundPolicy,
}

impl TmdbClient {
    /// Builds a client; an empty configured key falls back to [`DEMO_API_KEY`].
    pub fn new(config: &TmdbConfig, policy: OutboundPolicy) -> Self {
        let api_key = match config.api_key.as_deref() {
            Some(key) => key.trim(),
            None => "",
        };
        let api_key = if api_key.is_empty() {
            DEMO_API_KEY.to_string()
        } else {
            api_key.to_string()
        };
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            policy,
        }
    }

    /// Test seam: point the client at a different origin (wiremock).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn build_url(&self, path: &str, extra: &[(&str, &str)]) -> Result<String, AdapterError> {
        let mut url = Url::parse(&format!("{}{}", self.base_url.trim_end_matches('/'), path))
            .map_err(|e| AdapterError::InvalidConfig(format!("invalid base URL: {e}")))?;
        url.query_pairs_mut().append_pair("api_key", &self.api_key);
        for (k, v) in extra {
            url.query_pairs_mut().append_pair(k, v);
        }
        Ok(url.to_string())
    }

    async fn get_json(&self, url: &str) -> Result<Value, AdapterError> {
        let resp = get_validated(url, self.policy, TIMEOUT, MAX_HOPS)
            .await
            .map_err(|e| AdapterError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AdapterError::Network(e.to_string()))?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(AdapterError::Auth);
        }
        if !status.is_success() {
            return Err(AdapterError::Network(format!("tmdb http_error: {status}")));
        }
        serde_json::from_str(&body).map_err(|e| AdapterError::Parse(e.to_string()))
    }

    /// Connectivity/credential check returning the image base URL (parity with
    /// the Go `Validate`). Cheap authenticated GET against `/3/configuration`.
    pub async fn validate(&self) -> Result<String, AdapterError> {
        let url = self.build_url("/3/configuration", &[])?;
        let json = self.get_json(&url).await?;
        let base = json
            .pointer("/images/base_url")
            .and_then(Value::as_str)
            .unwrap_or("");
        if base.is_empty() {
            return Err(AdapterError::Parse("unexpected_response".into()));
        }
        Ok(base.to_string())
    }

    /// Shared implementation behind [`Self::search_movie`] / [`Self::search_show`].
    async fn search(&self, path: &str, query: &str) -> Result<Vec<TmdbSearchResult>, AdapterError> {
        if query.trim().is_empty() {
            return Err(AdapterError::InvalidConfig("empty search query".into()));
        }
        let url = self.build_url(path, &[("query", query)])?;
        let json = self.get_json(&url).await?;
        let results = json
            .pointer("/results")
            .and_then(Value::as_array)
            .ok_or_else(|| AdapterError::Parse("missing results array".into()))?;
        Ok(results.iter().filter_map(parse_result).collect())
    }

    /// Search movies (`/3/search/movie`), parsed to `(id, title, year)`.
    pub async fn search_movie(&self, query: &str) -> Result<Vec<TmdbSearchResult>, AdapterError> {
        self.search("/3/search/movie", query).await
    }

    /// Search TV shows (`/3/search/tv`), parsed to `(id, name, year)`.
    pub async fn search_show(&self, query: &str) -> Result<Vec<TmdbSearchResult>, AdapterError> {
        self.search("/3/search/tv", query).await
    }
}

#[async_trait]
impl MetadataProvider for TmdbClient {
    fn name(&self) -> &str {
        "tmdb"
    }

    async fn test(&self) -> Result<(), AdapterError> {
        self.validate().await.map(|_| ())
    }
}

/// Maps one TMDB search hit to `(id, title, year)`, skipping malformed items.
fn parse_result(item: &Value) -> Option<TmdbSearchResult> {
    let id = match item.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return None,
    };
    let title = item
        .get("title")
        .or_else(|| item.get("name"))
        .and_then(Value::as_str)?
        .to_string();
    let date = item
        .get("release_date")
        .or_else(|| item.get("first_air_date"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let year = date.get(..4).and_then(|y| y.parse::<i32>().ok());
    Some(TmdbSearchResult { id, title, year })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(uri: String) -> TmdbClient {
        TmdbClient::new(
            &TmdbConfig {
                api_key: Some("k".into()),
            },
            OutboundPolicy::dev(),
        )
        .with_base_url(uri)
    }

    #[tokio::test]
    async fn validate_returns_image_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .and(query_param("api_key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"images": {"base_url": "http://image.tmdb.org/t/p/"}}),
            ))
            .mount(&server)
            .await;
        let base = client(server.uri()).validate().await.unwrap();
        assert_eq!(base, "http://image.tmdb.org/t/p/");
    }

    #[tokio::test]
    async fn validate_401_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(server.uri()).validate().await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth));
    }

    #[tokio::test]
    async fn empty_config_falls_back_to_demo_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .and(query_param("api_key", DEMO_API_KEY))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"images": {"base_url": "http://image.tmdb.org/t/p/"}}),
            ))
            .expect(2)
            .mount(&server)
            .await;
        for cfg in [
            TmdbConfig::default(),
            TmdbConfig {
                api_key: Some("   ".into()),
            },
        ] {
            TmdbClient::new(&cfg, OutboundPolicy::dev())
                .with_base_url(server.uri())
                .test()
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn missing_image_base_url_is_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let err = client(server.uri()).validate().await.unwrap_err();
        assert!(matches!(err, AdapterError::Parse(_)));
    }

    #[tokio::test]
    async fn search_movie_parses_id_title_year() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/search/movie"))
            .and(query_param("query", "Inception"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    {"id": 27205, "title": "Inception", "release_date": "2010-07-16"},
                    {"id": 999, "title": "No Date", "release_date": ""},
                    {"id": 998, "title": "Missing Date"}
                ]
            })))
            .mount(&server)
            .await;
        let hits = client(server.uri())
            .search_movie("Inception")
            .await
            .unwrap();
        assert_eq!(
            hits,
            vec![
                TmdbSearchResult {
                    id: "27205".into(),
                    title: "Inception".into(),
                    year: Some(2010)
                },
                TmdbSearchResult {
                    id: "999".into(),
                    title: "No Date".into(),
                    year: None
                },
                TmdbSearchResult {
                    id: "998".into(),
                    title: "Missing Date".into(),
                    year: None
                },
            ]
        );
    }

    #[tokio::test]
    async fn search_show_parses_name_and_first_air_date() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/search/tv"))
            .and(query_param("query", "GoT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"id": 1399, "name": "Game of Thrones", "first_air_date": "2011-04-17"}]
            })))
            .mount(&server)
            .await;
        let hits = client(server.uri()).search_show("GoT").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "1399");
        assert_eq!(hits[0].title, "Game of Thrones");
        assert_eq!(hits[0].year, Some(2011));
    }

    #[tokio::test]
    async fn empty_query_rejected_without_network_call() {
        let server = MockServer::start().await;
        let err = client(server.uri()).search_movie("  ").await.unwrap_err();
        assert!(matches!(err, AdapterError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn search_server_error_maps_to_network() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = client(server.uri()).search_show("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Network(_)));
    }
}
