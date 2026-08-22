//! Session storage: high-entropy tokens, CSRF tokens, expiry and revocation.

use chrono::{Duration, Utc};
use sqlx::SqlitePool;

use crate::timeutil;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    #[sqlx(default)]
    pub csrf_token: String,
    pub created_at: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("userID required")]
    MissingUser,
    #[error("ttl must be positive")]
    BadTtl,
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Generates `n` random bytes, base64 RawStd encoded (matches Go token format).
pub fn new_token(n: usize) -> String {
    use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
    use base64::Engine;
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    B64.encode(b)
}

pub async fn create(
    pool: &SqlitePool,
    user_id: &str,
    ttl: Duration,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Session, SessionError> {
    if user_id.is_empty() {
        return Err(SessionError::MissingUser);
    }
    if ttl <= Duration::zero() {
        return Err(SessionError::BadTtl);
    }
    let id = new_token(32);
    let csrf = new_token(32);
    let now = Utc::now();
    let expires = now + ttl;
    sqlx::query(
        "INSERT INTO sessions (id, user_id, csrf_token, created_at, expires_at, ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(&csrf)
    .bind(timeutil::fmt(now))
    .bind(timeutil::fmt(expires))
    .bind(ip)
    .bind(user_agent)
    .execute(pool)
    .await?;
    get(pool, &id)
        .await?
        .ok_or_else(|| SessionError::Db(sqlx::Error::RowNotFound))
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Session>, sqlx::Error> {
    sqlx::query_as::<_, Session>("SELECT * FROM sessions WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn touch(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn revoke(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Deletes sessions expired at least `grace` ago. Returns rows removed.
/// Wired into the retention janitor (the original left this dead code).
pub async fn delete_expired(pool: &SqlitePool, grace: Duration) -> Result<u64, sqlx::Error> {
    let cutoff = timeutil::fmt(Utc::now() - grace);
    let res = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("s.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn create_get_revoke() {
        let (_d, p) = pool().await;
        let u = crate::storage::users::create(&p, "a@b.c", "argon2id$hash", "admin")
            .await
            .unwrap();
        let s = create(&p, &u.id, Duration::hours(1), None, None)
            .await
            .unwrap();
        assert_eq!(s.csrf_token.len(), 43, "32 bytes base64-nopad = 43 chars");
        let got = get(&p, &s.id).await.unwrap().unwrap();
        assert_eq!(got.user_id, u.id);
        revoke(&p, &s.id).await.unwrap();
        let got = get(&p, &s.id).await.unwrap().unwrap();
        assert!(got.revoked_at.is_some());
    }

    #[tokio::test]
    async fn expired_sessions_purged() {
        let (_d, p) = pool().await;
        let u = crate::storage::users::create(&p, "a@b.c", "argon2id$hash", "admin")
            .await
            .unwrap();
        let s = create(&p, &u.id, Duration::milliseconds(1), None, None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let n = delete_expired(&p, Duration::zero()).await.unwrap();
        assert_eq!(n, 1);
        assert!(get(&p, &s.id).await.unwrap().is_none());
    }
}
