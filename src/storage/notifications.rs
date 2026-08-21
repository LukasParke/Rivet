//! Notification sink storage: CRUD over the `notifications` table
//! (migration `0011_arr_domain`). A sink is an outbound channel
//! (`webhook` | `ntfy` | `discord`) plus its subscription list of event types
//! (`["*"]` matches every event).
//!
//! The struct keeps the raw TEXT columns (`config_json`, `events_json`);
//! [`crate::notify`] and the HTTP layer parse them through
//! [`NotificationSink::config`] / [`NotificationSink::events`].

use sqlx::SqlitePool;

use crate::timeutil;

/// One configured notification destination.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct NotificationSink {
    pub id: String,
    pub name: String,
    /// `webhook` | `ntfy` | `discord`.
    pub kind: String,
    pub config_json: String,
    /// JSON array of subscribed event types; `["*"]` subscribes to all.
    pub events_json: String,
    pub enabled: bool,
    pub created_at: String,
}

const COLS: &str =
    "id, name, kind, config_json, events_json, enabled, created_at";

impl NotificationSink {
    /// Parsed sink configuration (kind-specific keys such as `url`,
    /// `webhookUrl`, `topic`). Empty object when the stored JSON is malformed.
    pub fn config(&self) -> serde_json::Value {
        serde_json::from_str(&self.config_json)
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
    }

    /// Parsed event-type subscription list. Empty when the stored JSON is
    /// malformed (such a sink never fires, rather than firing on everything).
    pub fn events(&self) -> Vec<String> {
        serde_json::from_str(&self.events_json).unwrap_or_default()
    }

    /// True when this sink subscribes to `event_type` (exact entry or `"*"`).
    pub fn subscribes_to(&self, event_type: &str) -> bool {
        self.events().iter().any(|e| e == "*" || e == event_type)
    }
}

fn valid_kind(kind: &str) -> bool {
    matches!(kind, "webhook" | "ntfy" | "discord")
}

