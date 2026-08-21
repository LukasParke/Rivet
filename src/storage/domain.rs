//! Core domain storage: wanted targets, search runs, release candidates and
//! their accept/reject decisions.

use sqlx::SqlitePool;

use crate::timeutil;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct WantedTarget {
    pub id: String,
    pub kind: String,
    pub external_source: String,
    pub external_id: String,
    pub state: String,
    pub payload_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SearchRun {
    pub id: String,
    pub target_id: String,
    pub status: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Candidate {
    pub id: String,
    pub search_run_id: String,
    pub indexer_name: String,
    pub raw_json: String,
    pub normalized_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Decision {
    pub id: String,
    pub candidate_id: String,
    pub accepted: bool,
    pub score: i64,
    pub reasons_json: String,
    pub created_at: String,
}

/// A candidate paired with its decision (absent until the decider has run).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CandidateWithDecision {
    pub candidate: Candidate,
    pub decision: Option<Decision>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunLookupError {
    #[error("search_run not found")]
    NotFound,
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
}

fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 || limit > 200 {
        50
    } else {
        limit
    }
}

pub async fn create_wanted_target(
    pool: &SqlitePool,
    kind: &str,
    external_source: &str,
    external_id: &str,
    payload: Option<&serde_json::Value>,
) -> Result<WantedTarget, sqlx::Error> {
    let payload_json = payload.map(|p| p.to_string());
    let now = timeutil::now();
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, payload_json, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'wanted', ?, ?, ?)",
    )
    .bind(&id)
    .bind(kind)
    .bind(external_source)
    .bind(external_id)
    .bind(&payload_json)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(WantedTarget {
        id,
        kind: kind.to_string(),
        external_source: external_source.to_string(),
        external_id: external_id.to_string(),
        state: "wanted".to_string(),
        payload_json,
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn get_wanted(pool: &SqlitePool, id: &str) -> Result<Option<WantedTarget>, sqlx::Error> {
    sqlx::query_as::<_, WantedTarget>("SELECT * FROM wanted_targets WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Newest-first listing of wanted targets.
pub async fn list_wanted(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<WantedTarget>, sqlx::Error> {
    sqlx::query_as::<_, WantedTarget>(
        "SELECT * FROM wanted_targets ORDER BY created_at DESC LIMIT ?",
    )
    .bind(clamp_limit(limit))
    .fetch_all(pool)
    .await
}

pub async fn create_search_run(pool: &SqlitePool, target_id: &str) -> Result<SearchRun, sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO search_runs (id, target_id, status) VALUES (?, ?, 'queued')")
        .bind(&id)
        .bind(target_id)
        .execute(pool)
        .await?;
    Ok(SearchRun {
        id,
        target_id: target_id.to_string(),
        status: "queued".to_string(),
        started_at: None,
        completed_at: None,
        error: None,
    })
}

/// Marks the run as actively searching and pins `started_at` once.
pub async fn update_running(pool: &SqlitePool, run_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE search_runs SET status = 'running', started_at = COALESCE(started_at, ?) WHERE id = ?",
    )
    .bind(timeutil::now())
    .bind(run_id)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn set_completed(pool: &SqlitePool, run_id: &str) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query("UPDATE search_runs SET status = 'completed', completed_at = ?, error = NULL WHERE id = ?")
        .bind(&now)
        .bind(run_id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn set_failed(
    pool: &SqlitePool,
    run_id: &str,
    error: &str,
) -> Result<(), sqlx::Error> {
    let now = timeutil::now();
    sqlx::query("UPDATE search_runs SET status = 'failed', completed_at = ?, error = ? WHERE id = ?")
        .bind(&now)
        .bind(error)
        .bind(run_id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn get_search_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<SearchRun, RunLookupError> {
    sqlx::query_as::<_, SearchRun>("SELECT * FROM search_runs WHERE id = ?")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => RunLookupError::NotFound,
            other => RunLookupError::Db(other),
        })
}

pub async fn insert_candidate(
    pool: &SqlitePool,
    search_run_id: &str,
    indexer_name: &str,
    raw_json: &str,
    normalized_json: Option<&str>,
) -> Result<Candidate, sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = timeutil::now();
    sqlx::query(
        "INSERT INTO release_candidates (id, search_run_id, indexer_name, raw_json, normalized_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(search_run_id)
    .bind(indexer_name)
    .bind(raw_json)
    .bind(normalized_json)
    .bind(&created_at)
    .execute(pool)
    .await?;
    Ok(Candidate {
        id,
        search_run_id: search_run_id.to_string(),
        indexer_name: indexer_name.to_string(),
        raw_json: raw_json.to_string(),
        normalized_json: normalized_json.map(str::to_string),
        created_at,
    })
}

/// Records the decider's verdict for a candidate. `reasons` is serialized to a
/// JSON array.
pub async fn insert_decision(
    pool: &SqlitePool,
    candidate_id: &str,
    accepted: bool,
    score: i64,
    reasons: &[String],
) -> Result<Decision, sqlx::Error> {
    let reasons_json = serde_json::to_string(reasons).unwrap_or_else(|_| "[]".to_string());
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = timeutil::now();
    sqlx::query(
        "INSERT INTO decisions (id, candidate_id, accepted, score, reasons_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(candidate_id)
    .bind(accepted)
    .bind(score)
    .bind(&reasons_json)
    .bind(&created_at)
    .execute(pool)
    .await?;
    Ok(Decision {
        id,
        candidate_id: candidate_id.to_string(),
        accepted,
        score,
        reasons_json,
        created_at,
    })
}

/// Flat row for the LEFT JOIN in `get_run_with_results`; every column is
/// nullable because the decision side can be missing.
#[derive(sqlx::FromRow)]
struct CandidateDecisionRow {
    c_id: String,
    c_search_run_id: String,
    c_indexer_name: String,
    c_raw_json: String,
    c_normalized_json: Option<String>,
    c_created_at: String,
    d_id: Option<String>,
    d_accepted: Option<bool>,
    d_score: Option<i64>,
    d_reasons_json: Option<String>,
    d_created_at: Option<String>,
}

/// Loads a run with its candidates (oldest first), each optionally carrying its
/// decision. Missing runs yield [`RunLookupError::NotFound`] — exactly
/// `sqlx::Error::RowNotFound`; every other database error propagates unchanged
/// so the API layer never masks real faults as 404s (the Go bug).
pub async fn get_run_with_results(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<(SearchRun, Vec<CandidateWithDecision>), RunLookupError> {
    let run = get_search_run(pool, run_id).await?;

    let rows = sqlx::query_as::<_, CandidateDecisionRow>(
        "SELECT c.id AS c_id, c.search_run_id AS c_search_run_id,
                c.indexer_name AS c_indexer_name, c.raw_json AS c_raw_json,
                c.normalized_json AS c_normalized_json, c.created_at AS c_created_at,
                d.id AS d_id, d.accepted AS d_accepted, d.score AS d_score,
                d.reasons_json AS d_reasons_json, d.created_at AS d_created_at
         FROM release_candidates c
         LEFT JOIN decisions d ON d.candidate_id = c.id
         WHERE c.search_run_id = ?
         ORDER BY c.created_at ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;

    let results = rows
        .into_iter()
        .map(|r| CandidateWithDecision {
            candidate: Candidate {
                id: r.c_id.clone(),
                search_run_id: r.c_search_run_id,
                indexer_name: r.c_indexer_name,
                raw_json: r.c_raw_json,
                normalized_json: r.c_normalized_json,
                created_at: r.c_created_at,
            },
            decision: r.d_id.map(|d_id| Decision {
                id: d_id,
                candidate_id: r.c_id.clone(),
                accepted: r.d_accepted.unwrap_or(false),
                score: r.d_score.unwrap_or(0),
                reasons_json: r.d_reasons_json.unwrap_or_default(),
                created_at: r.d_created_at.unwrap_or_default(),
            }),
        })
        .collect();

    Ok((run, results))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("file://{}", dir.path().join("d.db").display());
        let p = crate::storage::open(&dsn).await.unwrap();
        crate::storage::migrations::apply(&p).await.unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn wanted_targets_roundtrip_and_ordering() {
        let (_d, p) = pool().await;
        let t1 =
            create_wanted_target(&p, "movie", "tmdb", "1", Some(&serde_json::json!({"year": 2001})))
                .await
                .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let t2 = create_wanted_target(&p, "episode", "tvdb", "2", None).await.unwrap();

        assert_eq!(t1.state, "wanted");
        assert_eq!(
            t1.payload_json.as_deref(),
            Some(r#"{"year":2001}"#),
            "payload serialized canonically"
        );
        assert!(t2.payload_json.is_none());

        let got = get_wanted(&p, &t1.id).await.unwrap().unwrap();
        assert_eq!(got.external_id, "1");
        assert!(get_wanted(&p, "nope").await.unwrap().is_none());

        // Newest first; invalid limits clamp to 50 instead of failing.
        let listed = list_wanted(&p, 0).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, t2.id);
        assert_eq!(listed[1].id, t1.id);
        assert_eq!(list_wanted(&p, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_run_lifecycle() {
        let (_d, p) = pool().await;
        let t = create_wanted_target(&p, "movie", "tmdb", "9", None).await.unwrap();
        let run = create_search_run(&p, &t.id).await.unwrap();
        assert_eq!(run.status, "queued");
        assert!(run.started_at.is_none());

        update_running(&p, &run.id).await.unwrap();
        let g = get_search_run(&p, &run.id).await.unwrap();
        assert_eq!(g.status, "running");
        assert!(g.started_at.is_some());
        let pinned = g.started_at.clone().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        update_running(&p, &run.id).await.unwrap();
        let g = get_search_run(&p, &run.id).await.unwrap();
        assert_eq!(g.started_at.unwrap(), pinned, "started_at pinned once");

        set_completed(&p, &run.id).await.unwrap();
        let g = get_search_run(&p, &run.id).await.unwrap();
        assert_eq!(g.status, "completed");
        assert!(g.completed_at.is_some());
        assert!(g.error.is_none());

        // A second run that fails carries the error text.
        let run2 = create_search_run(&p, &t.id).await.unwrap();
        set_failed(&p, &run2.id, "indexers down").await.unwrap();
        let g = get_search_run(&p, &run2.id).await.unwrap();
        assert_eq!(g.status, "failed");
        assert_eq!(g.error.as_deref(), Some("indexers down"));
        assert!(g.completed_at.is_some());
    }

    #[tokio::test]
    async fn candidates_and_decisions_persist() {
        let (_d, p) = pool().await;
        let t = create_wanted_target(&p, "movie", "tmdb", "9", None).await.unwrap();
        let run = create_search_run(&p, &t.id).await.unwrap();

        let c = insert_candidate(
            &p,
            &run.id,
            "idx-1",
            r#"{"guid":"a"}"#,
            Some(r#"{"title":"A"}"#),
        )
        .await
        .unwrap();
        assert_eq!(c.search_run_id, run.id);
        assert_eq!(c.normalized_json.as_deref(), Some(r#"{"title":"A"}"#));

        let d = insert_decision(&p, &c.id, true, 95, &["quality".into(), "seeders".into()])
            .await
            .unwrap();
        assert!(d.accepted);
        assert_eq!(d.score, 95);
        assert_eq!(d.reasons_json, r#"["quality","seeders"]"#);
    }

    #[tokio::test]
    async fn run_with_results_happy_path() {
        let (_d, p) = pool().await;
        let t = create_wanted_target(&p, "movie", "tmdb", "9", None).await.unwrap();
        let run = create_search_run(&p, &t.id).await.unwrap();

        let c1 = insert_candidate(&p, &run.id, "idx", r#"{}"#, None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let c2 = insert_candidate(&p, &run.id, "idx", r#"{}"#, None).await.unwrap();
        insert_decision(&p, &c1.id, false, -5, &["blacklisted".into()])
            .await
            .unwrap();

        let (got_run, results) = get_run_with_results(&p, &run.id).await.unwrap();
        assert_eq!(got_run.id, run.id);
        assert_eq!(results.len(), 2);
        // Oldest candidate first…
        assert_eq!(results[0].candidate.id, c1.id);
        assert_eq!(results[1].candidate.id, c2.id);
        // …with only the first carrying a decision.
        let d = results[0].decision.as_ref().unwrap();
        assert!(!d.accepted && d.score == -5);
        assert!(results[1].decision.is_none());
    }

    #[tokio::test]
    async fn run_with_results_not_found_vs_db_error() {
        let (_d, p) = pool().await;
        // Missing run → NotFound, not a generic error.
        match get_run_with_results(&p, "missing-run").await {
            Err(RunLookupError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }

        // Real DB failure propagates as Db even though the run exists.
        let t = create_wanted_target(&p, "movie", "tmdb", "9", None).await.unwrap();
        let run = create_search_run(&p, &t.id).await.unwrap();
        sqlx::query("DROP TABLE decisions").execute(&p).await.unwrap();
        match get_run_with_results(&p, &run.id).await {
            Err(RunLookupError::Db(_)) => {}
            other => panic!("expected Db error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cascade_delete_removes_runs_candidates_decisions() {
        let (_d, p) = pool().await;
        let t = create_wanted_target(&p, "movie", "tmdb", "9", None).await.unwrap();
        let run = create_search_run(&p, &t.id).await.unwrap();
        let c = insert_candidate(&p, &run.id, "idx", r#"{}"#, None).await.unwrap();
        insert_decision(&p, &c.id, true, 1, &[]).await.unwrap();

        sqlx::query("DELETE FROM wanted_targets WHERE id = ?")
            .bind(&t.id)
            .execute(&p)
            .await
            .unwrap();
        assert!(get_search_run(&p, &run.id).await.is_err(), "run cascaded with target");
    }
}
