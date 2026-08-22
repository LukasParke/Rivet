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
//! - `telegram`: POST `{"chat_id", "text"}` to
//!   `https://api.telegram.org/bot{botToken}/sendMessage`
//! - `slack`:    POST `{"text": ...}` to `config.webhookUrl`
//! - `pushover`: POST form-encoded `token/user/title/message` to
//!   `https://api.pushover.net/1/messages.json`
//! - `gotify`:   POST `{"title", "message"}` to `{url}/message?token={appToken}`
//!
//! Plain-text channels (all but `webhook`) render as `{event}: {summary}`,
//! where the summary is derived best-effort from the payload data — see
//! [`summarize`]. `telegram` and `pushover` accept an optional `baseUrl`
//! config override (tests / API proxies); it defaults to the official API
//! host.
//!
//! Every request goes through [`crate::security::ssrf::request_validated`]
//! under the state's outbound policy with a hard 5s timeout. Delivery
//! failures are logged and reported per sink; they NEVER propagate to the
//! caller or abort sibling sinks.

use std::time::Duration;

use serde_json::{json, Value};

use percent_encoding::utf8_percent_encode;

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
    let message = serde_json::to_string_pretty(&body).unwrap_or_else(|_| event_type.to_string());
    match sink.kind.as_str() {
        "webhook" => {
            let url = str_field(&config, "url")?;
            post_json(
                policy,
                url,
                &[("content-type", "application/json")],
                body.to_string(),
            )
            .await
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
        "telegram" => {
            let token = str_field(&config, "botToken")?;
            let chat_id = str_field(&config, "chatId")?;
            // Optional override (tests / API proxies); defaults to the real API.
            let base = config
                .get("baseUrl")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("https://api.telegram.org");
            // Telegram rejects texts over 4096 UTF-16 code units — truncate.
            let text: String = plain_text(event_type, body).chars().take(4096).collect();
            post_json(
                policy,
                &format!("{}/bot{token}/sendMessage", base.trim_end_matches('/')),
                &[("content-type", "application/json")],
                json!({ "chat_id": chat_id, "text": text }).to_string(),
            )
            .await
        }
        "slack" => {
            let url = str_field(&config, "webhookUrl")?;
            post_json(
                policy,
                url,
                &[("content-type", "application/json")],
                json!({ "text": plain_text(event_type, body) }).to_string(),
            )
            .await
        }
        "pushover" => {
            let user_key = str_field(&config, "userKey")?;
            let api_token = str_field(&config, "apiToken")?;
            // Optional override (tests); defaults to the real API.
            let base = config
                .get("baseUrl")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("https://api.pushover.net");
            // Pushover caps the message at 1024 chars — truncate.
            let message: String = plain_text(event_type, body).chars().take(1024).collect();
            let form = form_encode(&[
                ("token", api_token),
                ("user", user_key),
                ("title", event_type),
                ("message", &message),
            ]);
            post_json(
                policy,
                &format!("{}/1/messages.json", base.trim_end_matches('/')),
                &[("content-type", "application/x-www-form-urlencoded")],
                form,
            )
            .await
        }
        "gotify" => {
            let base = str_field(&config, "url")?;
            let app_token = str_field(&config, "appToken")?;
            let url = format!(
                "{}/message?token={}",
                base.trim_end_matches('/'),
                utf8_percent_encode(app_token, percent_encoding::CONTROLS)
            );
            post_json(
                policy,
                &url,
                &[("content-type", "application/json")],
                json!({
                    "title": event_type,
                    "message": plain_text(event_type, body),
                })
                .to_string(),
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "rivet".into()
    } else {
        cleaned
    }
}

/// `application/x-www-form-urlencoded` body from key/value pairs. Escapes
/// everything outside the unreserved set (`A-Za-z0-9 -._*`) plus reserved
/// delimiters; spaces become `%20`, which every form parser accepts.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, FORM_URLENCODED),
                utf8_percent_encode(v, FORM_URLENCODED)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Characters percent-escaped by [`form_encode`]: C0 controls, space, and
/// every delimiter reserved by `application/x-www-form-urlencoded`.
const FORM_URLENCODED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Renders the human-readable `{event}: {summary}` text used by every
/// channel except `webhook` (which gets the full JSON payload).
fn plain_text(event_type: &str, body: &Value) -> String {
    format!(
        "{event_type}: {}",
        summarize(body.get("data").unwrap_or(&Value::Null))
    )
}

/// Best-effort one-line summary derived from an event payload's data.
///
/// Prefers a human title (`title`/`name`), then appends identifying fields
/// (`status`, `acquisitionId`, `id`) when present. Falls back to the compact
/// JSON of the data itself so operators always see something useful.
fn summarize(data: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in ["title", "name"] {
        if let Some(s) = data.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                parts.push(s.to_string());
            }
            break;
        }
    }
    for key in ["status", "acquisitionId", "id"] {
        match data.get(key) {
            Some(Value::String(s)) if !s.is_empty() => parts.push(s.clone()),
            Some(v @ Value::Number(_)) => {
                if let Some(n) = v.as_i64() {
                    parts.push(n.to_string());
                } else if let Some(f) = v.as_f64() {
                    parts.push(f.to_string());
                }
            }
            _ => {}
        }
        if parts.len() >= 3 {
            break;
        }
    }
    if parts.is_empty() {
        serde_json::to_string(data).unwrap_or_else(|_| "…".into())
    } else {
        parts.join(" — ")
    }
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
    use wiremock::matchers::{
        body_partial_json, body_string_contains, header, method, path, query_param,
    };
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

        let outcomes =
            dispatch_with_results(&state, "acquisition.imported", json!({"title": "Example"}))
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
    async fn telegram_posts_chat_id_and_text_to_bot_endpoint() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123456:ABC-DEF/sendMessage"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "chat_id": "-100200300",
                "text": "acquisition.imported: Example",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "tg",
            "telegram",
            json!({
                "botToken": "123456:ABC-DEF",
                "chatId": "-100200300",
                "baseUrl": server.uri(),
            }),
            &["acquisition.imported"],
            true,
        )
        .await;

        let outcomes =
            dispatch_with_results(&state, "acquisition.imported", json!({"title": "Example"}))
                .await;
        assert!(outcomes[0].ok, "{outcomes:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn slack_posts_text_to_webhook_url() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/T000/B000/xyz"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "text": "acquisition.imported: Example",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "slack",
            "slack",
            json!({ "webhookUrl": format!("{}/services/T000/B000/xyz", server.uri()) }),
            &["acquisition.imported"],
            true,
        )
        .await;

        let outcomes =
            dispatch_with_results(&state, "acquisition.imported", json!({"title": "Example"}))
                .await;
        assert!(outcomes[0].ok, "{outcomes:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pushover_posts_form_encoded_credentials_and_message() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/1/messages.json"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("token=po-api-token"))
            .and(body_string_contains("user=po-user-key"))
            .and(body_string_contains("title=acquisition.imported"))
            .and(body_string_contains("message=acquisition.imported"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "pushover",
            "pushover",
            json!({
                "userKey": "po-user-key",
                "apiToken": "po-api-token",
                "baseUrl": server.uri(),
            }),
            &["acquisition.imported"],
            true,
        )
        .await;

        let outcomes =
            dispatch_with_results(&state, "acquisition.imported", json!({"title": "Example"}))
                .await;
        assert!(outcomes[0].ok, "{outcomes:?}");

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8(reqs[0].body.clone()).unwrap();
        // The summary is percent-encoded in the form body.
        assert!(
            body.contains("message=acquisition.imported%3A%20Example"),
            "body = {body:?}"
        );
    }

    #[tokio::test]
    async fn gotify_posts_title_and_message_with_token_query_param() {
        let state = test_state().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/message"))
            .and(query_param("token", "gotify-app-token"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "title": "acquisition.imported",
                "message": "acquisition.imported: Example",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        seed(
            &state,
            "gotify",
            "gotify",
            json!({
                "url": server.uri(),
                "appToken": "gotify-app-token",
            }),
            &["acquisition.imported"],
            true,
        )
        .await;

        let outcomes =
            dispatch_with_results(&state, "acquisition.imported", json!({"title": "Example"}))
                .await;
        assert!(outcomes[0].ok, "{outcomes:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn summarize_prefers_title_then_identifying_fields() {
        assert_eq!(
            summarize(&json!({"title": "Example", "status": "imported", "id": 7})),
            "Example — imported — 7"
        );
        assert_eq!(
            summarize(&json!({"name": "Show", "acquisitionId": "abc"})),
            "Show — abc"
        );
        // No known fields: falls back to compact JSON of the data.
        assert_eq!(summarize(&json!({"n": 1})), r#"{"n":1}"#);
        assert_eq!(summarize(&json!({})), "{}");
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
        assert_eq!(
            failing.error.as_deref(),
            Some("unexpected status 500 Internal Server Error")
        );
    }
}
