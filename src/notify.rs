//! Outbound notification dispatch.
//!
//! Loads every enabled sink subscribed to an event type (exact entry or
//! `"*"`), renders the canonical text payload
//! `{event, data, timestamp}` and delivers it per sink kind:
//!
//! - `webhook`:  POST the JSON payload to `config.url`
//! - `ntfy`:     POST `title\nmessage` to `config.url`/`config.topic`
//!   (`X-Title: <event>` header included)
//! - `discord`:  POST `{"content": ...}` to `config.webhookUrl`
//!
//! Every request goes through [`crate::security::ssrf::request_validated`]
//! under the state's outbound policy with a hard 5s timeout. Delivery
//! failures are logged and reported per sink; they NEVER propagate to the
//! caller or abort sibling sinks.

use std::time::Duration;

use serde_json::{json, Value};

use crate::security::ssrf;
use crate::state::AppState;
use crate::storage::notifications::{self, NotificationSink};
use crate::timeutil;

/// Per-send budget for every outbound notification request.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of delivering one event to one sink.
#[derive(Debug, Clone)]
pub struct SinkOutcome {
    pub sink_id: String,
    pub sink_name: String,
    pub ok: bool,
    /// Human-readable failure reason; `None` on success.
    pub error: Option<String>,
}

/// Fire-and-forget dispatch: sends `payload` to every enabled sink
/// subscribed to `event_type`, logging any delivery failures. Awaits each
/// send (5s cap) so callers that spawn it stay bounded.
pub async fn dispatch(state: &AppState, event_type: &str, payload: Value) {
    if let Err(err) = dispatch_inner(state, event_type, payload).await {
        // Only the sink-list DB read can fail here; deliveries never do.
        tracing::warn!(event = event_type, error = %err, "notification dispatch failed");
    }
}

async fn dispatch_inner(
    state: &AppState,
    event_type: &str,
    payload: Value,
) -> Result<(), sqlx::Error> {
    let sinks = notifications::list_enabled_matching(&state.db, event_type).await?;
    if sinks.is_empty() {
        return Ok(());
    }
    let body = render(event_type, payload);
    let outcomes = deliver_all(&state.policy, &sinks, event_type, &body).await;
    for o in outcomes {
        if let Some(err) = o.error {
            tracing::warn!(
                event = event_type,
                sink = %o.sink_name,
                error = %err,
                "notification delivery failed"
            );
        }
    }
    Ok(())
}

/// Dispatch that reports each sink's result — used by the test endpoint so
/// unreachable sinks are surfaced honestly instead of only logged.
pub async fn dispatch_with_results(
    state: &AppState,
    event_type: &str,
    payload: Value,
) -> Vec<SinkOutcome> {
    match notifications::list_enabled_matching(&state.db, event_type).await {
        Ok(sinks) => {
            let body = render(event_type, payload);
            deliver_all(&state.policy, &sinks, event_type, &body).await
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not load notification sinks");
            Vec::new()
        }
    }
}

fn render(event_type: &str, payload: Value) -> Value {
    json!({ "event": event_type, "data": payload, "timestamp": timeutil::now() })
}

async fn deliver_all(
    policy: &crate::security::OutboundPolicy,
    sinks: &[NotificationSink],
    event_type: &str,
    body: &Value,
) -> Vec<SinkOutcome> {
    let mut out = Vec::with_capacity(sinks.len());
    for sink in sinks {
        let res = send(policy, sink, event_type, body).await;
        out.push(match res {
            Ok(()) => SinkOutcome {
                sink_id: sink.id.clone(),
                sink_name: sink.name.clone(),
                ok: true,
                error: None,
            },
            Err(e) => SinkOutcome {
                sink_id: sink.id.clone(),
                sink_name: sink.name.clone(),
                ok: false,
                error: Some(e),
            },
        });
    }
    out
}

/// Sends the synthetic `test` event used by `POST /notifications/{id}/test`.
pub async fn probe_sink(state: &AppState, sink: &NotificationSink) -> Result<(), String> {
    let body = render(
        "test",
        json!({ "source": "rivet", "sink": sink.name, "manual": true }),
    );
    send(&state.policy, sink, "test", &body).await
}

