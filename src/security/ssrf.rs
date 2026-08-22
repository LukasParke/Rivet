//! SSRF-safe outbound HTTP.
//!
//! Fixes the original design's TOCTOU: URLs were validated once (single DNS
//! resolution) but every actual request dialed a *fresh* resolution through an
//! unmanaged transport, so short-TTL DNS rebinding could flip an approved
//! hostname onto an internal address after validation. Redirects were followed
//! blindly by the default transport.
//!
//! Here, `validate_url` resolves once and *pins* the returned IPs into the
//! reqwest client's resolver: the connection can only be established to
//! addresses that passed the policy check. Redirects are followed manually,
//! re-validating every hop. All adapters route through this module.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::Url;

#[derive(Debug, Clone, Copy)]
pub struct OutboundPolicy {
    /// When true, private/loopback/link-local targets are permitted (dev mode).
    pub allow_internal: bool,
}

impl OutboundPolicy {
    pub const fn production() -> Self {
        Self {
            allow_internal: false,
        }
    }
    pub const fn dev() -> Self {
        Self {
            allow_internal: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("only http/https URLs are allowed")]
    BadScheme,
    #[error("URL has no host")]
    NoHost,
    #[error("target resolves to a disallowed address")]
    DisallowedAddress,
    #[error("DNS resolution failed")]
    DnsFailure,
    #[error("request failed")]
    Network,
}

/// A URL whose host has been resolved and policy-checked.
#[derive(Debug, Clone)]
pub struct ValidatedTarget {
    pub url: Url,
    pub ips: Vec<IpAddr>,
}

/// Returns true when `ip` is loopback, private, link-local, CGNAT, or otherwise
/// unsuitable as an arbitrary outbound target under the default policy.
pub fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let private = matches!(o[0], 0 | 10 | 127)
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64/10
                || (o[0] == 169 && o[1] == 254)                // link-local
                || (o[0] == 172 && (16..=31).contains(&o[1]))  // RFC1918
                || (o[0] == 192 && o[1] == 168)                // RFC1918
                || (o[0] == 192 && o[1] == 0)                  // 192.0.0.0/24 + TEST-NET-1
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking 198.18/15
                || (o[0] == 198 && o[1] == 51)                 // TEST-NET-2
                || (o[0] == 203 && o[1] == 0)                  // TEST-NET-3
                || o[0] >= 224; // multicast + reserved
            private
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // IPv4-mapped / translated forms: evaluate the embedded v4 address.
            if v6.is_loopback()
                || v6.is_unspecified()
                || seg[0] & 0xfe00 == 0xfc00 // unique local fc00::/7
                || seg[0] & 0xffc0 == 0xfe80 // link-local fe80::/10
                || seg[0] & 0xff00 == 0xff00 // multicast ff00::/8
                || seg[0] == 0x100           // discard-only 100::/64
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)
            // documentation
            {
                return true;
            }
            if seg[..5].iter().all(|&s| s == 0) && seg[5] == 0xffff {
                // ::ffff:a.b.c.d — mapped v4
                return is_internal_ip(IpAddr::V4(std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                )));
            }
            if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6].iter().all(|&s| s == 0) {
                // 64:ff9b::/96 — NAT64 translation of a v4 address
                return is_internal_ip(IpAddr::V4(std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                )));
            }
            false
        }
    }
}

fn check_ips(ips: &[IpAddr], policy: OutboundPolicy) -> Result<(), SsrfError> {
    if policy.allow_internal {
        return Ok(());
    }
    if ips.is_empty() {
        return Err(SsrfError::DnsFailure);
    }
    if ips.iter().any(|ip| is_internal_ip(*ip)) {
        return Err(SsrfError::DisallowedAddress);
    }
    Ok(())
}

async fn resolve_host(host: &str, port: u16) -> Result<Vec<IpAddr>, SsrfError> {
    // Literal IP: no DNS involved.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| SsrfError::DnsFailure)?;
    let mut seen = HashSet::new();
    Ok(addrs
        .map(|sa: SocketAddr| sa.ip())
        .filter(|ip| seen.insert(*ip))
        .collect())
}
/// Validates `raw` against the policy: scheme allowlist plus resolution of the
/// host with an internal-address check over EVERY returned IP.
pub async fn validate_url(raw: &str, policy: OutboundPolicy) -> Result<ValidatedTarget, SsrfError> {
    let url = Url::parse(raw).map_err(|_| SsrfError::BadScheme)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(SsrfError::BadScheme),
    }
    let host = url.host_str().ok_or(SsrfError::NoHost)?.to_string();
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let ips = resolve_host(&host, port).await?;
    check_ips(&ips, policy)?;
    Ok(ValidatedTarget { url, ips })
}

/// Resolver that only ever yields the pre-validated addresses for one host.
/// Any other hostname fails, so a connection cannot silently re-resolve.
#[derive(Clone)]
struct PinnedResolver {
    host: String,
    ips: Vec<IpAddr>,
    port: u16,
}

