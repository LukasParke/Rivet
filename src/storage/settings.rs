//! Settings table: JSON string values keyed by name.

use sqlx::SqlitePool;

use crate::timeutil;

/// Returns the raw JSON string for `key`, or None when absent.
pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT value_json FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
}

pub async fn upsert(pool: &SqlitePool, key: &str, value_json: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value_json)
    .bind(timeutil::now())
    .execute(pool)
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upsert_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = crate::storage::open(&format!("file://{}", dir.path().join("x.db").display()))
            .await
            .unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        upsert(&p, "setup.paths", r#"{"a":1}"#).await.unwrap();
        upsert(&p, "setup.paths", r#"{"a":2}"#).await.unwrap();
        assert_eq!(get(&p, "setup.paths").await.unwrap().unwrap(), r#"{"a":2}"#);
        assert!(get(&p, "missing").await.unwrap().is_none());
    }
}