/// Delivers one rendered payload to one sink. Errors carry a short,
/// operator-readable reason; nothing here panics or escapes as `sqlx` errors.
async fn send(
    policy: &crate::security::OutboundPolicy,
    sink: &NotificationSink,
    event_type: &str,
    body: &Value,
) -> Result<(), String> {
    let config = sink.config();
    let message = serde_json::to_string_pretty(&body)
        .unwrap_or_else(|_| event_type.to_string());
    match sink.kind.as_str() {
        "webhook" => {
            let url = str_field(&config, "url")?;
            post_json(policy, url, &[("content-type", "application/json")], body.to_string()).await
        }
        "ntfy" => {
            let base = str_field(&config, "url")?;
            let topic = match config.get("topic").and_then(Value::as_str) {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => sanitize_topic(event_type),
            };
            let url = format!("{}/{}", base.trim_end_matches('/'), topic);
            // ntfy simple mode: the body is the message; X-Title carries the title.
            post_json(
                policy,
                &url,
                &[("content-type", "text/plain"), ("X-Title", event_type)],
                format!("{event_type}\n{message}"),
            )
            .await
        }
        "discord" => {
            let url = str_field(&config, "webhookUrl")?;
            // Discord rejects content over 2000 chars outright — truncate.
            let content: String = format!("{event_type}\n{message}")
                .chars()
                .take(2000)
                .collect();
            post_json(
                policy,
                url,
                &[("content-type", "application/json")],
                json!({ "content": content }).to_string(),
            )
            .await
        }
        other => Err(format!("unsupported sink kind {other:?}")),
    }
}

fn str_field<'a>(config: &'a Value, key: &str) -> Result<&'a str, String> {
    config
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing config field {key:?}"))
}

/// ntfy topics are URL path segments restricted to `[A-Za-z0-9_-]`.
fn sanitize_topic(event_type: &str) -> String {
    let cleaned: String = event_type
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if cleaned.is_empty() { "rivet".into() } else { cleaned }
}

