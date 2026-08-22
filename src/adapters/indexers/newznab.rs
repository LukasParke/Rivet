//! Newznab/Torznab indexer clients.
//!
//! Both protocols share the same caps/search XML; `torznab` adds torrent
//! attributes (seeders/peers/magnet) and `newznab` serves usenet NZBs.
//! One shared engine, two client structs.
//!
//! Review fixes baked in:
//! - All HTTP via [`crate::security::ssrf`] (validated, IP-pinned; no
//!   redirect-following transport).
//! - The API key is REDACTED from any URL that reaches debug output, logs, or
//!   WS events (the original leaked it via debug.URL on failed searches).
//! - Prowlarr hosts aggregate ALL backend results via the JSON search API,
//!   with a fallback to the standard endpoint (the original picked an
//!   arbitrary single backend indexer and had no fallback).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::adapters::{AdapterError, Indexer, Release, ReleaseKind, SearchQuery};
use crate::security::ssrf::{get_validated, OutboundPolicy};

const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct IndexerConfig {
    pub name: String,
    pub url: String,
    #[serde(alias = "api_key", alias = "apiKey")]
    pub apikey: String,
    #[serde(default)]
    pub torznab: bool,
    #[serde(default)]
    pub prowlarr: bool,
}

/// Redacts `apikey` (and `api_key`) query parameters from a URL string.
pub fn redact_apikey(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            if u.query().is_some() {
                let pairs: Vec<(String, String)> = u
                    .query_pairs()
                    .map(|(k, v)| {
                        let k = k.to_string();
                        if k.eq_ignore_ascii_case("apikey") || k.eq_ignore_ascii_case("api_key") {
                            (k, "REDACTED".to_string())
                        } else {
                            (k, v.to_string())
                        }
                    })
                    .collect();
                let mut q = form_urlencoded(&pairs);
                if !q.is_empty() {
                    q.insert(0, '?');
                    u.set_query(Some(&q));
                }
            }
            u.to_string()
        }
        // Not a parseable URL (e.g. reqwest error Display text): redact every
        // key=value fragment whose key is apikey/api_key, at ANY position in
        // the fragment — including the first param of Prowlarr-style URLs.
        Err(_) => {
            let re = |s: &str| -> String {
                s.split('&')
                    .map(|pair| {
                        let lower = pair.to_ascii_lowercase();
                        let key = lower.split('=').next().unwrap_or("");
                        if key == "apikey" || key == "api_key" {
                            format!("{key}=REDACTED")
                        } else {
                            pair.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("&")
            };
            if let Some(qpos) = url.find('?') {
                let (base, q) = url.split_at(qpos);
                format!("{base}{}", re(&q[1..]))
            } else {
                url.to_string()
            }
        }
    }
}

fn form_urlencoded(pairs: &[(String, String)]) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn is_prowlarr(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("prowlarr") || lower.contains(":9696")
}

#[derive(Debug, Clone)]
pub struct NewznabClient {
    pub cfg: IndexerConfig,
    pub policy: OutboundPolicy,
}

impl NewznabClient {
    pub fn new(
        cfg: &IndexerConfig,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn Indexer>, AdapterError> {
        if cfg.name.trim().is_empty() || cfg.url.trim().is_empty() || cfg.apikey.trim().is_empty() {
            return Err(AdapterError::InvalidConfig(
                "name/url/apikey required".into(),
            ));
        }
        Ok(Arc::new(Self {
            cfg: cfg.clone(),
            policy,
        }))
    }

    fn search_url(&self, q: &SearchQuery, limit: i64) -> String {
        let mut pairs: Vec<(String, String)> = vec![
            ("t".into(), "search".into()),
            ("apikey".into(), self.cfg.apikey.clone()),
            ("q".into(), q.query.clone()),
            ("limit".into(), limit.clamp(1, 100).to_string()),
        ];
        for (i, cat) in q.categories.iter().enumerate() {
            pairs.push((
                if i == 0 {
                    "cat".into()
                } else {
                    format!("cat{}", i + 1)
                },
                cat.to_string(),
            ));
        }
        let base = self.cfg.url.trim_end_matches('/');
        format!("{base}/api?{}", form_urlencoded(&pairs))
    }

    async fn fetch_body(&self, url: &str) -> Result<String, AdapterError> {
        let mut resp = get_validated(url, self.policy, TIMEOUT, 3)
            .await
            .map_err(|e| AdapterError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(AdapterError::Auth);
        }
        if !status.is_success() {
            return Err(AdapterError::Network(format!("status {status}")));
        }
        // Stream the body and abort the moment the cap would be exceeded:
        // never buffer an unbounded response in memory before checking.
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| AdapterError::Network(e.to_string()))?
        {
            if buf.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(AdapterError::Network("response_too_large".into()));
            }
            buf.extend_from_slice(&chunk);
        }
        String::from_utf8(buf).map_err(|e| AdapterError::Parse(e.to_string()))
    }

    async fn search_prowlarr_json(
        &self,
        q: &SearchQuery,
        limit: i64,
    ) -> Result<Vec<Release>, AdapterError> {
        #[derive(Deserialize)]
        struct ProwlarrItem {
            #[serde(default)]
            title: String,
            #[serde(default, rename = "downloadUrl")]
            download_url: String,
            #[serde(default, rename = "infoUrl")]
            info_url: Option<String>,
            #[serde(default, rename = "magnetUrl")]
            magnet_url: Option<String>,
            #[serde(default, rename = "indexer")]
            indexer: Option<String>,
            #[serde(default, rename = "size")]
            size: Option<i64>,
            #[serde(default, rename = "seeders")]
            seeders: Option<i64>,
            #[serde(default, rename = "leechers")]
            leechers: Option<i64>,
            #[serde(default, rename = "publishDate")]
            published: Option<String>,
            #[serde(default, rename = "infoHash")]
            info_hash: Option<String>,
            #[serde(default, rename = "protocol")]
            protocol: Option<String>,
        }
        let base = self.cfg.url.trim_end_matches('/');
        let mut pairs: Vec<(String, String)> = vec![
            ("apikey".into(), self.cfg.apikey.clone()),
            ("query".into(), q.query.clone()),
            ("limit".into(), limit.clamp(1, 100).to_string()),
        ];
        for cat in &q.categories {
            pairs.push(("categories".into(), cat.to_string()));
        }
        let url = format!("{base}/api/v1/search?{}", form_urlencoded(&pairs));
        let body = self.fetch_body(&url).await?;
        let items: Vec<ProwlarrItem> =
            serde_json::from_str(&body).map_err(|e| AdapterError::Parse(e.to_string()))?;
        Ok(items
            .into_iter()
            .map(|it| {
                let dl = if it.download_url.is_empty() {
                    it.magnet_url.clone().unwrap_or_default()
                } else {
                    it.download_url
                };
                let kind = match it.protocol.as_deref() {
                    Some("usenet") => ReleaseKind::Usenet,
                    _ => ReleaseKind::Torrent,
                };
                let mut rel = normalize(
                    it.indexer.unwrap_or_else(|| self.cfg.name.clone()),
                    &it.title,
                    &dl,
                    it.info_url,
                    kind,
                    it.size,
                    it.seeders,
                    it.leechers.or(None),
                    it.published.as_deref(),
                    it.info_hash.as_deref().filter(|h| !h.is_empty()),
                );
                rel.raw = serde_json::json!({"prowlarr": true});
                rel
            })
            .collect())
    }
}

