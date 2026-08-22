//! Acquisition endpoints: listing/detail (operator) and lifecycle actions.
//!
//! Role split matches the original: pause/resume/retry are operator actions;
//! remove, reverify, reimport, accept_verified, override_imported and the
//! blacklist pair are admin-only.
//!
//! Review fixes applied:
//! - `accept_verified` checks the import-enqueue result and returns 500
//!   `enqueue_failed` instead of silently reporting success with an empty job.
//! - Blacklist inserts rely on `storage::blacklist` INSERT OR IGNORE semantics
//!   (unique index `(scope, COALESCE(target_id,''), match_type, match_value)`),
//!   so repeated actions no longer accumulate duplicate rows.
//! - Download-client operations go through the adapter registry
//!   ([`crate::adapters::downloaders::build_torrent_client`]), which routes all
//!   outbound traffic through the SSRF-pinned transport.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::adapters::Downloader;
use crate::httpapi::middleware::auth::{Admin, Operator};
use crate::state::AppState;
use crate::storage::acquisitions::{self, Acquisition};
use crate::storage::audit::{self, AuditEvent};
use crate::storage::settings;
use crate::ws::hub::PublishExt;

/// Settings key holding the wizard-persisted download-client credentials.
const CLIENTS_SETTING: &str = "setup.clients";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/acquisitions", get(list_acquisitions_handler))
        .route("/acquisitions/{id}", get(get_acquisition))
        .route("/acquisitions/{id}/actions/pause", post(pause_acquisition))
        .route(
            "/acquisitions/{id}/actions/resume",
            post(resume_acquisition),
        )
        .route("/acquisitions/{id}/actions/retry", post(retry_acquisition))
        .route(
            "/acquisitions/{id}/actions/remove",
            post(remove_acquisition),
        )
        .route(
            "/acquisitions/{id}/actions/reverify",
            post(reverify_acquisition),
        )
        .route(
            "/acquisitions/{id}/actions/reimport",
            post(reimport_acquisition),
        )
        .route(
            "/acquisitions/{id}/actions/accept_verified",
            post(accept_verified),
        )
        .route(
            "/acquisitions/{id}/actions/override_imported",
            post(override_imported),
        )
        .route("/acquisitions/{id}/actions/blacklist", post(blacklist))
        .route(
            "/acquisitions/{id}/actions/blacklist_and_rescout",
            post(blacklist_and_rescout),
        )
}

// ---------------------------------------------------------------------------
// Error plumbing: bespoke {"error": code} envelopes ("db_error", ...) that
// AppError cannot express.
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

