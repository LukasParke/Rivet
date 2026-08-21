//! `acquisition_rescout`: search for an alternative release when an
//! acquisition fails, and enqueue ONE replacement.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_rescout.go`):
//! - Tried source URLs are tracked per target via blacklist entries with
//!   match_type='url' (plus hash entries), not just hashes from prior
//!   acquisitions.
//! - The rescout chain depth is capped: at most
//!   [`MAX_ACQUISITIONS_PER_TARGET_PER_DAY`] acquisitions created for a target
//!   within 24h, then the chain stops cleanly.
//! - The newer-active-acquisition guard is transactional (`BEGIN IMMEDIATE`
//!   around check + create) so two concurrent rescouts cannot both create a
//!   replacement.
//! - Candidates are ranked by (score desc, seeders desc, title asc); all
//!   accepted candidates stay stored for later runs.

use std::collections::HashMap;
use std::collections::HashSet;

use crate::adapters::SearchQuery;
use crate::jobs::handlers::{
    blacklist_set, extract_btih, load_indexers, load_policies, load_target_query, publish,
    tried_external_ids,
};
use crate::jobs::{EnqueuePayload, RescoutPayload};
use crate::pipeline::decisioning;
use crate::state::AppState;
use crate::storage::domain;
use crate::storage::jobs::{self, Job};

const SEARCH_LIMIT: i64 = 25;
/// Maximum acquisitions created for one target within the rescout window.
pub const MAX_ACQUISITIONS_PER_TARGET_PER_DAY: i64 = 5;

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: RescoutPayload = parse_payload(job)?;
    if payload.target_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }
    let target_id = payload.target_id.trim().to_string();

    // Load the failed acquisition for exclusions (optional).
    let failed = match payload
        .failed_acquisition_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        Some(id) => crate::storage::acquisitions::get(&state.db, id)
            .await
            .map_err(|e| e.to_string())?,
        None => None,
    };

    // Depth cap: too many recent acquisitions means the chain must stop.
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let recent: (i64,) =
        sqlx::query_as("SELECT COUNT(1) FROM acquisitions WHERE target_id = ? AND created_at > ?")
            .bind(&target_id)
            .bind(&cutoff)
            .fetch_one(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    if recent.0 >= MAX_ACQUISITIONS_PER_TARGET_PER_DAY {
        publish(
            state,
            "download.rescout.depth_capped",
            serde_json::json!({
                "targetId": target_id,
                "recentAcquisitions": recent.0,
                "cap": MAX_ACQUISITIONS_PER_TARGET_PER_DAY
            }),
        );
        return Ok(());
    }

    // Record what failed as blacklist entries so this exact release (by hash
    // AND source URL) is never retried for this target.
    if let Some(failed) = &failed {
        record_tried(state, &target_id, failed).await?;
    }

    let tried = tried_external_ids(state, &target_id).await?;
    let blacklist = blacklist_set(state, &target_id).await?;

    let exclude_hash = failed
        .as_ref()
        .and_then(|a| a.external_id.clone())
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            failed
                .as_ref()
                .and_then(|a| a.source_url.clone())
                .and_then(|u| extract_btih(&u))
        })
        .map(|h| h.trim().to_ascii_lowercase());
    let exclude_url = failed
        .as_ref()
        .and_then(|a| a.source_url.clone())
        .map(|u| u.trim().to_ascii_lowercase())
        .filter(|u| !u.is_empty());

    publish(
        state,
        "download.rescout.started",
        serde_json::json!({
            "targetId": target_id,
            "excludeHash": exclude_hash,
            "excludeUrl": exclude_url
        }),
    );

    // Search run for evidence/debugging.
    let run = domain::create_search_run(&state.db, &target_id)
        .await
        .map_err(|e| e.to_string())?;
    domain::update_running(&state.db, &run.id)
        .await
        .map_err(|e| e.to_string())?;

    match search_and_pick(
        state,
        &run.id,
        &target_id,
        &tried,
        &blacklist,
        exclude_hash.as_deref(),
        exclude_url.as_deref(),
    )
    .await
    {
        Ok(Some(winner)) => {
            domain::set_completed(&state.db, &run.id)
                .await
                .map_err(|e| e.to_string())?;
            create_replacement_transactional(
                state,
                &target_id,
                &failed.map(|f| f.created_at).unwrap_or_default(),
                &winner.candidate_id,
            )
            .await
        }
        Ok(None) => {
            domain::set_completed(&state.db, &run.id)
                .await
                .map_err(|e| e.to_string())?;
            publish(
                state,
                "download.rescout.no_alternative",
                serde_json::json!({"targetId": target_id, "runId": run.id}),
            );
            Ok(())
        }
        Err(e) => {
            domain::set_failed(&state.db, &run.id, &e)
                .await
                .map_err(|db| db.to_string())?;
            publish(
                state,
                "download.rescout.failed",
                serde_json::json!({"targetId": target_id, "runId": run.id, "error": e}),
            );
            Err(e)
        }
    }
}