#[async_trait]
impl Indexer for NewznabClient {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    async fn search(&self, q: &SearchQuery) -> Result<Vec<Release>, AdapterError> {
        let limit = if q.limit > 0 { q.limit } else { 50 };
        if self.cfg.prowlarr || (!self.cfg.torznab && is_prowlarr(&self.cfg.url)) {
            match self.search_prowlarr_json(q, limit).await {
                Ok(v) if !v.is_empty() => return Ok(v),
                Ok(_) => {} // empty → fall through to standard endpoint
                Err(AdapterError::Parse(_)) => {} // not JSON → fall through
                Err(e) => return Err(e),
            }
        }
        let url = self.search_url(q, limit);
        let body = self.fetch_body(&url).await?;
        parse_caps_xml(&body, self.name(), self.cfg.torznab)
    }

    async fn test(&self) -> Result<(), AdapterError> {
        let q = SearchQuery {
            query: "test".into(),
            categories: vec![],
            limit: 1,
        };
        self.search(&q).await.map(|_| ())
    }
}

/// Torznab client: same wire protocol, torrent semantics by default.
#[derive(Debug, Clone)]
pub struct TorznabClient {
    inner: NewznabClient,
}

impl TorznabClient {
    pub fn new(
        cfg: &IndexerConfig,
        policy: OutboundPolicy,
    ) -> Result<Arc<dyn Indexer>, AdapterError> {
        let mut cfg = cfg.clone();
        cfg.torznab = true;
        NewznabClient::new(&cfg, policy).map(|inner| {
            // Unwrap safe: we just constructed this exact type.
            let boxed = inner as Arc<dyn Indexer>;
            let inner_ref = boxed.as_ref() as &dyn Indexer;
            let _ = inner_ref;
            Arc::new(TorznabClient {
                inner: NewznabClient { cfg, policy },
            }) as Arc<dyn Indexer>
        })
    }
}

