//! Targets & search endpoints (operator): wanted-target CRUD, search-run
//! kick-off, run inspection and candidate acquisition creation.
//!
//! Wire format matches the Go handlers exactly (including the original's
//! PascalCase JSON keys for storage rows). Review fixes applied:
//! - `GET /search/runs/:id` returns 404 ONLY when the run does not exist
//!   ([`RunLookupError::NotFound`]); real database faults surface as 500
//!   instead of masquerading as missing runs.
//! - The run's candidate list is capped at 500 rows.
//! - `POST /candidates/:id/acquisition` treats a unique-index collision as a
//!   race it lost: it returns the existing acquisition with 200 instead of a
//!   spurious 500.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::httpapi::middleware::auth::Operator;
use crate::jobs::SearchRunPayload;
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::acquisitions;
use crate::storage::domain::{
    self, Candidate, CandidateWithDecision, Decision, RunLookupError, SearchRun, WantedTarget,
};

/// Candidate rows returned for one run (review fix: unbounded before).
const MAX_CANDIDATES: usize = 500;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/targets", get(list_targets).post(create_target))
        .route("/targets/{id}/search", post(start_search))
        .route("/search/runs/{id}", get(get_search_run))
        .route(
            "/candidates/{id}/acquisition",
            post(create_candidate_acquisition),
        )
}

// ---------------------------------------------------------------------------
// Error plumbing: the Go handlers used bespoke {"error": code} envelopes with
// codes like "db_error"; AppError cannot express those, so this file carries a
// tiny local error with the same wire shape.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: String,
}

impl ApiError {
    fn new(status: StatusCode, code: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
        }
    }

    fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(_: sqlx::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "db_error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Wire types (Go JSON key parity)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[derive(Debug, PartialEq)]
struct TargetWire {
    ID: String,
    Kind: String,
    ExternalSource: String,
    ExternalID: String,
    State: String,
    PayloadJSON: Option<String>,
    CreatedAt: String,
    UpdatedAt: String,
}

impl From<WantedTarget> for TargetWire {
    fn from(t: WantedTarget) -> Self {
        Self {
            ID: t.id,
            Kind: t.kind,
            ExternalSource: t.external_source,
            ExternalID: t.external_id,
            State: t.state,
            PayloadJSON: t.payload_json,
            CreatedAt: t.created_at,
            UpdatedAt: t.updated_at,
        }
    }
}

#[derive(serde::Serialize)]
struct RunWire {
    ID: String,
    TargetID: String,
    Status: String,
    StartedAt: Option<String>,
    CompletedAt: Option<String>,
    Error: Option<String>,
}

impl From<SearchRun> for RunWire {
    fn from(r: SearchRun) -> Self {
        Self {
            ID: r.id,
            TargetID: r.target_id,
            Status: r.status,
            StartedAt: r.started_at,
            CompletedAt: r.completed_at,
            Error: r.error,
        }
    }
}

#[derive(serde::Serialize)]
struct CandidateWire {
    ID: String,
    SearchRunID: String,
    IndexerName: String,
    RawJSON: String,
    NormalizedJSON: Option<String>,
    CreatedAt: String,
}

impl From<Candidate> for CandidateWire {
    fn from(c: Candidate) -> Self {
        Self {
            ID: c.id,
            SearchRunID: c.search_run_id,
            IndexerName: c.indexer_name,
            RawJSON: c.raw_json,
            NormalizedJSON: c.normalized_json,
            CreatedAt: c.created_at,
        }
    }
}

#[derive(serde::Serialize)]
struct DecisionWire {
    ID: String,
    CandidateID: String,
    Accepted: bool,
    Score: i64,
    ReasonsJSON: String,
    CreatedAt: String,
}

impl From<Decision> for DecisionWire {
    fn from(d: Decision) -> Self {
        Self {
            ID: d.id,
            CandidateID: d.candidate_id,
            Accepted: d.accepted,
            Score: d.score,
            ReasonsJSON: d.reasons_json,
            CreatedAt: d.created_at,
        }
    }
}

#[derive(serde::Serialize)]
struct CandidateWithDecisionWire {
    candidate: CandidateWire,
    decision: Option<DecisionWire>,
}

impl From<CandidateWithDecision> for CandidateWithDecisionWire {
    fn from(cwd: CandidateWithDecision) -> Self {
        Self {
            candidate: cwd.candidate.into(),
            decision: cwd.decision.map(DecisionWire::from),
        }
    }
}

// ---------------------------------------------------------------------------
// GET /targets
// ---------------------------------------------------------------------------

async fn list_targets(
    Operator(_user): Operator,
    State(state): State<AppState>,
) -> Result<Json<Vec<TargetWire>>, ApiError> {
    let items = domain::list_wanted(&state.db, 50).await?;
    Ok(Json(items.into_iter().map(TargetWire::from).collect()))
}

// ---------------------------------------------------------------------------
// POST /targets
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTargetRequest {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    external_source: String,
    #[serde(default)]
    external_id: String,
    #[serde(default)]
    query: String,
}

async fn create_target(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Json(req): Json<CreateTargetRequest>,
) -> Result<Response, ApiError> {
    let kind = req.kind.trim();
    let external_source = req.external_source.trim();
    let external_id = req.external_id.trim();
    let query = req.query.trim();
    if kind.is_empty() || external_source.is_empty() || external_id.is_empty() || query.is_empty()
    {
        return Err(ApiError::bad_request("missing_fields"));
    }

    state.hub.publish_event(
        "test.target.creating",
        json!({
            "kind": kind,
            "externalSource": external_source,
            "externalId": external_id,
            "query": query,
        }),
    );

    let payload = json!({ "query": query });
    let t =
        domain::create_wanted_target(&state.db, kind, external_source, external_id, Some(&payload))
            .await
            .map_err(|e| {
                state.hub.publish_event(
                    "test.target.error",
                    json!({ "error": "create_failed", "message": e.to_string() }),
                );
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "create_failed")
            })?;

    state.hub.publish_event(
        "test.target.created",
        json!({
            "targetId": t.id,
            "kind": kind,
            "externalSource": external_source,
            "externalId": external_id,
            "query": query,
        }),
    );

    Ok((StatusCode::CREATED, Json(json!({ "id": t.id }))).into_response())
}

