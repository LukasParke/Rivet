//! `/setup/paths/*`: filesystem layout validation + persistence (admin only).
//!
//! REVIEWED FIX: in dry-run mode the writability probe is stat-only — the
//! original wrote a `.rivet_write_test` file even while claiming to simulate
//! the filesystem. Non-dry-run validation still performs a real create/remove
//! probe.

use std::path::Path;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::httpapi::middleware::auth::Admin;
use crate::state::AppState;
use crate::ws::hub::PublishExt;
use crate::storage::audit::{insert as insert_audit, AuditEvent};

const SETTING_KEY: &str = "setup.paths";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SetupPathsRequest {
    downloadsCompletedPath: String,
    libraryRoots: Vec<String>,
    quarantinePath: String,
}

#[derive(Debug, Default, Clone, Serialize)]
struct PathCheck {
    path: String,
    exists: bool,
    isDir: bool,
    readable: bool,
    writable: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    filesystemId: String,
    hardlinkViable: bool,
}

#[derive(Debug, Serialize)]
struct ValidateResponse {
    ok: bool,
    downloadsCompleted: PathCheck,
    libraryRoots: Vec<PathCheck>,
    quarantine: PathCheck,
}

#[derive(Debug, Serialize)]
struct SaveResponse {
    saved: bool,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/setup/paths/validate", axum::routing::post(validate))
        .route("/setup/paths/save", axum::routing::post(save))
}

async fn validate(
    _user: Admin,
    State(state): State<AppState>,
    body: Result<axum::Json<SetupPathsRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    match validate_paths(&state, &req, state.config.dry_run).await {
        Ok(resp) => axum::Json(resp).into_response(),
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_paths", "message": msg})),
        )
            .into_response(),
    }
}

async fn save(
    user: Admin,
    State(state): State<AppState>,
    body: Result<axum::Json<SetupPathsRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let actor = user.0.user.id.clone();
    let Ok(axum::Json(req)) = body else {
        return AppError::BadRequest("invalid_json".into()).into_response();
    };
    let resp = match validate_paths(&state, &req, state.config.dry_run).await {
        Ok(r) => r,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "invalid_paths", "message": msg})),
            )
                .into_response()
        }
    };

    // In dry run mode saving is allowed even if paths don't exist yet; in
    // normal mode every path must have validated.
    if !resp.ok && !state.config.dry_run {
        return (StatusCode::BAD_REQUEST, axum::Json(resp)).into_response();
    }

    let payload = serde_json::to_string(&req).expect("serializable request");
    if let Err(_) = crate::storage::settings::upsert(&state.db, SETTING_KEY, &payload).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "save_failed"})),
        )
            .into_response();
    }

    let _ = insert_audit(
        &state.db,
        AuditEvent { action: "setup.paths_saved".into(), actor_user_id: Some(actor.clone()), ..Default::default() },
    )
    .await;
    state.hub.publish_event("audit.event", json!({"action": "setup.paths_saved", "actorUserId": actor}));
    axum::Json(SaveResponse { saved: true }).into_response()
}

/// Lexical cleanup mirroring Go's `filepath.Clean`: trims space, collapses
/// `.`/`//`, resolves inner `..` without touching the filesystem.
fn clean_path(p: &str) -> String {
    let t = p.trim();
    if t.is_empty() {
        return String::new();
    }
    let absolute = t.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for comp in t.split('/') {
        match comp {
            "" | "." => {}
            ".." => match parts.last() {
                Some(prev) if *prev != ".." => {
                    parts.pop();
                }
                _ if absolute => {} // .. above root stays at root
                _ => parts.push(".."),
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

fn can_read_dir(path: &Path) -> bool {
    // Opening + listing proves readability; an empty directory is fine.
    std::fs::read_dir(path).is_ok()
}

/// Real writability probe (non-dry-run only): create + remove a marker file.
fn can_write_dir_probe(path: &Path) -> bool {
    let target = path.join(".rivet_write_test");
    match std::fs::write(&target, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&target);
            true
        }
        Err(_) => false,
    }
}

/// Stat-only writability approximation used in dry-run mode so validation
/// never mutates the filesystem.
fn stat_only_writable(md: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        !md.permissions().readonly() && md.permissions().mode() & 0o200 != 0
    }
    #[cfg(not(unix))]
    {
        !md.permissions().readonly()
    }
}

#[cfg(unix)]
fn filesystem_id(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(md) => format!("dev:{}", md.dev()),
        Err(_) => String::new(),
    }
}