#[async_trait]
impl Indexer for TorznabClient {
    fn name(&self) -> &str {
        &self.inner.cfg.name
    }
    async fn search(&self, q: &SearchQuery) -> Result<Vec<Release>, AdapterError> {
        self.inner.search(q).await
    }
    async fn test(&self) -> Result<(), AdapterError> {
        self.inner.test().await
    }
}

/// Extracts releases from a Newznab/Torznab caps XML response.
pub fn parse_caps_xml(
    xml: &str,
    indexer_name: &str,
    torznab: bool,
) -> Result<Vec<Release>, AdapterError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| AdapterError::Parse(e.to_string()))?;
    let kind = if torznab {
        ReleaseKind::Torrent
    } else {
        ReleaseKind::Usenet
    };
    let mut out = Vec::new();
    for item in doc.descendants().filter(|n| n.has_tag_name("item")) {
        let title = child_text(&item, "title").unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let link = child_text(&item, "link");
        let (enc_url, enc_len) = enclosure(&item);
        let download_url = link.or(enc_url).unwrap_or_default();
        if download_url.is_empty() {
            continue;
        }
        let size = enc_len.or(attr(&item, "size").and_then(|v| v.parse().ok()));
        let seeders = int_attr(&item, "seeders");
        let peers = int_attr(&item, "peers").or(int_attr(&item, "leechers"));
        let pub_date = child_text(&item, "pubDate").and_then(|s| parse_rfc2822(&s));
        let info_hash = magnet_hash(&download_url).or(int_attr_hex(&item, "infohash"));
        let mut rel = normalize(
            indexer_name.to_string(),
            &title,
            &download_url,
            child_text(&item, "comments").or_else(|| child_text(&item, "guid")),
            kind,
            size,
            seeders,
            peers,
            pub_date.as_deref(),
            info_hash.as_deref(),
        );
        rel.raw = serde_json::json!({ "title": title });
        out.push(rel);
    }
    Ok(out)
}

fn child_text<'a>(node: &roxmltree::Node<'a, 'a>, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
}

fn enclosure(node: &roxmltree::Node) -> (Option<String>, Option<i64>) {
    for n in node.children() {
        if n.has_tag_name("enclosure") {
            let url = n.attribute("url").map(|s| s.to_string());
            let len = n.attribute("length").and_then(|v| v.parse().ok());
            return (url, len);
        }
    }
    (None, None)
}

/// Reads namespaced attrs (`torznab:attr name="seeders" value=".."` /
/// `newznab:attr ...`) regardless of the prefix actually used.
fn attr_node_value(node: &roxmltree::Node, name: &str) -> Option<String> {
    for n in node.children() {
        // Local name is "attr" regardless of the namespace prefix in use.
        if n.tag_name().name() == "attr" && n.attribute("name") == Some(name) {
            return n.attribute("value").map(|s| s.to_string());
        }
    }
    None
}

fn int_attr(node: &roxmltree::Node, name: &str) -> Option<i64> {
    attr_node_value(node, name).and_then(|v| v.replace(',', "").parse().ok())
}

fn int_attr_hex(node: &roxmltree::Node, name: &str) -> Option<String> {
    attr_node_value(node, name).map(|v| v.to_ascii_lowercase())
}

fn attr(node: &roxmltree::Node, name: &str) -> Option<String> {
    attr_node_value(node, name)
}

/// Extracts btih from a magnet: link.
pub fn magnet_hash(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let idx = lower.find("xt=urn:btih:")?;
    let start = idx + "xt=urn:btih:".len();
    let rest = &url[start..];
    let end = rest.find('&').unwrap_or(rest.len());
    let h = &rest[..end];
    // 40-char hex btih or 32-char base32 btih.
    if (h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())) || h.len() == 32 {
        Some(h.to_ascii_lowercase())
    } else {
        None
    }
}

