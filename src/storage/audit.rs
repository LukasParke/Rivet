//! Audit event recording.

use sqlx::SqlitePool;

use crate::timeutil;

#[derive(Debug, Clone, Default)]
pub struct AuditEvent {
    pub actor_user_id: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub details_json: Option<String>,
}

pub async fn insert(pool: &SqlitePool, ev: AuditEvent) -> Result<(), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO audit_events
         (id, actor_user_id, action, target_type, target_id, ip, user_agent, details_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(ev.actor_user_id)
    .bind(&ev.action)
    .bind(ev.target_type)
    .bind(ev.target_id)
    .bind(ev.ip)
    .bind(ev.user_agent)
    .bind(ev.details_json)
    .bind(timeutil::now())
    .execute(pool)
    .await
    .map(|_| ())
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct AuditRow {
    pub id: String,
    pub actor_user_id: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub created_at: String,
}

pub async fn recent(pool: &SqlitePool, limit: i64) -> Result<Vec<AuditRow>, sqlx::Error> {
    sqlx::query_as::<_, AuditRow>(
        "SELECT id, actor_user_id, action, target_type, target_id, created_at
         FROM audit_events ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}
