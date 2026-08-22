//! `search_run`: query every configured indexer for a target's query, record
//! candidates + decisions, and create ONE acquisition for the best candidate.
//!
//! Fixes over the Go original (`internal/jobs/handlers/search_run.go`):
//! - No "ubuntu" fallback query: a target without a query fails the run.
//! - Exclusions (tried external ids, blacklist hash/url, existing acquisition
//!   for the candidate) are applied BEFORE creating acquisitions.
//! - The best accepted candidate wins by (decision score desc, seeders desc,
//!   title asc) and exactly one acquisition is created per run; all other
//!   accepted candidates remain stored for rescouting.
//! - Continuation enqueues propagate failures; any failure marks the run
//!   failed instead of leaving it running/completed.

use crate::adapters::indexers::newznab::redact_apikey;
use crate::adapters::SearchQuery;
use crate::jobs::handlers::{
    blacklist_set, load_indexers, load_policies, load_target_query, publish,
};
use crate::jobs::{EnqueuePayload, SearchRunPayload};
use crate::pipeline::decisioning;
use crate::state::AppState;
use crate::storage::domain;
use crate::storage::jobs::{self, Job};

const SEARCH_LIMIT: i64 = 25;

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: SearchRunPayload = parse_payload(job)?;
    if payload.run_id.trim().is_empty() || payload.target_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }

    match run_inner(state, &payload.run_id, &payload.target_id).await {
        Ok(()) => {
            publish(
                state,
                "search.run.completed",
                serde_json::json!({"runId": payload.run_id, "targetId": payload.target_id}),
            );
            Ok(())
        }
        Err(e) => {
            // Mark the run failed rather than leaving it "running".
            if let Err(db_err) = domain::set_failed(&state.db, &payload.run_id, &e).await {
                tracing::error!(run_id = %payload.run_id, error = %db_err, "failed to mark search run failed");
            }
            publish(
                state,
                "search.run.failed",
                serde_json::json!({"runId": payload.run_id, "targetId": payload.target_id, "error": redact_apikey(&e)}),
            );
            Err(e)
        }
    }
}