async fn post_json(
    policy: &crate::security::OutboundPolicy,
    url: &str,
    headers: &[(&str, &str)],
    body: String,
) -> Result<(), String> {
    let resp = ssrf::request_validated(
        reqwest::Method::POST,
        url,
        *policy,
        SEND_TIMEOUT,
        headers,
        Some(body),
    )
    .await
    .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("unexpected status {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::secrets::SecretBox;
    use crate::storage::migrations;
    use crate::ws::hub::Hub;
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn test_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("notify.db").display()
        ))
        .await
        .unwrap();
        migrations::apply(&pool).await.unwrap();
        // Keep the tempdir alive for the test's lifetime.
        std::mem::forget(dir);
        AppState {
            db: pool,
            secret_box: Arc::new(SecretBox::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: true,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: std::time::Duration::from_secs(3600),
                allow_internal_urls: true,
            }),
        }
    }

    async fn seed(
        state: &AppState,
        name: &str,
        kind: &str,
        config: Value,
        events: &[&str],
        enabled: bool,
    ) -> NotificationSink {
        let events: Vec<String> = events.iter().map(|s| s.to_string()).collect();
        notifications::create(&state.db, name, kind, &config, &events, enabled)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn webhook_sink_receives_rendered_payload() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "event": "acquisition.imported",
                "data": {"title": "Example"},
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "ops-hook",
            "webhook",
            json!({ "url": format!("{}/hook", server.uri()) }),
            &["acquisition.imported"],
            true,
        )
        .await;

        let outcomes = dispatch_with_results(
            &state,
            "acquisition.imported",
            json!({"title": "Example"}),
        )
        .await;
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert!(outcomes[0].ok, "{outcomes:?}");
        assert!(outcomes[0].error.is_none());

        // The rendered payload carries the canonical timestamp field.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body["timestamp"].as_str().is_some(), "{body}");
        assert_eq!(body["event"], "acquisition.imported");
    }

    #[tokio::test]
    async fn event_filter_excludes_non_subscribed_sinks() {
        let state = test_state().await;
        let matching = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&matching)
            .await;
        let excluded = MockServer::start().await;
        // Zero expected requests: any hit fails the test.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&excluded)
            .await;

        seed(
            &state,
            "wildcard",
            "webhook",
            json!({ "url": matching.uri() }),
            &["*"],
            true,
        )
        .await;
        seed(
            &state,
            "other-events-only",
            "webhook",
            json!({ "url": excluded.uri() }),
            &["series.deleted"],
            true,
        )
        .await;
        seed(
            &state,
            "disabled-but-matching",
            "webhook",
            json!({ "url": excluded.uri() }),
            &["acquisition.imported"],
            false,
        )
        .await;

        dispatch(&state, "acquisition.imported", json!({"n": 1})).await;

        assert_eq!(
            matching.received_requests().await.unwrap().len(),
            1,
            "wildcard sink fires"
        );
        assert!(
            excluded.received_requests().await.unwrap().is_empty(),
            "non-subscribed and disabled sinks never fire"
        );
    }

    #[tokio::test]
    async fn ntfy_posts_title_and_message_to_url_topic() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rivet-events"))
            .and(header("X-Title", "test"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "ntfy",
            "ntfy",
            json!({ "url": server.uri(), "topic": "rivet-events" }),
            &["*"],
            true,
        )
        .await;

        let outcomes = dispatch_with_results(&state, "test", json!({"k": "v"})).await;
        assert!(outcomes[0].ok, "{outcomes:?}");

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8(reqs[0].body.clone()).unwrap();
        assert!(body.starts_with("test\n"), "body = {body:?}");
        assert!(body.contains("\"k\""), "message JSON present: {body:?}");
    }

    #[tokio::test]
    async fn discord_posts_content_to_webhook_url() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("acquisition.imported"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "discord",
            "discord",
            json!({ "webhookUrl": format!("{}/api/webhooks/1/abc", server.uri()) }),
            &["acquisition.imported"],
            true,
        )
        .await;

        let outcomes = dispatch_with_results(&state, "acquisition.imported", json!({})).await;
        assert!(outcomes[0].ok, "{outcomes:?}");
    }

    #[tokio::test]
    async fn failures_are_reported_not_propagated() {
        let state = test_state().await;

        // Unreachable URL: outcome is a per-sink error, dispatch returns ().
        seed(
            &state,
            "dead",
            "webhook",
            json!({ "url": "http://127.0.0.1:9/hook" }),
            &["*"],
            true,
        )
        .await;
        // Missing config field: config error, no request attempted.
        seed(&state, "broken", "webhook", json!({}), &["*"], true).await;
        // Unsupported kind: never sent. Inserted directly — create() rejects it.
        sqlx::query(
            "INSERT INTO notifications (id, name, kind, config_json, events_json, enabled, created_at)
             VALUES ('pigeon', 'weird', 'carrier-pigeon', '{}', '[\"*\"]', 1, '2026-01-01T00:00:00.000000000Z')",
        )
        .execute(&state.db)
        .await
        .unwrap();

        let outcomes = dispatch_with_results(&state, "test", json!({})).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert!(!o.ok, "{o:?}");
            assert!(!o.error.as_deref().unwrap_or_default().is_empty(), "{o:?}");
        }

        // Fire-and-forget wrapper swallows everything.
        dispatch(&state, "test", json!({})).await;

        // HTTP 500 from the sink is an error, not a panic.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        seed(
            &state,
            "failing",
            "webhook",
            json!({ "url": server.uri() }),
            &["*"],
            true,
        )
        .await;
        let outcomes = dispatch_with_results(&state, "test", json!({})).await;
        let failing = outcomes.iter().find(|o| o.sink_name == "failing").unwrap();
        assert_eq!(failing.error.as_deref(), Some("unexpected status 500 Internal Server Error"));
    }
}
