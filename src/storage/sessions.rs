//! Session storage: high-entropy tokens, CSRF tokens, expiry and revocation.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, Instant};

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
    let res = sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(timeutil::now())
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ());
    // Evict synchronously so logout and any other revocation path takes
    // effect on the very next request, not after SESSION_CACHE_TTL.
    cache_evict(id);
    res
}

// ---------------------------------------------------------------------------
// Per-request auth cache
//
// `load_session` used to run two SELECTs (session + user) plus one
// `last_seen_at` UPDATE on EVERY authenticated request; every hot GET pays
// that tax. This process-local cache serves the steady state without
// touching the database.
//
// TTL tradeoff: an entry is trusted for [`SESSION_CACHE_TTL`] without
// re-reading the DB, so changes made behind the cache's back surface at most
// one TTL late:
// - Revocation is NEVER delayed: [`revoke`] evicts the sid synchronously,
//   which keeps logout immediate by construction no matter who calls it.
// - Expiry is re-checked against the wall clock on each hit, so TTL expiry
//   needs no invalidation either.
// - Role or password changes made through other code paths are bounded by
//   the 5 s window.
/// How long a cached `(session, user)` pair is trusted without a DB re-read.
pub const SESSION_CACHE_TTL: StdDuration = StdDuration::from_secs(5);
/// Minimum interval between `last_seen_at` writes for the same session.
pub const TOUCH_MIN_INTERVAL: StdDuration = StdDuration::from_secs(60);
const SESSION_CACHE_MAX: usize = 4096;

struct CachedAuth {
    session: Session,
    user: crate::storage::users::User,
    cached_at: Instant,
}

fn auth_cache() -> &'static Mutex<HashMap<String, CachedAuth>> {
    static CACHE: LazyLock<Mutex<HashMap<String, CachedAuth>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &CACHE
}

fn cache_evict(id: &str) {
    // A poisoned mutex only loses caching, never correctness.
    if let Ok(mut map) = auth_cache().lock() {
        map.remove(id);
    }
}

/// Drops every cached entry belonging to `user_id` (all of their sessions).
/// Called whenever a user row is mutated (TOTP enrollment today; role or
/// password changes as they gain API surfaces) so the change applies on the
/// next request instead of after [`SESSION_CACHE_TTL`].
pub fn cache_evict_user(user_id: &str) {
    if let Ok(mut map) = auth_cache().lock() {
        map.retain(|_, e| e.session.user_id != user_id);
    }
}

fn cache_hit(id: &str) -> Option<(Session, crate::storage::users::User)> {
    let mut map = auth_cache().lock().ok()?;
    if let Some(entry) = map.get(id) {
        let fresh = entry.cached_at.elapsed() < SESSION_CACHE_TTL;
        let live = entry.session.revoked_at.is_none()
            && timeutil::parse(&entry.session.expires_at)
                .map(|exp| Utc::now() <= exp)
                .unwrap_or(false);
        if fresh && live {
            return Some((entry.session.clone(), entry.user.clone()));
        }
    }
    map.remove(id);
    None
}

fn cache_store(session: &Session, user: &crate::storage::users::User) {
    let Ok(mut map) = auth_cache().lock() else {
        return;
    };
    if map.len() >= SESSION_CACHE_MAX && !map.contains_key(&session.id) {
        let now = Instant::now();
        map.retain(|_, e| now.duration_since(e.cached_at) < SESSION_CACHE_TTL);
    }
    map.insert(
        session.id.clone(),
        CachedAuth {
            session: session.clone(),
            user: user.clone(),
            cached_at: Instant::now(),
        },
    );
}

/// Sliding `last_seen_at` bookkeeping, throttled to one write per
/// [`TOUCH_MIN_INTERVAL`] per session; failure must not break the request.
/// On write, the passed-in view is refreshed so cached copies stay
/// authoritative for the throttle decision without another read.
async fn throttled_touch(pool: &SqlitePool, mut session: Session) -> Session {
    let due = session
        .last_seen_at
        .as_deref()
        .and_then(timeutil::parse)
        .map(|seen| Utc::now() - seen > chrono::Duration::from_std(TOUCH_MIN_INTERVAL).unwrap())
        .unwrap_or(true);
    if due {
        let _ = touch(pool, &session.id).await;
        session.last_seen_at = Some(timeutil::now());
    }
    session
}

/// Cache-first resolution of a session cookie to its live `(session, user)`
/// pair; used by the auth middleware on every request. Returns `None` when
/// the sid is unknown, revoked or expired (callers treat that as anonymous)
/// or when the user row has disappeared.
///
/// Cache misses read through to the database and repopulate the entry; hits
/// cost zero queries and at most one throttled `last_seen_at` write per
/// [`TOUCH_MIN_INTERVAL`]. See the module-level tradeoff notes above.
pub async fn lookup_active(
    pool: &SqlitePool,
    id: &str,
) -> Option<(Session, crate::storage::users::User)> {
    if let Some((session, user)) = cache_hit(id) {
        let session = throttled_touch(pool, session).await;
        return Some((session, user));
    }

    let session = get(pool, id).await.ok()??;
    // Expired or revoked sessions are anonymous too; an unparseable expiry
    // counts as expired (fail closed on the timestamp only).
    let live = timeutil::parse(&session.expires_at)
        .map(|exp| Utc::now() <= exp)
        .unwrap_or(false);
    if session.revoked_at.is_some() || !live {
        return None;
    }
    let user = match crate::storage::users::get_by_id(pool, &session.user_id).await {
        Ok(u) => u,
        Err(_) => return None,
    };
    let session = throttled_touch(pool, session).await;
    cache_store(&session, &user);
    Some((session, user))
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