async fn run_inner(state: &AppState, run_id: &str, target_id: &str) -> Result<(), String> {
    publish(
        state,
        "search.run.started",
        serde_json::json!({"runId": run_id, "targetId": target_id}),
    );
    domain::update_running(&state.db, run_id)
        .await
        .map_err(|e| e.to_string())?;

    let query = load_target_query(state, target_id).await?;
    let policies = load_policies(state).await;

    let indexers = load_indexers(state, run_id, target_id).await?;
    let names: Vec<&str> = indexers.iter().map(|ix| ix.name()).collect();
    publish(
        state,
        "search.indexers.loaded",
        serde_json::json!({
            "runId": run_id,
            "targetId": target_id,
            "count": names.len(),
            "indexerNames": names,
        }),
    );
    publish(
        state,
        "search.loop.started",
        serde_json::json!({
            "runId": run_id,
            "targetId": target_id,
            "query": query,
            "count": names.len()
        }),
    );

    let tried = crate::jobs::handlers::tried_external_ids(state, target_id).await?;
    let blacklist = blacklist_set(state, target_id).await?;

    let mut best: Option<ScoredCandidate> = None;

    for ix in &indexers {
        publish(
            state,
            "search.indexer.started",
            serde_json::json!({"runId": run_id, "targetId": target_id, "indexer": ix.name(), "query": query}),
        );
        let q = SearchQuery {
            query: query.clone(),
            categories: Vec::new(),
            limit: SEARCH_LIMIT,
        };
        let releases = match ix.search(&q).await {
            Ok(r) => r,
            Err(e) => {
                publish(
                    state,
                    "search.indexer.error",
                    serde_json::json!({
                        "runId": run_id,
                        "targetId": target_id,
                        "indexer": ix.name(),
                        "error": redact_apikey(&e.to_string())
                    }),
                );
                continue;
            }
        };
        if releases.is_empty() {
            publish(
                state,
                "search.indexer.empty",
                serde_json::json!({"runId": run_id, "targetId": target_id, "indexer": ix.name(), "query": query}),
            );
            continue;
        }

        for rel in releases {
            let normalized = decisioning::normalize_title(&rel.title);
            let normalized_json = serde_json::to_string(&normalized).unwrap_or_default();

            // Persist the FULL release: downstream stages (enqueue's
            // candidate_missing_url check, monitor, rescout dedup) read
            // download_url/info_hash from this row.
            let cand = match domain::insert_candidate(
                &state.db,
                run_id,
                ix.name(),
                &serde_json::to_string(&rel).unwrap_or_default(),
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
            publish(
                state,
                "candidate.recorded",
                serde_json::json!({"runId": run_id, "candidateId": cand.id, "indexer": ix.name()}),
            );

            let dec = decisioning::decide(&normalized, &policies);
            if let Err(e) =
                domain::insert_decision(&state.db, &cand.id, dec.accepted, dec.score, &dec.reasons)
                    .await
            {
                tracing::warn!(error = %e, "decision insert failed");
            }
            publish(
                state,
                "decision.recorded",
                serde_json::json!({
                    "runId": run_id,
                    "candidateId": cand.id,
                    "accepted": dec.accepted,
                    "score": dec.score,
                    "reasons": dec.reasons
                }),
            );

            if !dec.accepted {
                continue;
            }

            let url = rel.download_url.trim().to_string();
            if url.is_empty() {
                continue;
            }
            let hash = rel
                .info_hash
                .clone()
                .filter(|h| !h.trim().is_empty())
                .or_else(|| crate::jobs::handlers::extract_btih(&url));
            let hash_lc = hash.as_ref().map(|h| h.trim().to_ascii_lowercase());

            // Exclusions BEFORE considering creation.
            if hash_lc.as_ref().is_some_and(|h| tried.contains(h)) {
                continue;
            }
            if hash_lc
                .as_ref()
                .is_some_and(|h| blacklist.get("hash").is_some_and(|s| s.contains(h)))
            {
                continue;
            }
            if blacklist
                .get("url")
                .is_some_and(|s| s.contains(&url.to_ascii_lowercase()))
            {
                continue;
            }
            if crate::storage::acquisitions::get_by_candidate(&state.db, &cand.id)
                .await
                .map_err(|e| e.to_string())?
                .is_some()
            {
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

    if let Some(winner) = best {
        let acq = crate::storage::acquisitions::create_acquisition(
            &state.db,
            &winner.candidate_id,
            target_id,
            "torrent",
            "qbittorrent",
            "default",
        )
        .await
        .map_err(|e| e.to_string())?;

        // Continuation MUST be durable: a failed enqueue fails the job so the
        // retry re-drives the chain from the recorded candidates.
        jobs::enqueue(
            &state.db,
            "acquisition_enqueue",
            Some(&EnqueuePayload {
                acquisition_id: acq.id.clone(),
            }),
            None,
        )
        .await
        .map_err(|e| format!("enqueue_acquisition_failed: {e}"))?;
        state.metrics.inc_jobs_enqueued();
        publish(
            state,
            "job.enqueued",
            serde_json::json!({
                "kind": "acquisition_enqueue",
                "acquisitionId": acq.id,
                "candidateId": winner.candidate_id
            }),
        );
    } else {
        publish(
            state,
            "search.no_candidate",
            serde_json::json!({"runId": run_id, "targetId": target_id}),
        );
    }

    domain::set_completed(&state.db, run_id)
        .await
        .map_err(|e| e.to_string())?;
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

fn parse_payload(job: &Job) -> Result<SearchRunPayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;

    async fn seed_target(state: &AppState, id: &str, query: &str) {
        sqlx::query(
            "INSERT INTO wanted_targets (id, kind, external_source, external_id, state, payload_json, created_at, updated_at)
             VALUES (?, 'movie', 'tmdb', '550', 'wanted', ?, ?, ?)",
        )
        .bind(id)
        .bind(serde_json::json!({ "query": query }).to_string())
        .bind(crate::timeutil::now())
        .bind(crate::timeutil::now())
        .execute(&state.db)
        .await
        .unwrap();
    }

    /// A search failure whose error text embeds the full request URL (reqwest
    /// errors do) must be published with the indexer API key REDACTED, never
    /// in plaintext.
    #[tokio::test]
    async fn indexer_error_events_redact_apikey() {
        let (_dir, mut state) = testutil::test_state().await;
        // Allow loopback so the adapter reaches the dead endpoint instead of
        // failing SSRF validation before a URL-bearing error exists.
        state.policy = crate::security::OutboundPolicy::dev();

        // Capture published events as an operator on the activity channel.
        let (sub_id, mut rx) = state.hub.add(crate::state::Role::Operator);
        assert!(state.hub.subscribe(sub_id, "activity"));

        // Port with nothing listening: reqwest connect error embeds the FULL
        // request URL, including ?apikey=SECRET.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let key_enc = state.secret_box.encrypt_to_string(b"SECRET").unwrap();
        let indexers = serde_json::json!({
            "torznab": [],
            "newznab": [{"name": "ix", "url": format!("http://{addr}"), "apiKeyEnc": key_enc}],
        });
        crate::storage::settings::upsert(&state.db, "setup.indexers", &indexers.to_string())
            .await
            .unwrap();

        seed_target(&state, "t1", "query").await;
        let run = domain::create_search_run(&state.db, "t1").await.unwrap();
        let job = jobs::enqueue(
            &state.db,
            "search_run",
            Some(&SearchRunPayload {
                run_id: run.id.clone(),
                target_id: "t1".into(),
            }),
            None,
        )
        .await
        .unwrap();
        handle(&state, &job).await.unwrap();

        let events: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let err_event = events
            .iter()
            .find(|e| e.contains("search.indexer.error"))
            .expect("search.indexer.error should be published");
        assert!(
            err_event.contains("apikey=REDACTED"),
            "api key must be redacted: {err_event}"
        );
        assert!(
            !err_event.contains("SECRET"),
            "plaintext api key leaked: {err_event}"
        );
    }

    #[tokio::test]
    async fn missing_query_fails_run_without_ubuntu_fallback() {
        let (_dir, state) = testutil::test_state().await;
        seed_target(&state, "t1", "").await;
        let run = domain::create_search_run(&state.db, "t1").await.unwrap();
        let job = jobs::enqueue(
            &state.db,
            "search_run",
            Some(&SearchRunPayload {
                run_id: run.id.clone(),
                target_id: "t1".into(),
            }),
            None,
        )
        .await
        .unwrap();

        let err = handle(&state, &job).await.unwrap_err();
        assert_eq!(err, "target_query_missing");

        let (updated, _) = domain::get_run_with_results(&state.db, &run.id)
            .await
            .unwrap();
        assert_eq!(updated.status, "failed");
        assert_eq!(updated.error.as_deref(), Some("target_query_missing"));
    }

    #[tokio::test]
    async fn invalid_payload_rejected() {
        let (_dir, state) = testutil::test_state().await;
        let job = jobs::enqueue::<serde_json::Value>(&state.db, "search_run", None, None)
            .await
            .unwrap();
        assert_eq!(handle(&state, &job).await.unwrap_err(), "missing_payload");

        let job = jobs::enqueue(
            &state.db,
            "search_run",
            Some(&serde_json::json!({"runId": "", "targetId": ""})),
            None,
        )
        .await
        .unwrap();
        assert_eq!(handle(&state, &job).await.unwrap_err(), "invalid_payload");
    }

    #[test]
    fn ranking_prefers_score_then_seeders_then_title() {
        let mk = |candidate_id: &str, score: i64, seeders: i64, title: &str| ScoredCandidate {
            candidate_id: candidate_id.into(),
            score,
            seeders,
            title: title.into(),
        };
        // Higher score wins.
        assert!(better(&mk("a", 40, 1, "z"), &mk("b", 30, 999, "a")));
        // Tie: more seeders win.
        assert!(better(&mk("a", 30, 50, "z"), &mk("b", 30, 10, "a")));
        // Tie: lexicographically smaller title wins.
        assert!(better(&mk("a", 30, 50, "alpha"), &mk("b", 30, 50, "beta")));
        assert!(!better(&mk("b", 30, 50, "beta"), &mk("a", 30, 50, "alpha")));
    }
}