impl From<crate::adapters::AdapterError> for ApiError {
    fn from(e: crate::adapters::AdapterError) -> Self {
        // Upstream client failures surface as 502 with the message, matching
        // the original's BadGateway envelope.
        Self::new(StatusCode::BAD_GATEWAY, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Wire type (Go JSON key parity for storage.Acquisition)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Debug)]
struct AcquisitionWire {
    ID: String,
    CandidateID: String,
    TargetID: String,
    Kind: String,
    Status: String,
    ClientKind: String,
    ClientName: String,
    SourceURL: Option<String>,
    ExternalID: Option<String>,
    ContentPath: Option<String>,
    VerifiedPath: Option<String>,
    QuarantinedPath: Option<String>,
    VerifyStatus: Option<String>,
    VerifyEvidenceJSON: Option<String>,
    ImportedPath: Option<String>,
    CompletedAt: Option<String>,
    ImportedAt: Option<String>,
    Progress: Option<f64>,
    DownloadStartedAt: Option<String>,
    ProgressChangedAt: Option<String>,
    StalledAt: Option<String>,
    RecoveryAttempt: i64,
    LastError: Option<String>,
    CreatedAt: String,
    UpdatedAt: String,
}

impl From<Acquisition> for AcquisitionWire {
    fn from(a: Acquisition) -> Self {
        Self {
            ID: a.id,
            CandidateID: a.candidate_id,
            TargetID: a.target_id,
            Kind: a.kind,
            Status: a.status,
            ClientKind: a.client_kind,
            ClientName: a.client_name,
            SourceURL: a.source_url,
            ExternalID: a.external_id,
            ContentPath: a.content_path,
            VerifiedPath: a.verified_path,
            QuarantinedPath: a.quarantined_path,
            VerifyStatus: a.verify_status,
            VerifyEvidenceJSON: a.verify_evidence_json,
            ImportedPath: a.imported_path,
            CompletedAt: a.completed_at,
            ImportedAt: a.imported_at,
            Progress: a.progress,
            DownloadStartedAt: a.download_started_at,
            ProgressChangedAt: a.progress_changed_at,
            StalledAt: a.stalled_at,
            RecoveryAttempt: a.recovery_attempt,
            LastError: a.last_error,
            CreatedAt: a.created_at,
            UpdatedAt: a.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// GET /acquisitions (+ /:id)
// ---------------------------------------------------------------------------

/// Review-fixed clamp: 1..=100, default 50 (the original let out-of-range
/// values fall through to the storage default of 50 inconsistently).
fn clamp_limit(raw: Option<&str>) -> i64 {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.parse::<i64>().unwrap_or(50).clamp(1, 100),
        None => 50,
    }
}

async fn list_acquisitions_handler(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<AcquisitionWire>>, ApiError> {
    let limit = clamp_limit(params.get("limit").map(String::as_str));
    let status = params
        .get("status")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let items = acquisitions::list(&state.db, status, limit).await?;
    Ok(Json(items.into_iter().map(AcquisitionWire::from).collect()))
}

async fn get_acquisition(
    Operator(_user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AcquisitionWire>, ApiError> {
    if id.trim().is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    let item = acquisitions::get(&state.db, id.trim())
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found"))?;
    Ok(Json(item.into()))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn actor_id(user: &crate::httpapi::middleware::auth::AuthenticatedUser) -> Option<String> {
    let id = user.user.id.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Loads the persisted client config for `kind` from `setup.clients`,
/// decrypting `passwordEnc` in place so the adapter receives usable
/// credentials. Returns the enriched config JSON.
async fn load_client_config(state: &AppState, kind: &str) -> Result<serde_json::Value, ApiError> {
    let raw = settings::get(&state.db, CLIENTS_SETTING)
        .await?
        .ok_or_else(|| ApiError::bad_request("no_clients_configured"))?;
    let mut cfg: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| ApiError::bad_request("invalid_clients_config"))?;
    let entry = cfg
        .get_mut(kind)
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| match kind {
            "qbittorrent" => ApiError::bad_request("qbittorrent_not_configured"),
            "deluge" => ApiError::bad_request("deluge_not_configured"),
            "transmission" => ApiError::bad_request("transmission_not_configured"),
            "nzbget" => ApiError::bad_request("nzbget_not_configured"),
            "sabnzbd" => ApiError::bad_request("sabnzbd_not_configured"),
            _ => ApiError::bad_request("client_not_configured"),
        })?;

    let enc = entry
        .get("passwordEnc")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !enc.is_empty() {
        let plain = state
            .secret_box
            .decrypt_string(enc)
            .map_err(|_| ApiError::bad_request("decrypt_failed"))?;
        let plain =
            String::from_utf8(plain).map_err(|_| ApiError::bad_request("decrypt_failed"))?;
        entry.insert("password".to_string(), json!(plain));
        entry.remove("passwordEnc");
    }
    Ok(cfg)
}

/// Builds the torrent downloader for an acquisition's configured client.
async fn build_client(
    state: &AppState,
    kind: &str,
) -> Result<std::sync::Arc<dyn Downloader>, ApiError> {
    let cfg = load_client_config(state, kind).await?;
    crate::adapters::downloaders::build_torrent_client(
        kind,
        &cfg,
        state.policy,
        state.config.dry_run,
        state.config.dry_run_fixture_path.as_deref(),
    )
    .await
    .map_err(|_| ApiError::bad_request("client_init_failed"))
}

/// Loads the acquisition or maps absence to 404.
async fn require_acquisition(state: &AppState, id: &str) -> Result<Acquisition, ApiError> {
    acquisitions::get(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found"))
}

// ---------------------------------------------------------------------------
// POST /acquisitions/:id/actions/{pause,resume}
// ---------------------------------------------------------------------------

async fn pause_acquisition(
    Operator(user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    control_action(&state, id.trim(), "pause", actor_id(&user)).await
}

async fn resume_acquisition(
    Operator(user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    control_action(&state, id.trim(), "resume", actor_id(&user)).await
}

async fn control_action(
    state: &AppState,
    id: &str,
    action: &str,
    actor: Option<String>,
) -> Result<Response, ApiError> {
    if id.is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    let a = require_acquisition(state, id).await?;

    let external_id = a
        .external_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing_external_id"))?
        .to_string();

    let client = build_client(state, &a.client_kind).await?;

    match action {
        "pause" => {
            client.pause(&external_id).await?;
            acquisitions::update_progress(
                &state.db,
                &a.id,
                "paused",
                a.progress,
                a.content_path.as_deref(),
            )
            .await?;
        }
        "resume" => {
            client.resume(&external_id).await?;
            acquisitions::update_progress(
                &state.db,
                &a.id,
                "downloading",
                a.progress,
                a.content_path.as_deref(),
            )
            .await?;
            acquisitions::touch_progress_changed(&state.db, &a.id, Utc::now()).await?;
        }
        _ => return Err(ApiError::bad_request("invalid_action")),
    }

    let action_name: &'static str = match action {
        "pause" => "acquisition.pause",
        "resume" => "acquisition.resume",
        "retry" => "acquisition.retry",
        "remove" => "acquisition.remove",
        "reverify" => "acquisition.reverify",
        "reimport" => "acquisition.reimport",
        "accept_verified" => "acquisition.accept_verified",
        "override_imported" => "acquisition.override_imported",
        "blacklist" => "acquisition.blacklist",
        "blacklist_and_rescout" => "acquisition.blacklist_and_rescout",
        _ => "acquisition.control",
    };
    record_audit(state, actor, action_name, &a.id, None).await;
    state.hub.publish_event(
        "download.control",
        json!({ "acquisitionId": a.id, "action": action }),
    );

    Ok(Json(json!({ "ok": true })).into_response())
}

// ---------------------------------------------------------------------------
// POST /acquisitions/:id/actions/remove (admin)
// ---------------------------------------------------------------------------

async fn remove_acquisition(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    let delete_files = params
        .get("deleteFiles")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);

    let a = require_acquisition(&state, id).await?;
    let external_id = a
        .external_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing_external_id"))?
        .to_string();

    let client = build_client(&state, &a.client_kind).await?;
    client.delete(&external_id, delete_files).await?;

    acquisitions::update_progress(
        &state.db,
        &a.id,
        "removed",
        a.progress,
        a.content_path.as_deref(),
    )
    .await?;

    record_audit(
        &state,
        actor_id(&user),
        "acquisition.remove",
        &a.id,
        Some(json!({ "deleteFiles": delete_files }).to_string()),
    )
    .await;
    state.hub.publish_event(
        "download.control",
        json!({ "acquisitionId": a.id, "action": "remove", "deleteFiles": delete_files }),
    );

    Ok(Json(json!({ "ok": true })).into_response())
}

// ---------------------------------------------------------------------------
// POST /acquisitions/:id/actions/{retry,reverify,reimport}
// ---------------------------------------------------------------------------

async fn retry_acquisition(
    Operator(user): Operator,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    enqueue_job_action(
        &state,
        id.trim(),
        "acquisition_enqueue",
        "acquisition.retry",
        actor_id(&user),
    )
    .await
}

async fn reverify_acquisition(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    enqueue_job_action(
        &state,
        id.trim(),
        "acquisition_verify",
        "acquisition.reverify",
        actor_id(&user),
    )
    .await
}

async fn reimport_acquisition(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    enqueue_job_action(
        &state,
        id.trim(),
        "acquisition_import",
        "acquisition.reimport",
        actor_id(&user),
    )
    .await
}

async fn enqueue_job_action(
    state: &AppState,
    id: &str,
    job_kind: &str,
    audit_action: &str,
    actor: Option<String>,
) -> Result<Response, ApiError> {
    if id.is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    require_acquisition(state, id).await?;

    let job = crate::storage::jobs::enqueue(
        &state.db,
        job_kind,
        Some(&crate::jobs::EnqueuePayload {
            acquisition_id: id.to_string(),
        }),
        Some(Utc::now() + chrono::Duration::seconds(1)),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "enqueue_failed"))?;

    state.hub.publish_event(
        "job.enqueued",
        json!({ "jobId": job.id, "kind": job.kind, "acquisitionId": id }),
    );

    record_audit(state, actor, audit_action, id, None).await;

    Ok(Json(json!({ "ok": true, "jobId": job.id })).into_response())
}

// ---------------------------------------------------------------------------
// POST /acquisitions/:id/actions/accept_verified (admin)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct AcceptVerifiedRequest {
    #[serde(default, rename = "verifiedPath")]
    verified_path: String,
}

async fn accept_verified(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    let a = require_acquisition(&state, id).await?;

    // Body is optional; a malformed body falls back to the stored paths just
    // like the original's ignored BodyParser error.
    let requested = serde_json::from_slice::<AcceptVerifiedRequest>(&body)
        .map(|r| r.verified_path.trim().to_string())
        .unwrap_or_default();

    // Best available local path wins (admin should know what they're doing).
    let verified_path = [
        Some(requested),
        a.quarantined_path.clone(),
        a.verified_path.clone(),
        a.content_path.clone(),
    ]
    .into_iter()
    .flatten()
    .map(|p| p.trim().to_string())
    .find(|p| !p.is_empty())
    .ok_or_else(|| ApiError::bad_request("missing_verified_path"))?;

    acquisitions::admin_accept_verified(&state.db, &a.id, &verified_path)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "update_failed"))?;

    // REVIEW FIX: the original discarded the enqueue error (`job, _ :=`),
    // reporting success while nothing would ever import. Propagate it.
    let job = crate::storage::jobs::enqueue(
        &state.db,
        "acquisition_import",
        Some(&crate::jobs::ImportPayload {
            acquisition_id: a.id.clone(),
        }),
        Some(Utc::now() + chrono::Duration::seconds(1)),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "enqueue_failed"))?;

    let actor = actor_id(&user);
    record_audit(
        &state,
        actor.clone(),
        "acquisition.accept_verified",
        &a.id,
        Some(json!({ "verifiedPath": verified_path, "jobId": job.id }).to_string()),
    )
    .await;
    state.hub.publish_event(
        "verify.completed",
        json!({
            "acquisitionId": a.id,
            "verifiedPath": verified_path,
            "overridden": true,
        }),
    );
    state.hub.publish_event(
        "job.enqueued",
        json!({ "jobId": job.id, "kind": job.kind, "acquisitionId": a.id }),
    );

    Ok(Json(json!({ "ok": true, "jobId": job.id })).into_response())
}

// ---------------------------------------------------------------------------
// POST /acquisitions/:id/actions/override_imported (admin)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OverrideImportedRequest {
    #[serde(default, rename = "importedPath")]
    imported_path: String,
}

async fn override_imported(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    let req: OverrideImportedRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalid_json"))?;
    let imported_path = req.imported_path.trim();
    if imported_path.is_empty() {
        return Err(ApiError::bad_request("missing_imported_path"));
    }
    require_acquisition(&state, id).await?;

    acquisitions::mark_imported(&state.db, id, imported_path)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "update_failed"))?;

    record_audit(
        &state,
        actor_id(&user),
        "acquisition.override_imported",
        id,
        Some(json!({ "importedPath": imported_path }).to_string()),
    )
    .await;
    state.hub.publish_event(
        "import.completed",
        json!({ "acquisitionId": id, "importedPath": imported_path, "overridden": true }),
    );

    Ok(Json(json!({ "ok": true })).into_response())
}