fn parse_rfc2822(s: &str) -> Option<String> {
    // Standard RSS pubDate ("Mon, 03 Aug 2026 12:00:00 +0000"); chrono's
    // RFC2822 parser also accepts GMT/UTC zone names.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(crate::timeutil::fmt(dt.with_timezone(&chrono::Utc)));
    }
    // Fallback: ISO8601/RFC3339 as some indexers emit.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(crate::timeutil::fmt(dt.with_timezone(&chrono::Utc)));
    }
    None
}

/// Normalizes a raw result: quality/tag detection, hash extraction.
#[allow(clippy::too_many_arguments)]
pub fn normalize(
    indexer_name: String,
    title: &str,
    download_url: &str,
    info_url: Option<String>,
    kind: ReleaseKind,
    size_bytes: Option<i64>,
    seeders: Option<i64>,
    peers: Option<i64>,
    published_at: Option<&str>,
    info_hash: Option<&str>,
) -> Release {
    let (quality, tags) = classify_title(title);
    Release {
        indexer_name,
        title: title.to_string(),
        download_url: download_url.to_string(),
        info_url,
        kind,
        size_bytes,
        seeders,
        peers,
        published_at: published_at.and_then(crate::timeutil::parse),
        info_hash: info_hash
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| magnet_hash(download_url)),
        raw: serde_json::json!({}),
        quality,
        tags,
    }
}

