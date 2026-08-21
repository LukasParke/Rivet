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
    pub created_at: String,
    pub updated_at: String,
}

pub async fn count_users(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(pool).await
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