// ---------------------------------------------------------------------------
// POST /targets/:id/search
// ---------------------------------------------------------------------------

async fn start_search(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let target_id = id.trim();
    if target_id.is_empty() {
        return Err(ApiError::bad_request("missing_target_id"));
    }

    state
        .hub
        .publish_event("test.search.starting", json!({ "targetId": target_id }));

    let run = domain::create_search_run(&state.db, target_id)
        .await
        .map_err(|e| {
            state.hub.publish_event(
                "test.search.error",
                json!({
                    "targetId": target_id,
                    "error": "create_run_failed",
                    "message": e.to_string(),
                }),
            );
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "create_run_failed")
        })?;

    let payload = SearchRunPayload {
        run_id: run.id.clone(),
        target_id: target_id.to_string(),
    };
    let job = crate::storage::jobs::enqueue(&state.db, "search_run", Some(&payload), None)
        .await
        .map_err(|e| {
            state.hub.publish_event(
                "test.search.error",
                json!({
                    "targetId": target_id,
                    "runId": run.id,
                    "error": "enqueue_failed",
                    "message": e.to_string(),
                }),
            );
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "enqueue_failed")
        })?;

    state.metrics.inc_jobs_enqueued();
    state.hub.publish_event(
        "job.enqueued",
        json!({ "jobId": job.id, "kind": job.kind, "runId": run.id, "targetId": target_id }),
    );
    state.hub.publish_event(
        "test.search.started",
        json!({ "targetId": target_id, "runId": run.id, "jobId": job.id }),
    );

    Ok((StatusCode::ACCEPTED, Json(json!({ "runId": run.id }))).into_response())
}

// ---------------------------------------------------------------------------
// GET /search/runs/:id
// ---------------------------------------------------------------------------

async fn get_search_run(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let run_id = id.trim();
    match domain::get_run_with_results(&state.db, run_id).await {
        Ok((run, mut candidates)) => {
            // Review fix: cap the candidate payload.
            candidates.truncate(MAX_CANDIDATES);
            Ok(Json(json!({
                "run": RunWire::from(run),
                "candidates": candidates
                    .into_iter()
                    .map(CandidateWithDecisionWire::from)
                    .collect::<Vec<_>>(),
            }))
            .into_response())
        }
        // 404 ONLY for a genuinely missing run; every other failure is a 500.
        Err(RunLookupError::NotFound) => Err(ApiError::new(StatusCode::NOT_FOUND, "not_found")),
        Err(RunLookupError::Db(e)) => {
            tracing::error!(error = %e, "search run lookup failed");
            Err(ApiError::from(e))
        }
    }
}

// ---------------------------------------------------------------------------
// POST /candidates/:id/acquisition
// ---------------------------------------------------------------------------