impl Resolve for PinnedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = self.host.clone();
        let ips = self.ips.clone();
        let port = self.port;
        Box::pin(async move {
            if name.as_str().eq_ignore_ascii_case(&host) {
                let addrs: Addrs =
                    Box::new(ips.into_iter().map(move |ip| SocketAddr::new(ip, port)));
                Ok(addrs)
            } else {
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "hostname not pinned by SSRF policy",
                ))
                    as Box<dyn std::error::Error + Send + Sync>)
            }
        })
    }
}
/// Public wrapper: a client pinned to the pre-validated target (no redirect
/// following). Use for POST/PUT probes that `get_validated` cannot express.
pub fn pinned_client(target: &ValidatedTarget, timeout: std::time::Duration) -> reqwest::Client {
    build_pinned_client(target, timeout)
}

pub(crate) fn build_pinned_client(target: &ValidatedTarget, timeout: Duration) -> reqwest::Client {
    let port = target
        .url
        .port_or_known_default()
        .unwrap_or(if target.url.scheme() == "https" {
            443
        } else {
            80
        });
    let resolver = Arc::new(PinnedResolver {
        host: target.url.host_str().unwrap_or_default().to_string(),
        ips: target.ips.clone(),
        port,
    });
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(resolver)
        .build()
        .expect("static client configuration")
}
/// Performs a single arbitrary-method request against a validated, pinned
/// target. No redirect following (3xx surfaces to the caller, which must
/// re-validate the Location itself). Use for authenticated POST/PUT probes
/// that [`get_validated`] cannot express.
pub async fn request_validated(
    method: reqwest::Method,
    raw_url: &str,
    policy: OutboundPolicy,
    timeout: Duration,
    headers: &[(&str, &str)],
    body: Option<String>,
) -> Result<reqwest::Response, SsrfError> {
    let target = validate_url(raw_url, policy).await?;
    let client = build_pinned_client(&target, timeout);
    let mut req = client.request(method, target.url.clone());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    req.send().await.map_err(|_| SsrfError::Network)
}

/// Performs a GET following at most `max_hops` redirects, validating and
/// pinning every hop against the policy. Returns the final response.
pub async fn get_validated(
    raw_url: &str,
    policy: OutboundPolicy,
    timeout: Duration,
    max_hops: usize,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    let mut current = raw_url.to_string();
    for hop in 0..=max_hops {
        let target = validate_url(&current, policy).await.map_err(Box::new)?;
        let client = build_pinned_client(&target, timeout);
        let mut req = client.get(target.url.clone());
        if hop > 0 {
            // Some trackers require the referer chain; keep it simple and safe.
            req = req.header("referer", current);
        }
        let resp = req.send().await.map_err(Box::new)?;
        if resp.status().is_redirection() {
            if hop == max_hops {
                return Err("too many redirects".into());
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without location")?;
            current = target.url.join(loc)?.to_string();
            continue;
        }
        return Ok(resp);
    }
    unreachable!("loop returns within max_hops+1 iterations")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_ipv4_classification() {
        assert!(is_internal_ip("127.0.0.1".parse().unwrap()));
        assert!(is_internal_ip("10.1.2.3".parse().unwrap()));
        assert!(is_internal_ip("172.16.0.1".parse().unwrap()));
        assert!(is_internal_ip("172.31.255.255".parse().unwrap()));
        assert!(!is_internal_ip("172.32.0.1".parse().unwrap()));
        assert!(is_internal_ip("192.168.1.1".parse().unwrap()));
        assert!(is_internal_ip("169.254.169.254".parse().unwrap()));
        assert!(is_internal_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_internal_ip("100.128.0.1".parse().unwrap()));
        assert!(is_internal_ip("0.0.0.0".parse().unwrap()));
        assert!(is_internal_ip("224.0.0.1".parse().unwrap()));
        assert!(!is_internal_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn internal_ipv6_classification() {
        assert!(is_internal_ip("::1".parse().unwrap()));
        assert!(is_internal_ip("fe80::1".parse().unwrap()));
        assert!(is_internal_ip("fd00::1".parse().unwrap()));
        assert!(is_internal_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_internal_ip("64:ff9b::7f00:1".parse().unwrap()));
        assert!(!is_internal_ip("2606:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_disallowed_literal() {
        let err = validate_url("http://127.0.0.1:8081/x", OutboundPolicy::production())
            .await
            .unwrap_err();
        assert!(matches!(err, SsrfError::DisallowedAddress));
    }

    #[tokio::test]
    async fn rejects_bad_scheme() {
        assert!(matches!(
            validate_url("file:///etc/passwd", OutboundPolicy::production()).await,
            Err(SsrfError::BadScheme)
        ));
        assert!(matches!(
            validate_url("ftp://example.com", OutboundPolicy::production()).await,
            Err(SsrfError::BadScheme)
        ));
    }

    #[tokio::test]
    async fn allows_internal_when_policy_says_so() {
        let t = validate_url("http://127.0.0.1:1/x", OutboundPolicy::dev())
            .await
            .unwrap();
        assert_eq!(t.ips, vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
    }
}