/// Inserts a sink. `kind` must be one of the three supported channels;
/// `events` empty means subscribe to nothing (the caller decides whether to
/// widen it to `["*"]`).
pub async fn create(
    pool: &SqlitePool,
    name: &str,
    kind: &str,
    config: &serde_json::Value,
    events: &[String],
    enabled: bool,
) -> Result<NotificationSink, sqlx::Error> {
    debug_assert!(valid_kind(kind), "unsupported notification kind {kind}");
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = timeutil::now();
    sqlx::query(
        "INSERT INTO notifications (id, name, kind, config_json, events_json, enabled, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(kind)
    .bind(config.to_string())
    .bind(serde_json::to_string(events).unwrap_or_else(|_| "[]".into()))
    .bind(enabled)
    .bind(&created_at)
    .execute(pool)
    .await?;
    Ok(NotificationSink {
        id,
        name: name.to_string(),
        kind: kind.to_string(),
        config_json: config.to_string(),
        events_json: serde_json::to_string(events)
            .unwrap_or_else(|_| "[]".into()),
        enabled,
        created_at,
    })
}

/// Lists every sink, oldest first.
pub async fn list(pool: &SqlitePool) -> Result<Vec<NotificationSink>, sqlx::Error> {
    sqlx::query_as::<_, NotificationSink>(
        &format!("SELECT {COLS} FROM notifications ORDER BY created_at, id"),
    )
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<NotificationSink>, sqlx::Error> {
    sqlx::query_as::<_, NotificationSink>(
        &format!("SELECT {COLS} FROM notifications WHERE id = ?"),
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Full-row update; `Ok(None)` when the id does not exist.
#[allow(clippy::too_many_arguments)]
pub async fn update(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    kind: &str,
    config: &serde_json::Value,
    events: &[String],
    enabled: bool,
) -> Result<Option<NotificationSink>, sqlx::Error> {
    debug_assert!(valid_kind(kind), "unsupported notification kind {kind}");
    let res = sqlx::query(
        "UPDATE notifications
         SET name = ?, kind = ?, config_json = ?, events_json = ?, enabled = ?
         WHERE id = ?",
    )
    .bind(name)
    .bind(kind)
    .bind(config.to_string())
    .bind(serde_json::to_string(events).unwrap_or_else(|_| "[]".into()))
    .bind(enabled)
    .bind(id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    get(pool, id).await
}

/// Deletes a sink; true when a row was removed.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM notifications WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Enabled sinks whose subscription includes `event_type` (exact or `"*"`),
/// oldest first. Subscription filtering happens after the cheap SQL scan so
/// the semantics stay identical to [`NotificationSink::subscribes_to`].
pub async fn list_enabled_matching(
    pool: &SqlitePool,
    event_type: &str,
) -> Result<Vec<NotificationSink>, sqlx::Error> {
    let rows = sqlx::query_as::<_, NotificationSink>(
        &format!(
            "SELECT {COLS} FROM notifications WHERE enabled = 1 ORDER BY created_at, id"
        ),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|s| s.subscribes_to(event_type))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let p = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("notif.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (p, dir)
    }

    fn events(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let (p, _dir) = pool().await;

        let sink = create(
            &p,
            "ops-hook",
            "webhook",
            &json!({"url": "https://hooks.example.test/rivet"}),
            &events(&["acquisition.imported", "*"]),
            true,
        )
        .await
        .unwrap();
        assert!(!sink.id.is_empty());
        assert_eq!(sink.kind, "webhook");
        assert!(sink.enabled);
        assert!(!sink.created_at.is_empty());

        // List + get see the same row.
        let listed = list(&p).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, sink.id);
        let got = get(&p, &sink.id).await.unwrap().unwrap();
        assert_eq!(got.name, "ops-hook");
        assert_eq!(got.config(), json!({"url": "https://hooks.example.test/rivet"}));
        assert_eq!(got.events(), vec!["acquisition.imported".to_string(), "*".to_string()]);
        assert!(got.subscribes_to("anything"));
        assert!(got.subscribes_to("acquisition.imported"));

        // Update rewrites the mutable fields and preserves id/created_at.
        let updated = update(
            &p,
            &sink.id,
            "renamed",
            "discord",
            &json!({"webhookUrl": "https://discord.example.test/api/webhooks/1/x"}),
            &events(&["test"]),
            false,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.kind, "discord");
        assert!(!updated.enabled);
        assert_eq!(updated.created_at, sink.created_at);
        assert!(updated.subscribes_to("test"));
        assert!(!updated.subscribes_to("acquisition.imported"));
        assert!(!updated.subscribes_to("anything"));

        // Update of a missing id is None, not an error.
        assert!(update(&p, "nope", "n", "webhook", &json!({}), &[], true)
            .await
            .unwrap()
            .is_none());

        // Delete removes exactly once.
        assert!(delete(&p, &sink.id).await.unwrap());
        assert!(!delete(&p, &sink.id).await.unwrap());
        assert!(get(&p, &sink.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn enabled_filter_matches_subscription_semantics() {
        let (p, _dir) = pool().await;
        let star = create(&p, "all", "webhook", &json!({"url": "https://a.test"}),
                          &events(&["*"]), true).await.unwrap();
        let exact = create(&p, "import-only", "ntfy", &json!({"url": "https://b.test"}),
                           &events(&["acquisition.imported"]), true).await.unwrap();
        let disabled = create(&p, "off", "webhook", &json!({"url": "https://c.test"}),
                              &events(&["acquisition.imported"]), false).await.unwrap();

        let hits = list_enabled_matching(&p, "acquisition.imported").await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&star.id.as_str()));
        assert!(ids.contains(&exact.id.as_str()));
        assert!(!ids.contains(&disabled.id.as_str()));

        let hits = list_enabled_matching(&p, "unrelated.event").await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec![star.id.as_str()]);

        // Malformed events_json disables the sink entirely (never wildcard).
        sqlx::query("UPDATE notifications SET events_json = '{oops}' WHERE id = ?")
            .bind(&exact.id)
            .execute(&p)
            .await
            .unwrap();
        let hits = list_enabled_matching(&p, "acquisition.imported").await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains(&exact.id.as_str()));
    }
}