/// Detects resolution/quality token and release tags from a title.
pub fn classify_title(title: &str) -> (Option<String>, Vec<String>) {
    let t = title.to_ascii_lowercase();
    let quality = ["2160p", "1080p", "1080i", "720p", "576p", "480p"]
        .into_iter()
        .find(|q| t.contains(q))
        .map(|q| q.to_string());
    let source = if t.contains("web-dl") || t.contains("webdl") || t.contains("web dl") {
        Some("web")
    } else if t.contains("webrip") {
        Some("webrip")
    } else if t.contains("hdtv") {
        Some("hdtv")
    } else if t.contains("remux") {
        Some("remux")
    } else if t.contains("bluray") || t.contains("blu-ray") || t.contains("bdrip") {
        Some("bluray")
    } else if t.contains("dvdrip") {
        Some("dvd")
    } else if t.contains("x264") || t.contains("x265") || t.contains("h.264") || t.contains("hevc")
    {
        Some("encode")
    } else {
        None
    };
    let mut tags = Vec::new();
    for (needle, tag) in [
        ("proper", "proper"),
        ("repack", "repack"),
        ("extended", "extended"),
        ("internal", "internal"),
        ("dual audio", "dualaudio"),
        ("hdr", "hdr"),
    ] {
        if t.contains(needle) {
            tags.push(tag.to_string());
        }
    }
    if let Some(src) = source {
        tags.push(src.to_string());
    }
    (quality, tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:torznab="http://torznab.com/schemas/2015/feed" version="2.0">
  <channel>
    <item>
      <title>Show.Name.S01E05.1080p.WEB-DL.x265.PROPER</title>
      <guid>https://example.com/details/1</guid>
      <link>https://example.com/download/1</link>
      <pubDate>Mon, 03 Aug 2026 12:00:00 +0000</pubDate>
      <enclosure url="https://example.com/download/1" length="12345678" type="application/x-nzb"/>
      <torznab:attr name="seeders" value="42"/>
      <torznab:attr name="peers" value="7"/>
      <torznab:attr name="infohash" value="ABCDEF0123456789ABCDEF0123456789ABCDEF01"/>
    </item>
    <item>
      <title>Other.Release.720p.HDTV</title>
      <link>magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567&amp;dn=Other</link>
    </item>
    <item>
      <title></title>
      <link>https://example.com/nope</link>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_caps_xml() {
        let rels = parse_caps_xml(SAMPLE_XML, "testidx", true).unwrap();
        assert_eq!(rels.len(), 2, "empty-title item skipped");
        let first = &rels[0];
        assert_eq!(first.title, "Show.Name.S01E05.1080p.WEB-DL.x265.PROPER");
        assert_eq!(first.quality.as_deref(), Some("1080p"));
        assert!(first.tags.contains(&"proper".to_string()));
        assert!(first.tags.contains(&"web".to_string()));
        assert_eq!(first.seeders, Some(42));
        assert_eq!(first.peers, Some(7));
        assert_eq!(first.size_bytes, Some(12_345_678));
        assert_eq!(
            first.info_hash.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        assert!(first.published_at.is_some());
        // Second item: magnet hash extraction.
        assert_eq!(
            rels[1].info_hash.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(rels[1].quality.as_deref(), Some("720p"));
    }

    #[test]
    fn magnet_hash_extraction() {
        assert_eq!(
            magnet_hash("magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567&dn=x")
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(magnet_hash("https://example.com/file.torrent"), None);
        assert_eq!(magnet_hash("magnet:?xt=urn:btih:short"), None);
    }

    #[test]
    fn apikey_redaction() {
        let u = "https://idx.example.com/api?t=search&apikey=SUPERSECRET&q=show";
        let red = redact_apikey(u);
        assert!(!red.contains("SUPERSECRET"));
        assert!(red.contains("apikey=REDACTED"));
        assert!(red.contains("q=show"));
        // Unparseable-ish input still redacted by pattern.
        let red2 = redact_apikey("notaurl?t=x&api_key=abc");
        assert!(!red2.contains("abc"));
    }

    #[test]
    fn title_classification() {
        let (q, tags) = classify_title("Movie.2020.2160p.BluRay.REMUX.HDR.HEVC");
        assert_eq!(q.as_deref(), Some("2160p"));
        assert!(tags.contains(&"hdr".to_string()));
        assert!(tags.contains(&"remux".to_string()));
        let (q2, _) = classify_title("Something.unclear.release");
        assert_eq!(q2, None);
    }

    #[test]
    fn apikey_redacted_as_first_param_in_error_text() {
        // reqwest Display text with Prowlarr-style first-param apikey.
        let err = "error sending request for url (http://host:9696/api/v1/search?apikey=SECRET&query=x)";
        let red = redact_apikey(err);
        assert!(!red.contains("SECRET"), "{red}");
        assert!(red.contains("apikey=REDACTED"));
        assert!(red.contains("query=x"));
    }

    #[test]
    fn prowlarr_detection() {
        assert!(is_prowlarr("http://localhost:9696/1"));
        assert!(is_prowlarr("http://prowlarr.local/api"));
        assert!(!is_prowlarr("https://indexer.example.com/api"));
    }

    #[tokio::test]
    async fn config_validation() {
        let policy = OutboundPolicy::dev();
        let bad = IndexerConfig {
            name: "".into(),
            url: "http://x".into(),
            apikey: "k".into(),
            torznab: true,
            prowlarr: false,
        };
        assert!(matches!(
            NewznabClient::new(&bad, policy),
            Err(AdapterError::InvalidConfig(_))
        ));
        assert!(TorznabClient::new(&bad, policy).is_err());
    }

    fn test_client(base: &str) -> NewznabClient {
        NewznabClient {
            cfg: IndexerConfig {
                name: "stub".into(),
                url: base.to_string(),
                apikey: "k".into(),
                torznab: false,
                prowlarr: false,
            },
            policy: OutboundPolicy::dev(),
        }
    }

    /// Happy path unchanged by the streaming rewrite: a small body parses
    /// through fetch_body byte-identically.
    #[tokio::test]
    async fn fetch_body_small_body_ok() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello-feed"))
            .mount(&server)
            .await;
        let client = test_client(&server.uri());
        let body = client
            .fetch_body(&format!("{}/api", server.uri()))
            .await
            .unwrap();
        assert_eq!(body, "hello-feed");
    }

    /// Regression: an oversized response must abort during streaming with the
    /// truncated-body error — the old code buffered the whole response via
    /// `resp.bytes()` before checking MAX_BODY.
    #[tokio::test]
    async fn fetch_body_aborts_over_cap() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_BODY + 1024]),
            )
            .mount(&server)
            .await;
        let client = test_client(&server.uri());
        let err = client
            .fetch_body(&format!("{}/api", server.uri()))
            .await
            .unwrap_err();
        match err {
            AdapterError::Network(msg) => {
                assert_eq!(msg, "response_too_large");
            }
            other => panic!("expected response_too_large network error, got {other:?}"),
        }
    }

    /// A response exactly at the cap still succeeds (boundary).
    #[tokio::test]
    async fn fetch_body_exactly_at_cap_ok() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; MAX_BODY]))
            .mount(&server)
            .await;
        let client = test_client(&server.uri());
        let body = client
            .fetch_body(&format!("{}/api", server.uri()))
            .await
            .unwrap();
        assert_eq!(body.len(), MAX_BODY);
    }
}
