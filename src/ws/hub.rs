//! Role-gated WebSocket hub.
//!
//! Fixes the original authorization gap: every authenticated connection was
//! auto-subscribed to the `audit` channel and the hub fanned out all events to
//! all connections regardless of role, leaking the admin audit trail and
//! filesystem paths to viewers. Here each connection carries the user's role;
//! channel clearance is enforced at delivery time:
//! - `activity`, `jobs`, `health` — any authenticated user
//! - `downloads`, `imports`      — operator and above (they expose paths)
//! - `setup`                     — operator and above (indexer URLs, probes)
//! - `audit`                     — admin only
//!
//! Subscription semantics are explicit: unsubscribing from a channel removes
//! it (the original flipped an empty subscription set into "receive
//! everything").

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use super::channels_for_type;
use crate::state::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChannelClearance {
    Any = 0,
    Operator = 1,
    Admin = 2,
}

impl ChannelClearance {
    pub fn for_channel(channel: &str) -> Self {
        match channel {
            "audit" => Self::Admin,
            "downloads" | "imports" | "setup" => Self::Operator,
            _ => Self::Any,
        }
    }
}

/// Bitflag per canonical channel; publish eligibility is a single AND.
const BIT_ACTIVITY: u64 = 1 << 0;
const BIT_JOBS: u64 = 1 << 1;
const BIT_HEALTH: u64 = 1 << 2;
const BIT_DOWNLOADS: u64 = 1 << 3;
const BIT_IMPORTS: u64 = 1 << 4;
const BIT_AUDIT: u64 = 1 << 5;
const BIT_SETUP: u64 = 1 << 6;

fn channel_bit(channel: &str) -> u64 {
    match channel {
        "activity" => BIT_ACTIVITY,
        "jobs" => BIT_JOBS,
        "health" => BIT_HEALTH,
        "downloads" => BIT_DOWNLOADS,
        "imports" => BIT_IMPORTS,
        "audit" => BIT_AUDIT,
        "setup" => BIT_SETUP,
        // Unknown channels never match a published event (channels_for_type
        // only emits canonical names); they are still tracked by name for
        // subscribed_channels().
        _ => 0,
    }
}

struct Connection {
    id: u64,
    /// Cached union of channel_bit() over `subscriptions`. A set bit implies
    /// the role clearance was validated when the subscription was added (roles
    /// are immutable after add), so publish needs no per-event re-check.
    subs_mask: u64,
    role: Role,
    /// Explicitly subscribed channels. Starts with the default set; empty set
    /// means receive nothing (explicit opt-out is honored).
    subscriptions: HashSet<String>,
    tx: mpsc::Sender<String>,
}

impl Connection {
    fn new(id: u64, role: Role, subscriptions: HashSet<String>, tx: mpsc::Sender<String>) -> Self {
        let subs_mask = subscriptions.iter().map(|s| channel_bit(s)).sum();
        Self {
            id,
            subs_mask,
            role,
            subscriptions,
            tx,
        }
    }
}

#[derive(Default)]
pub struct Hub {
    next_id: AtomicU64,
    conns: std::sync::Mutex<Vec<Connection>>,
}