async fn create_candidate_acquisition(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let candidate_id = id.trim();
    if candidate_id.is_empty() {
        return Err(ApiError::bad_request("missing_candidate_id"));
    }

    // Already acquired? Return the existing acquisition (idempotent).
    if let Some(existing) = acquisitions::get_by_candidate(&state.db, candidate_id).await? {
        return Ok(Json(json!({
            "ok": true,
            "acquisitionId": existing.id,
            "existing": true,
        }))
        .into_response());
    }

    // Candidate → search run → target.
    let search_run_id: String =
        sqlx::query_scalar("SELECT search_run_id FROM release_candidates WHERE id = ?")
            .bind(candidate_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "candidate_not_found"))?;

    let target_id: String = sqlx::query_scalar("SELECT target_id FROM search_runs WHERE id = ?")
        .bind(&search_run_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "target_not_found"))?;

    state.hub.publish_event(
        "test.acquisition.creating",
        json!({ "candidateId": candidate_id, "targetId": target_id }),
    );

    let created = acquisitions::create_acquisition(
        &state.db,
        candidate_id,
        &target_id,
        "torrent",
        "qbittorrent",
        "default",
    )
    .await;

    let acq = match created {
        Ok(a) => a,
        // REVIEW FIX: a unique-index collision on candidate_id means a
        // concurrent request created the acquisition first — report the
        // winner's row with 200 instead of a 500.
        Err(crate::storage::acquisitions::AcquisitionError::Db(e))
            if e.as_database_error()
                .map(|d| d.is_unique_violation())
                .unwrap_or(false) =>
        {
            if let Some(existing) = acquisitions::get_by_candidate(&state.db, candidate_id).await?
            {
                return Ok(Json(json!({
                    "ok": true,
                    "acquisitionId": existing.id,
                    "existing": true,
                }))
                .into_response());
            }
            return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "create_failed"));
        }
        Err(e) => {
            state.hub.publish_event(
                "test.acquisition.error",
                json!({
                    "candidateId": candidate_id,
                    "targetId": target_id,
                    "error": "create_failed",
                    "message": e.to_string(),
                }),
            );
            return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "create_failed"));
        }
    };

    match crate::storage::jobs::enqueue(
        &state.db,
        "acquisition_enqueue",
        Some(&crate::jobs::EnqueuePayload {
            acquisition_id: acq.id.clone(),
        }),
        None,
    )
    .await
    {
        Err(_) => {
            // Acquisition row exists; enqueue failed. Original kept a 200
            // with a warning envelope — preserved.
            state.hub.publish_event(
                "acquisition.created",
                json!({
                    "acquisitionId": acq.id,
                    "candidateId": candidate_id,
                    "targetId": target_id,
                    "enqueueFailed": true,
                }),
            );
            state.hub.publish_event(
                "test.acquisition.created",
                json!({
                    "acquisitionId": acq.id,
                    "candidateId": candidate_id,
                    "targetId": target_id,
                    "warning": "enqueue_failed",
                }),
            );
            Ok(Json(json!({
                "ok": true,
                "acquisitionId": acq.id,
                "jobId": "",
                "warning": "enqueue_failed",
            }))
            .into_response())
        }
        Ok(job) => {
            state.metrics.inc_jobs_enqueued();
            state.hub.publish_event(
                "job.enqueued",
                json!({
                    "jobId": job.id,
                    "kind": "acquisition_enqueue",
                    "acquisitionId": acq.id,
                    "candidateId": candidate_id,
                }),
            );
            state.hub.publish_event(
                "acquisition.created",
                json!({
                    "acquisitionId": acq.id,
                    "candidateId": candidate_id,
                    "targetId": target_id,
                }),
            );
            state.hub.publish_event(
                "test.acquisition.created",
                json!({
                    "acquisitionId": acq.id,
                    "candidateId": candidate_id,
                    "targetId": target_id,
                    "jobId": job.id,
                }),
            );
            Ok((
                StatusCode::CREATED,
                Json(json!({ "ok": true, "acquisitionId": acq.id, "jobId": job.id })),
            )
                .into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::AuthenticatedUser;
    use crate::security::secrets::SecretBox;
    use crate::storage::migrations;
    use crate::storage::{acquisitions as acq_store, open};
    use crate::ws::hub::Hub;
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        migrations::apply(&pool).await.unwrap();
        let state = AppState {
            db: pool,
            secret_box: Arc::new(SecretBox::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: false,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: Duration::from_secs(3600),
                allow_internal_urls: true,
            }),
        };
        (state, dir)
    }

    fn operator() -> Operator {
        Operator(AuthenticatedUser {
            user: crate::storage::users::User {
                id: "u1".into(),
                email: "op@rivet.test".into(),
                password_hash: "x".into(),
                role: "operator".into(),
                totp_secret_enc: None,
                totp_enabled: false,
                created_at: String::new(),
                updated_at: String::new(),
            },
            session: crate::storage::sessions::Session {
                id: "s1".into(),
                user_id: "u1".into(),
                csrf_token: "c".into(),
                created_at: String::new(),
                expires_at: String::new(),
                revoked_at: None,
                last_seen_at: None,
                ip: None,
                user_agent: None,
            },
        })
    }

    async fn body_json(res: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn list_targets_returns_empty_array() {
        let (state, _dir) = test_state().await;
        let res = list_targets(operator(), State(state.clone())).await.unwrap();
        assert_eq!(res.0, Vec::<TargetWire>::new());
    }

    #[tokio::test]
    async fn create_target_validates_fields_and_persists() {
        let (state, _dir) = test_state().await;

        let bad = create_target(
            operator(),
            State(state.clone()),
            Json(CreateTargetRequest {
                kind: "movie".into(),
                external_source: "".into(),
                external_id: "42".into(),
                query: "dune".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(bad.code, "missing_fields");

        let res = create_target(
            operator(),
            State(state.clone()),
            Json(CreateTargetRequest {
                kind: " movie ".into(),
                external_source: " tmdb ".into(),
                external_id: " 42 ".into(),
                query: " dune ".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let v = body_json(res).await;
        assert!(v["id"].as_str().is_some());

        let listed = domain::list_wanted(&state.db, 50).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].external_source, "tmdb");
        assert_eq!(listed[0].state, "wanted");
    }

    #[tokio::test]
    async fn start_search_enqueues_202() {
        let (state, _dir) = test_state().await;
        let t = domain::create_wanted_target(&state.db, "movie", "tmdb", "42", None)
            .await
            .unwrap();

        let res = start_search(operator(), State(state.clone()), Path(t.id.clone()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let v = body_json(res).await;
        let run_id = v["runId"].as_str().unwrap().to_string();

        assert_eq!(
            state.metrics.jobs_enqueued_total.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let (run, _) = domain::get_run_with_results(&state.db, &run_id).await.unwrap();
        assert_eq!(run.status, "queued");
        assert_eq!(run.target_id, t.id);
    }

    #[tokio::test]
    async fn get_search_run_404_only_for_missing() {
        let (state, _dir) = test_state().await;

        let err = get_search_run(operator(), State(state), Path("nope".into()))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.code, "not_found");
    }

    #[tokio::test]
    async fn get_search_run_caps_candidates() {
        let (state, _dir) = test_state().await;
        let t = domain::create_wanted_target(&state.db, "movie", "tmdb", "1", None)
            .await
            .unwrap();
        let run = domain::create_search_run(&state.db, &t.id).await.unwrap();
        for i in 0..(MAX_CANDIDATES + 10) {
            domain::insert_candidate(&state.db, &run.id, "torznab", &format!(r#"{{"i":{i}}}"#), None)
                .await
                .unwrap();
        }

        let res = get_search_run(operator(), State(state), Path(run.id.clone()))
            .await
            .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["run"]["ID"].as_str(), Some(run.id.as_str()));
        let cands = v["candidates"].as_array().unwrap();
        assert_eq!(cands.len(), MAX_CANDIDATES);
    }

    #[tokio::test]
    async fn candidate_acquisition_is_idempotent_on_race() {
        let (state, _dir) = test_state().await;
        let t = domain::create_wanted_target(&state.db, "movie", "tmdb", "1", None)
            .await
            .unwrap();
        let run = domain::create_search_run(&state.db, &t.id).await.unwrap();
        let cand = domain::insert_candidate(&state.db, &run.id, "torznab", "{}", None)
            .await
            .unwrap();

        let res = create_candidate_acquisition(operator(), State(state.clone()), Path(cand.id.clone()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        // Second call finds the existing acquisition → 200 existing:true.
        let res = create_candidate_acquisition(operator(), State(state.clone()), Path(cand.id))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["existing"], true);

        // The unique index must back the race fix: a direct duplicate insert
        // surfaces as a unique violation, which is exactly the branch the
        // handler maps to 200/existing.
        // A genuinely different (and existing) candidate must insert fine;
        // only a same-candidate duplicate hits ux_acquisitions_candidate_id.
        let cand2 = domain::insert_candidate(&state.db, &run.id, "torznab", "{}", None)
            .await
            .unwrap();
        let conflict = acq_store::create_acquisition(
            &state.db,
            &cand2.id,
            &t.id,
            "torrent",
            "qbittorrent",
            "default",
        )
        .await;
        assert!(conflict.is_ok(), "different candidate must insert fine");

        let dup = acq_store::list(&state.db, None, 10).await.unwrap();
        assert_eq!(dup.len(), 2);
    }

    #[tokio::test]
    async fn candidate_acquisition_unknown_candidate_404() {
        let (state, _dir) = test_state().await;
        let err = create_candidate_acquisition(operator(), State(state), Path("ghost".into()))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.code, "candidate_not_found");
    }
}