#[cfg(not(unix))]
fn filesystem_id(_path: &Path) -> String {
    String::new()
}

fn check_dir_rw(path: &str, dry_run: bool) -> PathCheck {
    let mut pc = PathCheck { path: path.to_string(), ..Default::default() };
    let md = std::fs::metadata(path);
    let Some(md) = md.ok() else {
        pc.exists = false;
        if dry_run {
            pc.warnings.push(
                "Path does not exist (will be validated when actually used in dry run mode)"
                    .into(),
            );
        } else {
            pc.errors.push(format!("stat {path}: no such file or directory"));
        }
        return pc;
    };
    pc.exists = true;
    pc.isDir = md.is_dir();
    if !pc.isDir {
        if dry_run {
            pc.warnings.push("Path exists but is not a directory".into());
        } else {
            pc.errors.push("not a directory".into());
        }
        return pc;
    }
    pc.readable = can_read_dir(Path::new(path));
    pc.writable = if dry_run { stat_only_writable(&md) } else { can_write_dir_probe(Path::new(path)) };
    let bucket = if dry_run { &mut pc.warnings } else { &mut pc.errors };
    if !pc.readable {
        bucket.push(if dry_run { "Directory exists but is not readable".into() } else { "not readable".into() });
    }
    if !pc.writable {
        bucket.push(if dry_run { "Directory exists but is not writable".into() } else { "not writable".into() });
    }
    pc.filesystemId = filesystem_id(Path::new(path));
    pc
}

fn all_paths_ok(roots: &[PathCheck]) -> bool {
    roots.iter().all(|r| r.exists && r.isDir && r.readable && r.writable)
}

