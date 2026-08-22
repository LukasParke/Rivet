//! Users table access.

use chrono::Utc;
use sqlx::SqlitePool;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    pub role: String,
    pub totp_secret_enc: Option<String>,
    #[serde(default)]
    pub totp_enabled: bool,
    /// Timestep of the most recently accepted TOTP code (replay guard);
    /// NULL before the first acceptance.
    #[serde(default)]
    pub totp_last_step: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn count_users(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
}

pub async fn get_by_email(pool: &SqlitePool, email: &str) -> Result<User, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = ?")
        .bind(email)
        .fetch_one(pool)
        .await
}

pub async fn get_by_id(pool: &SqlitePool, id: &str) -> Result<User, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// Creates a user; email uniqueness is enforced by the schema.
pub async fn create(
    pool: &SqlitePool,
    email: &str,
    password_hash: &str,
    role: &str,
) -> Result<User, sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::timeutil::fmt(Utc::now());
    sqlx::query("INSERT INTO users (id, email, password_hash, role, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(email)
        .bind(password_hash)
        .bind(role)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    get_by_id(pool, &id).await
}

/// Sets (or clears) the encrypted TOTP secret and enabled flag atomically.
pub async fn set_totp_secret(
    pool: &SqlitePool,
    user_id: &str,
    enc: Option<&str>,
    enabled: bool,
) -> Result<(), sqlx::Error> {
    let now = crate::timeutil::fmt(Utc::now());
    sqlx::query(
        "UPDATE users
         SET totp_secret_enc = ?, totp_enabled = ?, totp_last_step = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(enc)
    .bind(enabled)
    .bind(&now)
    .bind(user_id)
    .execute(pool)
    .await
    .map(|_| ())?;
    // The auth middleware caches (session, user) pairs; drop this user's
    // entries so the TOTP change is visible on the very next request.
    crate::storage::sessions::cache_evict_user(user_id);
    Ok(())
}

/// Atomically records `step` as the user's latest accepted TOTP timestep.
///
/// The compare-and-set happens inside a single UPDATE, so concurrent logins
/// cannot both win: returns `Ok(false)` when a step >= `step` was already
/// accepted (code replay).
pub async fn record_totp_step(
    pool: &SqlitePool,
    user_id: &str,
    step: i64,
) -> Result<bool, sqlx::Error> {
    let now = crate::timeutil::fmt(Utc::now());
    let res = sqlx::query(
        "UPDATE users SET totp_last_step = ?, updated_at = ?
         WHERE id = ? AND (totp_last_step IS NULL OR totp_last_step < ?)",
    )
    .bind(step)
    .bind(&now)
    .bind(user_id)
    .bind(step)
    .execute(pool)
    .await?;
    let accepted = res.rows_affected() == 1;
    if accepted {
        // Keep the auth middleware's cached user consistent with the row.
        crate::storage::sessions::cache_evict_user(user_id);
    }
    Ok(accepted)
}