pub const DEFAULT_CHANNELS: &[&str] = &["activity", "jobs", "health"];

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a connection with its role and returns (id, event receiver).
    pub fn add(&self, role: Role) -> (u64, mpsc::Receiver<String>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(128);
        let subscriptions: HashSet<String> =
            DEFAULT_CHANNELS.iter().map(|s| s.to_string()).collect();
        self.conns
            .lock()
            .expect("hub mutex")
            .push(Connection::new(id, role, subscriptions, tx));
        (id, rx)
    }

    pub fn remove(&self, id: u64) {
        self.conns.lock().expect("hub mutex").retain(|c| c.id != id);
    }

    pub fn subscribe(&self, id: u64, channel: &str) -> bool {
        // A connection may only subscribe to channels its role clears.
        let mut conns = self.conns.lock().expect("hub mutex");
        if let Some(c) = conns.iter_mut().find(|c| c.id == id) {
            if ChannelClearance::for_channel(channel) <= clearance_of(c.role) {
                c.subscriptions.insert(channel.to_string());
                c.subs_mask |= channel_bit(channel);
                return true;
            }
        }
        false
    }

    pub fn unsubscribe(&self, id: u64, channel: &str) {
        if let Some(c) = self
            .conns
            .lock()
            .expect("hub mutex")
            .iter_mut()
            .find(|c| c.id == id)
        {
            c.subscriptions.remove(channel);
            c.subs_mask = c.subscriptions.iter().map(|s| channel_bit(s)).sum();
        }
    }

    pub fn subscribed_channels(&self, id: u64) -> Vec<String> {
        self.conns
            .lock()
            .expect("hub mutex")
            .iter()
            .find(|c| c.id == id)
            .map(|c| {
                let mut v: Vec<String> = c.subscriptions.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// Publishes an event to every connection whose role clears at least one
    /// of the event's channels AND which is subscribed to one of them.
    pub fn publish(&self, event_type: &str, payload: serde_json::Value) {
        let envelope = super::Envelope::new(event_type, payload).marshal();
        let mut ev_mask = 0u64;
        for ch in channels_for_type(event_type) {
            ev_mask |= channel_bit(ch);
        }
        // Collect eligible senders under the lock, then release it before
        // touching the bounded channels so publishers don't serialize on the
        // registry while consumers drain.
        let targets: Vec<mpsc::Sender<String>> = {
            let mut conns = self.conns.lock().expect("hub mutex");
            conns.retain(|c| !c.tx.is_closed());
            let mut t = Vec::with_capacity(conns.len());
            for c in conns.iter() {
                if c.subs_mask & ev_mask != 0 {
                    t.push(c.tx.clone());
                }
            }
            t
        };
        for tx in &targets {
            // Bounded send: drop on backpressure rather than blocking publishers.
            let _ = tx.try_send(envelope.clone());
        }
    }

    pub fn connection_count(&self) -> usize {
        self.conns.lock().expect("hub mutex").len()
    }
}

fn clearance_of(role: Role) -> ChannelClearance {
    match role {
        Role::Admin => ChannelClearance::Admin,
        Role::Operator => ChannelClearance::Operator,
        Role::Viewer => ChannelClearance::Any,
    }
}

/// Convenience wrapper used across handlers.
pub trait PublishExt {
    fn publish_event(&self, event_type: &str, payload: serde_json::Value);
}

impl PublishExt for Arc<Hub> {
    fn publish_event(&self, event_type: &str, payload: serde_json::Value) {
        self.publish(event_type, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Role;

    fn recv(rx: &mut mpsc::Receiver<String>) -> String {
        rx.try_recv().expect("event should be delivered")
    }

    #[tokio::test]
    async fn audit_events_reach_admin_only() {
        let hub = Arc::new(Hub::new());
        let (admin_id, mut admin_rx) = hub.add(Role::Admin);
        let (_viewer_id, mut viewer_rx) = hub.add(Role::Viewer);
        assert!(hub.subscribe(admin_id, "audit"));

        hub.publish("audit.login", serde_json::json!({"action": "auth.login"}));

        assert!(recv(&mut admin_rx).contains("audit.login"));
        assert!(
            viewer_rx.try_recv().is_err(),
            "viewer must not receive audit events"
        );
    }

    #[tokio::test]
    async fn download_paths_reach_operators_not_viewers() {
        let hub = Arc::new(Hub::new());
        let (op_id, mut op_rx) = hub.add(Role::Operator);
        let (_v_id, mut v_rx) = hub.add(Role::Viewer);
        assert!(hub.subscribe(op_id, "imports"));

        hub.publish(
            "verify.completed",
            serde_json::json!({"verifiedPath": "/data/library/x.mkv"}),
        );
        assert!(recv(&mut op_rx).contains("/data/library/x.mkv"));
        assert!(v_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn setup_events_reach_operators_not_viewers() {
        let hub = Arc::new(Hub::new());
        let (op_id, mut op_rx) = hub.add(Role::Operator);
        let (v_id, mut v_rx) = hub.add(Role::Viewer);
        assert!(hub.subscribe(op_id, "setup"));
        assert!(
            !hub.subscribe(v_id, "setup"),
            "viewer must not be able to subscribe to setup"
        );

        // setup.*/test.* events ride ONLY the operator-cleared setup channel.
        hub.publish(
            "setup.indexer.test.started",
            serde_json::json!({"indexerName": "ix", "indexerUrl": "http://ix:9696"}),
        );
        assert!(recv(&mut op_rx).contains("setup.indexer.test.started"));
        assert!(
            v_rx.try_recv().is_err(),
            "viewer must not receive setup events"
        );

        // The mapping change must hold end to end: no activity fallback.
        hub.unsubscribe(op_id, "setup");
        hub.subscribe(v_id, "activity");
        hub.publish("test.search.started", serde_json::json!({"runId": "r"}));
        assert!(
            v_rx.try_recv().is_err(),
            "test.* events must not reach viewers via activity"
        );
    }

    #[tokio::test]
    async fn job_events_reach_everyone() {
        let hub = Arc::new(Hub::new());
        let (_a, mut admin_rx) = hub.add(Role::Admin);
        let (_o, mut op_rx) = hub.add(Role::Operator);
        let (_v, mut viewer_rx) = hub.add(Role::Viewer);

        hub.publish("job.claimed", serde_json::json!({"kind": "search_run"}));
        for rx in [&mut admin_rx, &mut op_rx, &mut viewer_rx] {
            assert!(recv(rx).contains("job.claimed"));
        }
    }

    #[tokio::test]
    async fn unsubscribe_is_explicit_not_receive_all() {
        let hub = Arc::new(Hub::new());
        let (id, mut rx) = hub.add(Role::Admin);
        assert_eq!(
            hub.subscribed_channels(id),
            vec!["activity", "health", "jobs"]
        );

        hub.unsubscribe(id, "jobs");
        hub.unsubscribe(id, "activity");
        hub.unsubscribe(id, "health");
        hub.publish("job.completed", serde_json::json!({}));
        assert!(
            rx.try_recv().is_err(),
            "empty subscription set must mean silence"
        );

        hub.subscribe(id, "jobs");
        hub.publish("job.failed", serde_json::json!({}));
        assert!(recv(&mut rx).contains("job.failed"));
    }

    #[tokio::test]
    async fn viewer_cannot_subscribe_to_privileged_channel() {
        let hub = Arc::new(Hub::new());
        let (id, mut rx) = hub.add(Role::Viewer);
        assert!(
            !hub.subscribe(id, "audit"),
            "audit subscribe must be denied"
        );
        hub.publish("audit.x", serde_json::json!({}));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remove_stops_delivery() {
        let hub = Arc::new(Hub::new());
        let (id, rx) = hub.add(Role::Admin);
        drop(rx);
        hub.publish("job.claimed", serde_json::json!({}));
        hub.remove(id);
        assert_eq!(hub.connection_count(), 0);
    }
}