async fn validate_paths(
    state: &AppState,
    req: &SetupPathsRequest,
    dry_run: bool,
) -> Result<ValidateResponse, String> {
    state.hub.publish_event(
        "setup.paths.validation.started",
        json!({"dryRun": dry_run, "libraryRootsCount": req.libraryRoots.len()}),
    );

    let dl_raw = clean_path(&req.downloadsCompletedPath);
    if dl_raw.is_empty() {
        state.hub.publish_event(
            "setup.paths.validation.error",
            json!({"error": "missing_downloads_completed_path", "message": "downloadsCompletedPath is required"}),
        );
        return Err("downloadsCompletedPath is required".into());
    }
    let q_raw = clean_path(&req.quarantinePath);
    if q_raw.is_empty() {
        state.hub.publish_event(
            "setup.paths.validation.error",
            json!({"error": "missing_quarantine_path", "message": "quarantinePath is required"}),
        );
        return Err("quarantinePath is required".into());
    }
    if req.libraryRoots.is_empty() {
        state.hub.publish_event(
            "setup.paths.validation.error",
            json!({"error": "missing_library_roots", "message": "at least one library root is required"}),
        );
        return Err("at least one library root is required".into());
    }

    let dl_check = check_dir_rw(&dl_raw, dry_run);
    let q_check = check_dir_rw(&q_raw, dry_run);

    let mut roots: Vec<PathCheck> = Vec::with_capacity(req.libraryRoots.len());
    for r in &req.libraryRoots {
        let rp = clean_path(r);
        if rp.is_empty() {
            let mut pc = PathCheck { path: r.clone(), ..Default::default() };
            pc.errors.push("empty path".into());
            roots.push(pc);
            continue;
        }
        roots.push(check_dir_rw(&rp, dry_run));
    }

    // Hardlink viability: downloads dir on the same filesystem as each root.
    for root in &mut roots {
        if dl_check.exists && root.exists {
            root.hardlinkViable =
                !dl_check.filesystemId.is_empty() && dl_check.filesystemId == root.filesystemId;
            if !root.hardlinkViable {
                root.warnings.push("hardlink not viable (different filesystem)".into());
            }
        }
    }

    // Dry run tolerates missing/unwritable paths; live mode does not.
    let ok = if dry_run {
        true
    } else {
        dl_check.exists
            && dl_check.isDir
            && dl_check.readable
            && dl_check.writable
            && q_check.exists
            && q_check.isDir
            && q_check.readable
            && q_check.writable
            && all_paths_ok(&roots)
    };

    state.hub.publish_event(
        "setup.paths.validation.completed",
        json!({
            "ok": ok,
            "dryRun": dry_run,
            "summary": {
                "downloadsCompletedOk": dl_check.exists && dl_check.isDir && dl_check.readable && dl_check.writable,
                "quarantineOk": q_check.exists && q_check.isDir && q_check.readable && q_check.writable,
                "libraryRootsOk": !roots.is_empty() && all_paths_ok(&roots),
            },
        }),
    );

    Ok(ValidateResponse { ok, downloadsCompleted: dl_check, libraryRoots: roots, quarantine: q_check })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::metrics::Metrics;
    use crate::security::secrets::SecretBox;
    use crate::security::OutboundPolicy;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware;
    use tower::ServiceExt;

    async fn app_state(dry_run: bool) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("paths_test.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&db).await.unwrap();
        let config = Arc::new(crate::config::Config {
            env: "test".into(),
            http_addr: String::new(),
            database_url: String::new(),
            dry_run,
            dry_run_fixture_path: None,
            ffprobe_path: None,
            secret_key_base64: None,
            session_cookie_name: "rivet_session".into(),
            session_ttl: std::time::Duration::from_secs(30 * 24 * 3600),
            allow_internal_urls: true,
        });
        (
            dir,
            AppState {
                db,
                secret_box: Arc::new(SecretBox::new(None).unwrap()),
                policy: OutboundPolicy::dev(),
                hub: Arc::new(Hub::new()),
                metrics: Arc::new(Metrics::default()),
                config,
            },
        )
    }

    async fn seed_admin_session(state: &AppState) -> crate::storage::sessions::Session {
        let hash = crate::security::password::hash_password("long-enough-password").unwrap();
        let u = crate::storage::users::create(&state.db, "admin@x.test", &hash, "admin")
            .await
            .unwrap();
        crate::storage::sessions::create(&state.db, &u.id, chrono::Duration::hours(1), None, None)
            .await
            .unwrap()
    }

    fn full_router(state: AppState) -> axum::Router {
        router()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state)
    }

    async fn post(
        state: AppState,
        uri: &str,
        body: &str,
        sess: Option<&crate::storage::sessions::Session>,
    ) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(s) = sess {
            b = b
                .header("cookie", format!("rivet_session={}", s.id))
                .header("x-csrf-token", &s.csrf_token);
        }
        let resp = full_router(state)
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn admin_gate_blocks_anonymous() {
        let (_dir, state) = app_state(false).await;
        let (s, v) = post(
            state,
            "/api/v1/setup/paths/validate",
            r#"{"downloadsCompletedPath":"/tmp/a","libraryRoots":["/tmp/b"],"quarantinePath":"/tmp/c"}"#,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "unauthorized");
    }

    #[tokio::test]
    async fn validate_live_mode_checks_directories_without_side_effects_on_missing() {
        let (dir, state) = app_state(false).await;
        let real = dir.path().join("real-downloads");
        std::fs::create_dir_all(&real).unwrap();

        let sess = seed_admin_session(&state).await;
        let body = serde_json::json!({
            "downloadsCompletedPath": real.display().to_string(),
            "libraryRoots": [real.display().to_string()],
            "quarantinePath": dir.path().join("missing-quarantine").display().to_string(),
        })
        .to_string();

        let (s, v) = post(state.clone(), "/api/v1/setup/paths/validate", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], false, "missing quarantine fails live validation");
        assert_eq!(v["downloadsCompleted"]["exists"], true);
        assert_eq!(v["downloadsCompleted"]["writable"], true);
        assert!(
            v["downloadsCompleted"]["filesystemId"].as_str().unwrap_or("").starts_with("dev:"),
            "filesystem id computed"
        );
        assert_eq!(v["hardlinksNote"], serde_json::Value::Null);

        // Real-mode probes clean up their marker file.
        let entries: Vec<_> = std::fs::read_dir(&real).unwrap().collect();
        assert!(entries.is_empty(), "probe file must be removed");
    }

    #[tokio::test]
    async fn dry_run_never_writes_probe_files() {
        let (dir, state) = app_state(true).await;
        let ghost = dir.path().join("ghost"); // does NOT exist
        let existing = dir.path().join("exists");
        std::fs::create_dir_all(&existing).unwrap();

        let sess = seed_admin_session(&state).await;
        let body = serde_json::json!({
            "downloadsCompletedPath": ghost.display().to_string(),
            "libraryRoots": [existing.display().to_string(), ghost.display().to_string()],
            "quarantinePath": ghost.display().to_string(),
        })
        .to_string();

        let (s, v) = post(state, "/api/v1/setup/paths/validate", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], true, "dry run tolerates missing paths");
        assert_eq!(v["downloadsCompleted"]["exists"], false);
        assert!(v["downloadsCompleted"]["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("does not exist")));

        // THE FIX: no probe files anywhere under either directory.
        for p in [&ghost, &existing] {
            assert!(!p.join(".rivet_write_test").exists(), "dry run must not write probe files");
        }
        assert_eq!(std::fs::read_dir(&existing).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn save_persists_request_json_and_audits() {
        let (dir, state) = app_state(true).await;
        let real = dir.path().join("dl");
        std::fs::create_dir_all(&real).unwrap();
        let sess = seed_admin_session(&state).await;

        let body = serde_json::json!({
            "downloadsCompletedPath": real.display().to_string(),
            "libraryRoots": ["/somewhere/library"],
            "quarantinePath": "/somewhere/quarantine",
        })
        .to_string();

        // Live mode would reject unknown paths; dry-run saves anyway.
        let (s, v) = post(state.clone(), "/api/v1/setup/paths/save", &body, Some(&sess)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["saved"], true);

        let stored = crate::storage::settings::get(&state.db, "setup.paths")
            .await
            .unwrap()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed["downloadsCompletedPath"], real.display().to_string());
        assert_eq!(parsed["libraryRoots"][0], "/somewhere/library");

        let actions = crate::storage::audit::recent(&state.db, 10).await.unwrap();
        assert!(actions.iter().any(|a| a.action == "setup.paths_saved"));
    }

    #[test]
    fn clean_path_normalizes_lexically() {
        assert_eq!(clean_path("  /a//b/./c/../d  "), "/a/b/d");
        assert_eq!(clean_path("/.."), "/");
        assert_eq!(clean_path("relative/./x"), "relative/x");
        assert_eq!(clean_path(""), "");
        assert_eq!(clean_path("   "), "");
    }

    #[tokio::test]
    async fn missing_required_fields_are_invalid_paths() {
        let (_dir, state) = app_state(false).await;
        let sess = seed_admin_session(&state).await;
        let (s, v) = post(
            state,
            "/api/v1/setup/paths/validate",
            r#"{"downloadsCompletedPath":"","libraryRoots":[],"quarantinePath":""}"#,
            Some(&sess),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "invalid_paths");
    }
}
