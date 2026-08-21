//! WebSocket endpoint (`GET /ws`) and info probe (`GET /ws/info`).
//!
//! Requires an authenticated user (the global session layer populates the
//! extension; the [`AuthenticatedUser`] extractor rejects anonymous
//! connections). The connection registers with the role-gated hub, receives a
//! `ws.connected` hello listing the default channels, drives
//! subscribe/unsubscribe via client messages (clearance enforced by the hub),
//! and is removed cleanly on disconnect.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::httpapi::middleware::auth::AuthenticatedUser;
use crate::state::{AppState, Role};
use crate::ws::hub::Hub;
use crate::ws::{ClientMessage, Envelope};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/info", get(ws_info))
}

/// GET /ws/info — cheap reachability probe for the socket route.
async fn ws_info() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    user: AuthenticatedUser,
    State(state): State<AppState>,
) -> Response {
    // The hub gates channel delivery by role; an unrecognized stored role is
    // treated as the most restrictive one rather than failing the upgrade —
    // matching the original, which only required *a* user.
    let role = user.role().unwrap_or(Role::Viewer);
    ws.on_upgrade(move |socket| handle_socket(socket, state.hub.clone(), role))
}

async fn handle_socket(socket: WebSocket, hub: std::sync::Arc<Hub>, role: Role) {
    let (conn_id, mut events) = hub.add(role);

    // Hello, delivered to this connection only.
    let hello = Envelope::new(
        "ws.connected",
        json!({ "ok": true, "channels": crate::ws::hub::DEFAULT_CHANNELS }),
    )
    .marshal();

    let mut socket = socket;
    if socket.send(Message::Text(hello.into())).await.is_err() {
        hub.remove(conn_id);
        return;
    }

    // Single-owner loop: hub envelopes and client frames interleave on one
    // task, so no split-sink handoff can strand either direction.
    loop {
        tokio::select! {
            envelope = events.recv() => match envelope {
                Some(text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // Hub dropped our sender: we were removed; hang up.
                None => break,
            },
            frame = socket.recv() => match frame {
                Some(Ok(Message::Text(text))) => apply_client_message(&hub, conn_id, &text),
                Some(Ok(Message::Ping(payload))) => {
                    let _ = socket.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }

    hub.remove(conn_id);
}

/// Wire-compatible port of the original `ApplyClientMessage`: a non-empty
/// `channels` array overrides `subscribe`/`unsubscribe`; unknown or malformed
/// messages are ignored silently.
fn apply_client_message(hub: &Hub, conn_id: u64, text: &str) {
    let Ok(m) = serde_json::from_str::<ClientMessage>(text) else {
        return;
    };
    match m.msg_type.as_str() {
        "subscribe" => {
            let channels = override_or(&m.channels, &m.subscribe);
            for ch in channels {
                let ch = ch.trim();
                if !ch.is_empty() {
                    hub.subscribe(conn_id, ch);
                }
            }
        }
        "unsubscribe" => {
            let channels = override_or(&m.channels, &m.unsub);
            for ch in channels {
                let ch = ch.trim();
                if !ch.is_empty() {
                    hub.unsubscribe(conn_id, ch);
                }
            }
        }
        _ => {}
    }
}

/// `channels` wins over the per-action list when non-empty (Go parity).
fn override_or<'a>(channels: &'a [String], action: &'a [String]) -> &'a [String] {
    if !channels.is_empty() {
        channels
    } else {
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_client_message_is_ignored() {
        let hub = Hub::new();
        let (id, _rx) = hub.add(Role::Admin);
        // Must not panic and must not change subscriptions.
        apply_client_message(&hub, id, "not json");
        assert_eq!(hub.subscribed_channels(id), ["activity", "health", "jobs"]);
    }

    #[test]
    fn subscribe_and_unsubscribe_follow_channels_override() {
        let hub = Hub::new();
        let (id, _rx) = hub.add(Role::Admin);

        apply_client_message(&hub, id, r#"{"type":"subscribe","subscribe":["audit"]}"#);
        assert!(hub.subscribed_channels(id).contains(&"audit".to_string()));

        // Non-empty `channels` overrides the per-action list.
        apply_client_message(
            &hub,
            id,
            r#"{"type":"unsubscribe","unsubscribe":["jobs"],"channels":["audit"]}"#,
        );
        let subs = hub.subscribed_channels(id);
        assert!(!subs.contains(&"audit".to_string()));
        assert!(subs.contains(&"jobs".to_string()));
    }

    #[test]
    fn empty_and_whitespace_channels_are_skipped() {
        let hub = Hub::new();
        let (id, _rx) = hub.add(Role::Viewer);
        apply_client_message(&hub, id, r#"{"type":"subscribe","subscribe":["  ",""]}"#);
        assert_eq!(hub.subscribed_channels(id), ["activity", "health", "jobs"]);
    }

    #[test]
    fn clearance_still_enforced_through_client_messages() {
        let hub = Hub::new();
        let (id, _rx) = hub.add(Role::Viewer);
        apply_client_message(&hub, id, r#"{"type":"subscribe","subscribe":["audit"]}"#);
        assert!(!hub.subscribed_channels(id).contains(&"audit".to_string()));
    }

    #[test]
    fn unknown_message_type_ignored() {
        let hub = Hub::new();
        let (id, _rx) = hub.add(Role::Operator);
        apply_client_message(&hub, id, r#"{"type":"ping","channels":["audit"]}"#);
        assert_eq!(hub.subscribed_channels(id), ["activity", "health", "jobs"]);
    }
}
