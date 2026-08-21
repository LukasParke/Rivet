//! Time helpers.
//!
//! The original service stored timestamps as Go `RFC3339Nano` TEXT and compared
//! them lexicographically in SQL. RFC3339Nano trims trailing fractional zeros,
//! which breaks lexicographic ordering (`...00.5Z` sorts before `...00Z`).
//! We keep the TEXT schema for compatibility but always emit fixed 9-digit
//! fractional seconds, making string order equal chronological order.

use chrono::{DateTime, SecondsFormat, Utc};

/// Formats a timestamp as RFC3339 with fixed nanosecond precision (UTC).
pub fn fmt(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Current UTC timestamp, formatted for storage.
pub fn now() -> String {
    fmt(Utc::now())
}

/// Parses a stored timestamp; accepts both fixed-precision and legacy
/// variable-precision RFC3339 strings written by the Go implementation.
pub fn parse(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn lexicographic_order_matches_chronological() {
        let a = fmt(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
        let b = fmt(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap() + chrono::Duration::milliseconds(500));
        let c = fmt(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 1).unwrap());
        assert!(a < b && b < c, "a={a} b={b} c={c}");
    }

    #[test]
    fn parses_legacy_go_nano_format() {
        // Go RFC3339Nano trims trailing zeros: ".5" instead of ".500000000".
        let legacy = "2026-01-01T12:00:00.5Z";
        let dt = parse(legacy).expect("legacy format must parse");
        assert_eq!(fmt(dt), "2026-01-01T12:00:00.500000000Z");
    }
}
