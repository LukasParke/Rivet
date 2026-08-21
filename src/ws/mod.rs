//! WebSocket protocol types: envelopes and client messages, wire-compatible
//! with the original implementation.
pub mod hub;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub v: i32,
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts: String,
    pub payload: Value,
}

impl Envelope {
    pub fn new(event_type: &str, payload: Value) -> Self {
        Self {
            v: 1,
            event_type: event_type.to_string(),
            ts: crate::timeutil::fmt(Utc::now()),
            payload,
        }
    }

    pub fn marshal(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"v":1,"type":"error","payload":{"error":"marshal"}}"#.to_string()
        })
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientMessage {
    #[serde(rename = "type", default)]
    pub msg_type: String,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub subscribe: Vec<String>,
    #[serde(default, rename = "unsubscribe")]
    pub unsub: Vec<String>,
}

/// Maps an event type to the channels it belongs to.
///
/// Privileged classes deliberately do NOT ride `activity`: audit events are
/// admin-only, and download/import events carry filesystem paths visible only
/// to operators. The hub enforces role clearance per channel at delivery.
pub fn channels_for_type(event_type: &str) -> &'static [&'static str] {
    let prefix = event_type.split('.').next().unwrap_or(event_type);
    match prefix {
        "job" | "search" | "candidate" | "decision" => &["activity", "jobs"],
        "audit" => &["audit"],
        "health" => &["activity", "health"],
        "verify" | "import" => &["imports"],
        "download" => &["downloads"],
        "setup" | "test" => &["activity"],
        _ => &["activity"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_wire_format() {
        let e = Envelope::new("job.claimed", json!({"id": "x"}));
        let s = e.marshal();
        assert!(s.contains(r#""v":1"#));
        assert!(s.contains(r#""type":"job.claimed""#));
        assert!(s.contains(r#""payload":{"id":"x"}"#));
        assert!(s.contains(r#""ts":"#));
    }

    #[test]
    fn channel_mapping_matches_original() {
        assert_eq!(channels_for_type("job.claimed"), &["activity", "jobs"]);
        assert_eq!(channels_for_type("search.run.started"), &["activity", "jobs"]);
        assert_eq!(channels_for_type("audit.login"), &["audit"]);
        assert_eq!(channels_for_type("health.snapshot"), &["activity", "health"]);
        assert_eq!(channels_for_type("verify.completed"), &["imports"]);
        assert_eq!(channels_for_type("import.completed"), &["imports"]);
        assert_eq!(channels_for_type("download.progress"), &["downloads"]);
        assert_eq!(channels_for_type("setup.paths.saved"), &["activity"]);
    }

    #[test]
    fn client_message_parses_both_field_styles() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"type":"subscribe","channels":["jobs"]}"#).unwrap();
        assert_eq!(m.subscribe.len(), 0);
        assert_eq!(m.channels, vec!["jobs"]);
        let m: ClientMessage =
            serde_json::from_str(r#"{"type":"unsubscribe","unsubscribe":["audit"]}"#).unwrap();
        assert_eq!(m.unsub, vec!["audit"]);
    }
}