// ---------------------------------------------------------------------------
// POST /acquisitions/:id/actions/{blacklist,blacklist_and_rescout} (admin)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct BlacklistRequest {
    /// "target" (default) or "global".
    #[serde(default)]
    scope: String,
    #[serde(default)]
    reason: String,
}

async fn blacklist(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let req = serde_json::from_slice::<BlacklistRequest>(&body).unwrap_or_default();
    blacklist_action(&state, id.trim(), req, false, actor_id(&user)).await
}

async fn blacklist_and_rescout(
    Admin(user): Admin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let req = serde_json::from_slice::<BlacklistRequest>(&body).unwrap_or_default();
    blacklist_action(&state, id.trim(), req, true, actor_id(&user)).await
}

async fn blacklist_action(
    state: &AppState,
    id: &str,
    req: BlacklistRequest,
    also_rescout: bool,
    actor: Option<String>,
) -> Result<Response, ApiError> {
    if id.is_empty() {
        return Err(ApiError::bad_request("invalid_id"));
    }
    let a = require_acquisition(state, id).await?;

    let scope = {
        let s = req.scope.trim().to_ascii_lowercase();
        if s.is_empty() {
            "target".to_string()
        } else {
            s
        }
    };
    let reason = req.reason.trim();
    let reason_opt = if reason.is_empty() {
        None
    } else {
        Some(reason)
    };

    // Resolve candidate URL/title for extra matching.
    let (cand_url, cand_title) = load_candidate_url_and_title(&state.db, &a.candidate_id).await;

    let mut hash = a
        .external_id
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if hash.is_empty() {
        hash = extract_btih(&cand_url);
    }

    let target_scope: Option<&str> = if scope == "target" && !a.target_id.trim().is_empty() {
        Some(a.target_id.trim())
    } else {
        None
    };

    let mut created: u32 = 0;
    let mut matches: Vec<serde_json::Value> = Vec::new();

    // Prefer hash blacklist (most stable).
    for (match_type, value) in [
        ("hash", hash.as_str()),
        ("url", cand_url.as_str()),
        ("title", cand_title.as_str()),
    ] {
        if value.is_empty() {
            continue;
        }
        // INSERT OR IGNORE under the unique index: duplicates are skipped and
        // NOT counted (review fix for accumulating rows).
        if let Ok((_, inserted)) = crate::storage::blacklist::insert(
            &state.db,
            &scope,
            target_scope,
            match_type,
            value,
            reason_opt,
            actor.as_deref(),
        )
        .await
        {
            if inserted {
                created += 1;
                matches.push(json!({ "type": match_type, "value": value }));
            }
        }
    }

    if created == 0 {
        return Err(ApiError::bad_request("no_match_to_blacklist"));
    }

    let action = if also_rescout {
        "acquisition.blacklist_and_rescout"
    } else {
        "acquisition.blacklist"
    };
    record_audit(
        state,
        actor,
        action,
        &a.id,
        Some(
            json!({
                "scope": scope,
                "reason": reason,
                "matches": matches,
                "targetId": a.target_id,
            })
            .to_string(),
        ),
    )
    .await;
    state.hub.publish_event(
        "download.blacklisted",
        json!({ "acquisitionId": a.id, "scope": scope, "matches": matches }),
    );

    let mut job_id: Option<String> = None;
    if also_rescout {
        let rescout_payload = crate::jobs::RescoutPayload {
            target_id: a.target_id.clone(),
            failed_acquisition_id: Some(a.id.clone()),
        };
        let job = crate::storage::jobs::enqueue(
            &state.db,
            "acquisition_rescout",
            Some(&rescout_payload),
            Some(Utc::now() + chrono::Duration::seconds(1)),
        );
        if let Ok(job) = job.await {
            job_id = Some(job.id.clone());
            state.hub.publish_event(
                "download.rescout.scheduled",
                json!({ "acquisitionId": a.id, "jobId": job.id }),
            );
        }
    }

    Ok(Json(json!({ "ok": true, "created": created, "jobId": job_id })).into_response())
}

