//! Blacklist storage: known-bad release exclusions (hash/url/title matches).
//!
//! Dedupe relies on the `ux_blacklist_entries` unique index
//! `(scope, COALESCE(target_id,''), match_type, match_value)` added in 0009:
//! inserts use `INSERT OR IGNORE` and report whether the row was newly
//! created, fixing the Go version's duplicate-insert bug.

use sqlx::SqlitePool;

use crate::timeutil;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct BlacklistEntry {
    pub id: String,
    pub scope: String,
    pub target_id: Option<String>,
    pub match_type: String,
    pub match_value: String,
    pub reason: Option<String>,
    pub created_by_user_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BlacklistError {
    #[error("invalid_scope")]
    InvalidScope,
    #[error("target_id_required")]
    TargetIdRequired,
    #[error("match_type_required")]
    MatchTypeRequired,
    #[error("invalid_match_type")]
    InvalidMatchType,
    #[error("match_value_required")]
    MatchValueRequired,
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Normalizes and validates inputs exactly like Go's InsertBlacklistEntry.
/// Returns `(scope, target_id, match_type, match_value)`.
fn validate(
    scope: &str,
    target_id: Option<&str>,
    match_type: &str,
    match_value: &str,
) -> Result<(String, Option<String>, String, String), BlacklistError> {
    let scope = normalize(scope);
    let scope = if scope.is_empty() {
        "target".to_string()
    } else {
        scope
    };
    if scope != "global" && scope != "target" {
        return Err(BlacklistError::InvalidScope);
    }

    let target_id = match scope.as_str() {
        "target" => {
            let tid = target_id.map(str::trim).filter(|t| !t.is_empty());
            match tid {
                Some(t) => Some(t.to_string()),
                None => return Err(BlacklistError::TargetIdRequired),
            }
        }
        _ => None,
    };

    let match_type = normalize(match_type);
    if match_type.is_empty() {
        return Err(BlacklistError::MatchTypeRequired);
    }
    if !matches!(match_type.as_str(), "hash" | "url" | "title") {
        return Err(BlacklistError::InvalidMatchType);
    }

    let match_value = normalize(match_value);
    if match_value.is_empty() {
        return Err(BlacklistError::MatchValueRequired);
    }

    Ok((scope, target_id, match_type, match_value))
}

fn trim_opt(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Inserts an entry, ignoring duplicates against the unique index. Returns the
/// stored row and whether it was newly created (`false` means an identical
/// entry already existed and was left untouched).
pub async fn insert(
    pool: &SqlitePool,
    scope: &str,
    target_id: Option<&str>,
    match_type: &str,
    match_value: &str,
    reason: Option<&str>,
    created_by_user_id: Option<&str>,
) -> Result<(BlacklistEntry, bool), BlacklistError> {
    let (scope, target_id, match_type, match_value) =
        validate(scope, target_id, match_type, match_value)?;
    let reason = trim_opt(reason);
    let created_by_user_id = trim_opt(created_by_user_id);

    let id = uuid::Uuid::new_v4().to_string();
    let now = timeutil::now();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO blacklist_entries
           (id, scope, target_id, match_type, match_value, reason, created_by_user_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&scope)
    .bind(&target_id)
    .bind(&match_type)
    .bind(&match_value)
    .bind(&reason)
    .bind(&created_by_user_id)
    .bind(&now)
    .execute(pool)
    .await?;

    if res.rows_affected() == 1 {
        return Ok((
            BlacklistEntry {
                id,
                scope,
                target_id,
                match_type,
                match_value,
                reason,
                created_by_user_id,
                created_at: now,
            },
            true,
        ));
    }

    // Duplicate: surface the pre-existing row so callers can render it.
    let existing: BlacklistEntry = sqlx::query_as(
        "SELECT * FROM blacklist_entries
         WHERE scope = ? AND COALESCE(target_id,'') = COALESCE(?, '')
           AND match_type = ? AND match_value = ?",
    )
    .bind(&scope)
    .bind(&target_id)
    .bind(&match_type)
    .bind(&match_value)
    .fetch_one(pool)
    .await?;
    Ok((existing, false))
}

/// True when any of the provided values is blacklisted for `target_id` —
/// considering both global entries and entries scoped to that target. Values
/// are normalized before matching; blank values are ignored.
pub async fn matches_exist(
    pool: &SqlitePool,
    target_id: Option<&str>,
    hash: Option<&str>,
    url: Option<&str>,
    title: Option<&str>,
) -> Result<bool, sqlx::Error> {
    // match_type literals are fixed above; only values are bound.
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    for (mt, v) in [("hash", hash), ("url", url), ("title", title)] {
        if let Some(v) = v.map(normalize).filter(|v| !v.is_empty()) {
            clauses.push(format!("(match_type = '{mt}' AND match_value = ?)"));
            values.push(v);
        }
    }
    if clauses.is_empty() {
        return Ok(false);
    }

    let sql = format!(
        "SELECT EXISTS(
           SELECT 1 FROM blacklist_entries
           WHERE (scope = 'global' OR (scope = 'target' AND target_id = ?))
             AND ({})
         )",
        clauses.join(" OR ")
    );

    let mut q = sqlx::query_as::<_, (i64,)>(&sql).bind(target_id);
    for v in &values {
        q = q.bind(v);
    }
    let (hit,): (i64,) = q.fetch_one(pool).await?;
    Ok(hit != 0)
}

pub async fn list(pool: &SqlitePool, limit: i64) -> Result<Vec<BlacklistEntry>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 500 {
        100
    } else {
        limit
    };
    sqlx::query_as::<_, BlacklistEntry>(
        "SELECT * FROM blacklist_entries ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Deletes by id. Returns whether a row was removed.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM blacklist_entries WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Go-parity exclusion filter source: blacklisted values grouped by
/// `match_type`, including global entries plus entries scoped to `target_id`.
/// Values are normalized (trimmed, lowercased).
pub async fn load_set(
    pool: &SqlitePool,
    target_id: &str,
) -> Result<std::collections::HashMap<String, std::collections::HashSet<String>>, sqlx::Error> {
    let target_id = target_id.trim();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT match_type, match_value FROM blacklist_entries
         WHERE scope = 'global' OR (scope = 'target' AND target_id = ?)",
    )
    .bind(target_id)
    .fetch_all(pool)
    .await?;

    let mut out: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (t, v) in rows {
        let t = normalize(&t);
        let v = normalize(&v);
        if t.is_empty() || v.is_empty() {
            continue;
        }
        out.entry(t).or_default().insert(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("b.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn insert_dedupes_on_unique_index() {
        let (_d, p) = pool().await;
        let (e, created) = insert(
            &p,
            "global",
            None,
            "hash",
            "ABC123",
            Some("bad rip"),
            Some("u1"),
        )
        .await
        .unwrap();
        assert!(created);
        assert_eq!(e.match_value, "abc123", "value normalized");
        assert_eq!(e.scope, "global");
        assert_eq!(e.reason.as_deref(), Some("bad rip"));

        // Repeat insert with a different id/reason must not create a second row.
        let (e2, created2) = insert(&p, "GLOBAL", None, "Hash", " abc123 ", Some("again"), None)
            .await
            .unwrap();
        assert!(!created2);
        assert_eq!(e2.id, e.id, "returns the pre-existing row");
        assert_eq!(
            e2.reason.as_deref(),
            Some("bad rip"),
            "original reason untouched"
        );

        let all = list(&p, 10).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn target_scope_distinct_from_global() {
        let (_d, p) = pool().await;
        // Same value under global and target scopes are distinct rows.
        let (g, _) = insert(&p, "global", None, "url", "https://x/dl", None, None)
            .await
            .unwrap();
        let (t, _) = insert(
            &p,
            "target",
            Some("tgt-1"),
            "url",
            "https://x/dl",
            None,
            None,
        )
        .await
        .unwrap();
        assert_ne!(g.id, t.id);
        assert_eq!(list(&p, 10).await.unwrap().len(), 2);

        // Blank scope defaults to "target"; without a target id that is invalid.
        let err = insert(&p, "", None, "hash", "h", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BlacklistError::TargetIdRequired));
    }

    #[tokio::test]
    async fn validation_errors() {
        let (_d, p) = pool().await;
        let cases: Vec<(&str, Option<&str>, &str, &str, BlacklistError)> = vec![
            ("bogus", None, "hash", "v", BlacklistError::InvalidScope),
            (
                "target",
                None,
                "hash",
                "v",
                BlacklistError::TargetIdRequired,
            ),
            (
                "target",
                Some("  "),
                "hash",
                "v",
                BlacklistError::TargetIdRequired,
            ),
            ("global", None, "", "v", BlacklistError::MatchTypeRequired),
            (
                "global",
                None,
                "imdb",
                "v",
                BlacklistError::InvalidMatchType,
            ),
            (
                "global",
                None,
                "hash",
                "   ",
                BlacklistError::MatchValueRequired,
            ),
        ];
        for (scope, tid, mt, mv, want) in cases {
            let err = insert(&p, scope, tid, mt, mv, None, None)
                .await
                .unwrap_err();
            assert!(
                std::mem::discriminant(&err) == std::mem::discriminant(&want),
                "scope={scope} mt={mt}: got {err:?}, want {:?}",
                want
            );
        }
        // Nothing was written.
        assert!(list(&p, 10).await.unwrap().is_empty());
        // Blank optional fields are nulled rather than stored as "".
        let (e, _) = insert(&p, "global", None, "title", "v", Some("  "), Some(""))
            .await
            .unwrap();
        assert!(e.reason.is_none() && e.created_by_user_id.is_none());
    }

    #[tokio::test]
    async fn matches_exist_global_and_target_scope() {
        let (_d, p) = pool().await;
        insert(&p, "global", None, "hash", "deadbeef", None, None)
            .await
            .unwrap();
        insert(&p, "target", Some("tgt-9"), "title", "cam rip", None, None)
            .await
            .unwrap();

        // Global hit, normalization applied to probe value.
        assert!(
            matches_exist(&p, Some("other"), Some("DEADBEEF"), None, None)
                .await
                .unwrap()
        );
        // Target-scoped hit only for that target.
        assert!(
            matches_exist(&p, Some("tgt-9"), None, None, Some(" Cam Rip "))
                .await
                .unwrap()
        );
        // Not visible to other targets…
        assert!(
            !matches_exist(&p, Some("tgt-8"), None, None, Some("cam rip"))
                .await
                .unwrap()
        );
        // …nor without a target context.
        assert!(!matches_exist(&p, None, None, None, Some("cam rip"))
            .await
            .unwrap());
        // Misses.
        assert!(
            !matches_exist(&p, Some("tgt-9"), Some("cafe"), Some("z"), None)
                .await
                .unwrap()
        );
        // All-blank probe short-circuits to false.
        assert!(!matches_exist(&p, Some("tgt-9"), Some(""), None, None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn list_and_delete_roundtrip() {
        let (_d, p) = pool().await;
        let (e, _) = insert(&p, "global", None, "url", "u1", None, None)
            .await
            .unwrap();
        insert(&p, "global", None, "url", "u2", None, None)
            .await
            .unwrap();

        let listed = list(&p, 1).await.unwrap();
        assert_eq!(listed.len(), 1);

        assert!(delete(&p, &e.id).await.unwrap());
        assert!(
            !delete(&p, &e.id).await.unwrap(),
            "second delete reports false"
        );
        assert!(!delete(&p, "missing").await.unwrap());
    }

    #[tokio::test]
    async fn load_set_groups_by_match_type() {
        let (_d, p) = pool().await;
        insert(&p, "global", None, "hash", "H1", None, None)
            .await
            .unwrap();
        insert(&p, "global", None, "url", "https://x/1", None, None)
            .await
            .unwrap();
        insert(&p, "target", Some("t1"), "title", " Cam Rip ", None, None)
            .await
            .unwrap();

        let set = load_set(&p, "t1").await.unwrap();
        assert_eq!(
            set.get("hash").unwrap(),
            &["h1".to_string()].into_iter().collect()
        );
        assert!(set.get("url").unwrap().contains("https://x/1"));
        assert!(set.get("title").unwrap().contains("cam rip"));

        // Other target: only globals.
        let set = load_set(&p, "t2").await.unwrap();
        assert!(set.contains_key("hash") && set.contains_key("url"));
        assert!(!set.contains_key("title"));
    }
}
