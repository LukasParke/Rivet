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
        "UPDATE users SET totp_secret_enc = ?, totp_enabled = ?, updated_at = ? WHERE id = ?",
    )
    .bind(enc)
    .bind(enabled)
    .bind(&now)
    .bind(user_id)
    .execute(pool)
    .await
    .map(|_| ())
}