/// Reads `enclosureUrl` (falling back to `link`) and `title` from the
/// candidate's raw JSON. Missing candidate or malformed JSON yields empties.
async fn load_candidate_url_and_title(
    pool: &sqlx::SqlitePool,
    candidate_id: &str,
) -> (String, String) {
    let candidate_id = candidate_id.trim();
    if candidate_id.is_empty() {
        return (String::new(), String::new());
    }
    let Ok(Some(raw)) =
        sqlx::query_scalar::<_, String>("SELECT raw_json FROM release_candidates WHERE id = ?")
            .bind(candidate_id)
            .fetch_optional(pool)
            .await
    else {
        return (String::new(), String::new());
    };
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let pick = |key: &str| {
        parsed
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let url = pick("enclosureUrl");
    let url = if url.is_empty() { pick("link") } else { url };
    (url, pick("title"))
}

/// Extracts the btih info-hash from a magnet link; empty for other URLs.
fn extract_btih(u: &str) -> String {
    let u = u.trim();
    if !u.to_ascii_lowercase().starts_with("magnet:?") {
        return String::new();
    }
    let lower = u.to_ascii_lowercase();
    let marker = "xt=urn:btih:";
    let idx = match lower.find(marker) {
        Some(i) => i + marker.len(),
        None => return String::new(),
    };
    let end = u[idx..]
        .find(['&', '#'])
        .map(|i| idx + i)
        .unwrap_or(u.len());
    u[idx..end].trim().to_string()
}

// ---------------------------------------------------------------------------
// Audit helper
// ---------------------------------------------------------------------------

async fn record_audit(
    state: &AppState,
    actor_user_id: Option<String>,
    action: &str,
    target_id: &str,
    details_json: Option<String>,
) {
    let _ = audit::insert(
        &state.db,
        AuditEvent {
            actor_user_id,
            action: action.to_string(),
            target_type: Some("acquisition".to_string()),
            target_id: Some(target_id.to_string()),
            details_json,
            ..AuditEvent::default()
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::AuthenticatedUser;
    use crate::security::secrets::SecretBox as Box2;
    use crate::storage::domain;
    use crate::storage::migrations;
    use crate::ws::hub::Hub;
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        migrations::apply(&pool).await.unwrap();
        let state = AppState {
            db: pool,
            secret_box: Arc::new(Box2::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: true,
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

    fn principal(role: &str) -> AuthenticatedUser {
        AuthenticatedUser {
            user: crate::storage::users::User {
                id: "u1".into(),
                email: "op@rivet.test".into(),
                password_hash: "x".into(),
                role: role.into(),
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
        }
    }

    async fn seed_acquisition(state: &AppState) -> (Acquisition, domain::SearchRun, String) {
        let t = domain::create_wanted_target(&state.db, "movie", "tmdb", "1", None)
            .await
            .unwrap();
        let run = domain::create_search_run(&state.db, &t.id).await.unwrap();
        let cand = domain::insert_candidate(&state.db, &run.id, "torznab", "{}", None)
            .await
            .unwrap();
        let a = acquisitions::create_acquisition(
            &state.db,
            &cand.id,
            &t.id,
            "torrent",
            "qbittorrent",
            "default",
        )
        .await
        .unwrap();
        (a, run, cand.id)
    }

    async fn body_json(res: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn limit_clamps_to_1_100() {
        assert_eq!(clamp_limit(None), 50);
        assert_eq!(clamp_limit(Some("")), 50);
        assert_eq!(clamp_limit(Some("abc")), 50);
        assert_eq!(clamp_limit(Some("0")), 1);
        assert_eq!(clamp_limit(Some("-5")), 1);
        assert_eq!(clamp_limit(Some("500")), 100);
        assert_eq!(clamp_limit(Some("25")), 25);
    }

    #[tokio::test]
    async fn list_respects_clamped_limit() {
        let (state, _dir) = test_state().await;
        for _ in 0..3 {
            seed_acquisition(&state).await;
        }
        let res = list_acquisitions_handler(
            Operator(principal("operator")),
            State(state.clone()),
            Query(HashMap::from([("limit".to_string(), "2".to_string())])),
        )
        .await
        .unwrap();
        assert_eq!(res.0.len(), 2);

        // Out-of-range high values clamp to 100, not the storage default 50.
        let res = list_acquisitions_handler(
            Operator(principal("operator")),
            State(state.clone()),
            Query(HashMap::from([("limit".to_string(), "9999".to_string())])),
        )
        .await
        .unwrap();
        assert_eq!(res.0.len(), 3);

        // Zero/negative clamp to 1.
        let res = list_acquisitions_handler(
            Operator(principal("operator")),
            State(state.clone()),
            Query(HashMap::from([("limit".to_string(), "0".to_string())])),
        )
        .await
        .unwrap();
        assert_eq!(res.0.len(), 1);
    }

    #[tokio::test]
    async fn get_acquisition_404_for_missing() {
        let (state, _dir) = test_state().await;
        let err = get_acquisition(
            Operator(principal("operator")),
            State(state),
            Path("ghost".into()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn accept_verified_falls_back_through_paths() {
        let (state, _dir) = test_state().await;
        let (a, _, _) = seed_acquisition(&state).await;
        acquisitions::set_verify_status(
            &state.db,
            &a.id,
            "needs_review",
            None,
            Some("/data/quarantine/x.nzb"),
            None,
        )
        .await
        .unwrap();

        // No body: falls back to quarantined_path.
        let res = accept_verified(
            Admin(principal("admin")),
            State(state.clone()),
            Path(a.id.clone()),
            axum::body::Bytes::new(),
        )
        .await
        .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["ok"], true);
        assert!(v["jobId"].as_str().is_some());

        let updated = acquisitions::get(&state.db, &a.id).await.unwrap().unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.verify_status.as_deref(), Some("passed"));
        assert_eq!(
            updated.verified_path.as_deref(),
            Some("/data/quarantine/x.nzb")
        );
    }

    #[tokio::test]
    async fn accept_verified_missing_every_path_400() {
        let (state, _dir) = test_state().await;
        let (a, _, _) = seed_acquisition(&state).await;
        let err = accept_verified(
            Admin(principal("admin")),
            State(state),
            Path(a.id),
            axum::body::Bytes::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "missing_verified_path");
    }

    #[tokio::test]
    async fn accept_verified_broken_db_returns_clean_500() {
        let dir = tempfile::tempdir().unwrap();
        let pool =
            crate::storage::open(&format!("file://{}", dir.path().join("gone.db").display()))
                .await
                .unwrap();
        pool.close().await;
        let state = AppState {
            db: pool,
            secret_box: Arc::new(Box2::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: true,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: Duration::from_secs(3600),
                allow_internal_urls: true,
            }),
        };
        // Broken DB handle: every step fails; the handler must surface a clean
        // 500 envelope (never panic, never swallow into ok:true).
        let err = accept_verified(
            Admin(principal("admin")),
            State(state),
            Path("whatever".into()),
            axum::body::Bytes::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn enqueue_actions_check_result() {
        let (state, _dir) = test_state().await;
        let (a, _, _) = seed_acquisition(&state).await;

        let res = reverify_acquisition(
            Admin(principal("admin")),
            State(state.clone()),
            Path(a.id.clone()),
        )
        .await
        .unwrap();
        let v = body_json(res).await;
        assert!(v["jobId"].as_str().is_some());

        // Broken pool → enqueue_failed, not ok:true.
        let dir2 = tempfile::tempdir().unwrap();
        let pool2 = crate::storage::open(&format!("file://{}", dir2.path().join("g.db").display()))
            .await
            .unwrap();
        pool2.close().await;
        let broken = AppState {
            db: pool2,
            secret_box: state.secret_box.clone(),
            policy: state.policy,
            hub: state.hub.clone(),
            metrics: state.metrics.clone(),
            config: state.config.clone(),
        };
        let err = reimport_acquisition(Admin(principal("admin")), State(broken), Path("x".into()))
            .await
            .unwrap_err();
        assert_eq!(err.code, "db_error");
    }

    #[tokio::test]
    async fn blacklist_dedupes_via_or_ignore() {
        let (state, _dir) = test_state().await;
        let (a, _, cand) = seed_acquisition(&state).await;
        // Give the candidate a magnet URL so the hash matcher fires.
        let magnet = "magnet:?xt=urn:btih:DEADBEEF&dn=x";
        sqlx::query("UPDATE release_candidates SET raw_json = ? WHERE id = ?")
            .bind(json!({ "enclosureUrl": magnet, "title": "Bad Release" }).to_string())
            .bind(&cand)
            .execute(&state.db)
            .await
            .unwrap();
        acquisitions::mark_enqueued(&state.db, &a.id, Some("DEADBEEF"), None)
            .await
            .unwrap();

        let req = BlacklistRequest {
            scope: "global".into(),
            reason: "junk".into(),
        };
        let res = blacklist_action(&state, &a.id, req, false, Some("u1".into()))
            .await
            .unwrap();
        let v = body_json(res).await;
        // hash (external_id) + url + title = 3 new entries.
        assert_eq!(v["created"], 3);

        // Second identical action: all duplicates → nothing counted, but the
        // unique index means zero NEW rows; handler reports no_match.
        let req = BlacklistRequest {
            scope: "global".into(),
            reason: "junk".into(),
        };
        let err = blacklist_action(&state, &a.id, req, false, Some("u1".into()))
            .await
            .unwrap_err();
        assert_eq!(err.code, "no_match_to_blacklist");

        let entries = crate::storage::blacklist::list(&state.db, 100)
            .await
            .unwrap();
        assert_eq!(entries.len(), 3, "duplicates must not accumulate");
    }

    #[test]
    fn btih_extraction_matches_go() {
        assert_eq!(extract_btih("magnet:?xt=urn:btih:ABC123&dn=name"), "ABC123");
        assert_eq!(extract_btih("magnet:?xt=urn:btih:ABC123"), "ABC123");
        assert_eq!(extract_btih("MAGNET:?XT=URN:BTIH:UPPER"), "UPPER");
        assert_eq!(extract_btih("https://example.com/file.torrent"), "");
        assert_eq!(extract_btih("magnet:?dn=nohash"), "");
    }

    #[tokio::test]
    async fn override_imported_marks_row() {
        let (state, _dir) = test_state().await;
        let (a, _, _) = seed_acquisition(&state).await;
        let res = override_imported(
            Admin(principal("admin")),
            State(state.clone()),
            Path(a.id.clone()),
            axum::body::Bytes::from(json!({ "importedPath": "/library/Movie.mkv" }).to_string()),
        )
        .await
        .unwrap();
        let v = body_json(res).await;
        assert_eq!(v["ok"], true);

        let updated = acquisitions::get(&state.db, &a.id).await.unwrap().unwrap();
        assert_eq!(updated.status, "imported");
        assert_eq!(updated.imported_path.as_deref(), Some("/library/Movie.mkv"));

        // Empty path rejected.
        let err = override_imported(
            Admin(principal("admin")),
            State(state),
            Path(a.id),
            axum::body::Bytes::from(json!({ "importedPath": "  " }).to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "missing_imported_path");
    }
}