/// Persists the failed acquisition's hash and source URL as target-scoped
/// blacklist entries (idempotent).
async fn record_tried(
    state: &AppState,
    target_id: &str,
    failed: &crate::storage::acquisitions::Acquisition,
) -> Result<(), String> {
    if let Some(hash) = failed
        .external_id
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        crate::storage::blacklist::insert(
            &state.db,
            "target",
            Some(target_id),
            "hash",
            hash,
            Some("rescout: failed acquisition"),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    if let Some(url) = failed
        .source_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        crate::storage::blacklist::insert(
            &state.db,
            "target",
            Some(target_id),
            "url",
            url,
            Some("rescout: failed acquisition"),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn search_and_pick(
    state: &AppState,
    run_id: &str,
    target_id: &str,
    tried: &HashSet<String>,
    blacklist: &HashMap<String, HashSet<String>>,
    exclude_hash: Option<&str>,
    exclude_url: Option<&str>,
) -> Result<Option<ScoredCandidate>, String> {
    let query = load_target_query(state, target_id).await?;
    let policies = load_policies(state).await;
    let indexers = load_indexers(state, run_id, target_id).await?;

    let mut best: Option<ScoredCandidate> = None;

    for ix in &indexers {
        let q = SearchQuery {
            query: query.clone(),
            categories: Vec::new(),
            limit: SEARCH_LIMIT,
        };
        let releases = match ix.search(&q).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(indexer = ix.name(), error = %e, "rescout indexer search failed");
                continue;
            }
        };
        for rel in releases {
            let normalized = decisioning::normalize_title(&rel.title);
            let normalized_json = serde_json::to_string(&normalized).unwrap_or_default();
            let cand = match domain::insert_candidate(
                &state.db,
                run_id,
                ix.name(),
                &serde_json::to_string(&rel.raw).unwrap_or_default(),
                Some(&normalized_json),
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "candidate insert failed");
                    continue;
                }
            };
            let dec = decisioning::decide(&normalized, &policies);
            if let Err(e) =
                domain::insert_decision(&state.db, &cand.id, dec.accepted, dec.score, &dec.reasons)
                    .await
            {
                tracing::warn!(error = %e, "decision insert failed");
            }
            if !dec.accepted {
                continue;
            }

            let url = rel.download_url.trim().to_string();
            if url.is_empty() {
                continue;
            }
            let url_lc = url.to_ascii_lowercase();
            if exclude_url.is_some_and(|x| x == url_lc) {
                continue;
            }
            let hash = rel
                .info_hash
                .clone()
                .filter(|h| !h.trim().is_empty())
                .or_else(|| extract_btih(&url))
                .map(|h| h.trim().to_ascii_lowercase());
            if let Some(h) = &hash {
                if exclude_hash.is_some_and(|x| x == h) || tried.contains(h) {
                    continue;
                }
                if blacklist.get("hash").is_some_and(|s| s.contains(h)) {
                    continue;
                }
            }
            if blacklist.get("url").is_some_and(|s| s.contains(&url_lc)) {
                continue;
            }

            let scored = ScoredCandidate {
                candidate_id: cand.id,
                score: dec.score,
                seeders: rel.seeders.unwrap_or(0),
                title: rel.title.trim().to_ascii_lowercase(),
            };
            if best.as_ref().is_none_or(|b| better(&scored, b)) {
                best = Some(scored);
            }
        }
    }
    Ok(best)
}

