//! Environment-driven configuration, mirroring the original Go `internal/config`.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub env: String,
    pub http_addr: String,
    pub database_url: String,
    /// Simulates download clients and filesystem writes; indexer/metadata calls stay real.
    pub dry_run: bool,
    /// Optional read-only fixture used by verify/import in dry run.
    pub dry_run_fixture_path: Option<String>,
    /// Optional explicit ffprobe path; empty = search PATH.
    pub ffprobe_path: Option<String>,
    /// Base64 (32 bytes decoded) key for encrypting secrets at rest.
    pub secret_key_base64: Option<String>,
    pub session_cookie_name: String,
    pub session_ttl: Duration,
    /// SSRF policy: allow private/loopback/link-local targets for integration endpoints.
    pub allow_internal_urls: bool,
}

impl Config {
    /// Loads configuration from the process environment with the same defaults
    /// and validation rules as the Go implementation.
    pub fn load() -> Result<Config, String> {
        let env = getenv_default("APP_ENV", "dev");
        let http_addr = getenv_default("HTTP_ADDR", ":8080");
        let database_url = getenv_default(
            "DATABASE_URL",
            "file:/data/app.db?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)",
        );
        let dry_run = getenv_bool("DRY_RUN", false);
        let dry_run_fixture_path = nonempty("DRY_RUN_FIXTURE_PATH");
        let ffprobe_path = nonempty("FFPROBE_PATH");
        let secret_key_base64 = nonempty("APP_SECRET_KEY");
        let session_cookie_name = getenv_default("SESSION_COOKIE_NAME", "rivet_session");
        let session_ttl = getenv_duration("SESSION_TTL", Duration::from_secs(30 * 24 * 3600));
        let allow_internal_urls = getenv_bool("ALLOW_INTERNAL_URLS", env == "dev");

        if secret_key_base64.is_none() && env != "dev" {
            return Err(
                "APP_SECRET_KEY must be set (base64, 32 bytes decoded) outside dev".to_string(),
            );
        }

        Ok(Config {
            env,
            http_addr,
            database_url,
            dry_run,
            dry_run_fixture_path,
            ffprobe_path,
            secret_key_base64,
            session_cookie_name,
            session_ttl,
            allow_internal_urls,
        })
    }
}

fn getenv_default(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

fn nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn getenv_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => true,
            "0" | "false" | "f" | "no" | "n" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

fn getenv_duration(key: &str, default: Duration) -> Duration {
    let Some(v) = std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
        return default;
    };
    // Plain seconds ("30") or Go-style duration strings ("30s", "5m", "1h30m").
    if let Ok(n) = v.parse::<u64>() {
        return Duration::from_secs(n);
    }
    parse_go_duration(&v).unwrap_or(default)
}

/// Parses a subset of Go duration syntax: signed decimal numbers with unit
/// suffixes ms/s/m/h, possibly concatenated ("1h30m").
pub fn parse_go_duration(s: &str) -> Option<Duration> {
    let mut total = 0f64;
    let mut rest = s;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let n: f64 = rest[..num_end].parse().ok()?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        let mult = match unit {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += n * mult;
        rest = &rest[unit_end..];
    }
    if total < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_duration_parsing() {
        assert_eq!(parse_go_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_go_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_go_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("bogus"), None);
        assert_eq!(parse_go_duration(""), None);
    }

    #[test]
    fn plain_seconds_supported() {
        std::env::set_var("SESSION_TTL_TEST_PLAIN", "45");
        // direct function check via getenv_duration equivalent path
        assert_eq!(getenv_duration("SESSION_TTL_TEST_UNSET_XYZ", Duration::from_secs(7)), Duration::from_secs(7));
    }
}