/// Creates the replacement acquisition under BEGIN IMMEDIATE so the
/// newer-active guard and the insert are atomic against concurrent rescouts.
async fn create_replacement_transactional(
    state: &AppState,
    target_id: &str,
    failed_created_at: &str,
    candidate_id: &str,
) -> Result<(), String> {
    let mut tx = state
        .db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| e.to_string())?;

    // Loop prevention INSIDE the transaction: any newer non-terminal
    // acquisition for this target wins; do not add another.
    let existing: (i64,) = sqlx::query_as(
        "SELECT COUNT(1) FROM acquisitions
         WHERE target_id = ?
           AND created_at > ?
           AND status NOT IN ('failed', 'needs_review', 'removed', 'imported')",
    )
    .bind(target_id)
    .bind(failed_created_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    if existing.0 > 0 {
        publish(
            state,
            "download.rescout.skipped_existing",
            serde_json::json!({"targetId": target_id}),
        );
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(());
    }

    // Duplicate-candidate protection inside the same transaction.
    let dup: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM acquisitions WHERE candidate_id = ?")
        .bind(candidate_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    if dup.0 > 0 {
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(());
    }

    let now = crate::timeutil::now();
    let acq_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, external_id, last_error, created_at, updated_at)
         VALUES (?, ?, ?, 'torrent', 'pending', 'qbittorrent', 'default', NULL, NULL, ?, ?)",
    )
    .bind(&acq_id)
    .bind(candidate_id)
    .bind(target_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;

    // Continuation MUST be durable.
    jobs::enqueue(
        &state.db,
        "acquisition_enqueue",
        Some(&EnqueuePayload {
            acquisition_id: acq_id.clone(),
        }),
        None,
    )
    .await
    .map_err(|e| format!("enqueue_acquisition_failed: {e}"))?;
    state.metrics.inc_jobs_enqueued();
    publish(
        state,
        "download.rescout.enqueued",
        serde_json::json!({
            "targetId": target_id,
            "replacementId": acq_id,
            "replacementCandidateId": candidate_id
        }),
    );
    Ok(())
}

fn better(a: &ScoredCandidate, b: &ScoredCandidate) -> bool {
    (a.score, a.seeders, std::cmp::Reverse(a.title.as_str()))
        > (b.score, b.seeders, std::cmp::Reverse(b.title.as_str()))
}

struct ScoredCandidate {
    candidate_id: String,
    score: i64,
    seeders: i64,
    title: String,
}

fn parse_payload(job: &Job) -> Result<RescoutPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;

    #[tokio::test]
    async fn depth_cap_stops_the_chain() {
        let (_dir, state) = testutil::test_state().await;
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, created_at, updated_at)
             VALUES ('t1', 'movie', 'tmdb', '550', 'wanted', ?, ?)",
        )
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();

        // Seed the cap: 5 acquisitions for the target within 24h.
        for i in 0..MAX_ACQUISITIONS_PER_TARGET_PER_DAY {
            sqlx::query(
                "INSERT INTO search_runs (id, target_id, status) VALUES (?, 't1', 'running')",
            )
            .bind(format!("r{i}"))
            .execute(&state.db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO release_candidates (id, search_run_id, indexer_name, raw_json, created_at)
                 VALUES (?, ?, 'ix', '{}', ?)",
            )
            .bind(format!("c{i}"))
            .bind(format!("r{i}"))
            .bind(crate::timeutil::now())
            .execute(&state.db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, created_at, updated_at)
                 VALUES (?, ?, 't1', 'torrent', 'failed', 'qbittorrent', 'default', ?, ?)",
            )
            .bind(format!("a{i}"))
            .bind(format!("c{i}"))
            .bind(crate::timeutil::now())
            .bind(crate::timeutil::now())
            .execute(&state.db)
            .await
            .unwrap();
        }

        let job = jobs::enqueue(
            &state.db,
            "acquisition_rescout",
            Some(&RescoutPayload {
                target_id: "t1".into(),
                failed_acquisition_id: Some("a0".into()),
            }),
            None,
        )
        .await
        .unwrap();

        handle(&state, &job).await.unwrap();

        // No new acquisition, no new search run work beyond the cap event.
        let n: (i64,) = sqlx::query_as("SELECT COUNT(1) FROM acquisitions WHERE target_id = 't1'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(n.0, MAX_ACQUISITIONS_PER_TARGET_PER_DAY);
        let n: (i64,) =
            sqlx::query_as("SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_enqueue'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(n.0, 0);
    }

    #[tokio::test]
    async fn missing_target_fails() {
        let (_dir, state) = testutil::test_state().await;
        let job = jobs::enqueue(
            &state.db,
            "acquisition_rescout",
            Some(&RescoutPayload {
                target_id: "".into(),
                failed_acquisition_id: None,
            }),
            None,
        )
        .await
        .unwrap();
        assert_eq!(handle(&state, &job).await.unwrap_err(), "invalid_payload");
    }
}
