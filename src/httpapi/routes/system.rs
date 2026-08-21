//! System endpoints (`*arr` parity): status, health aggregation, database
//! backups and remote path mappings.
//!
//! Role split: `/system/status`, `/system/health`, `/system/backups` and
//! `GET /system/backup` are readable by any authenticated user (read-only
//! operational visibility); backup creation and remote-path-mapping
//! mutations are admin-only.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::{ApiResult, AppError};
use crate::httpapi::middleware::auth::{Admin, AuthenticatedUser};
use crate::state::AppState;
use crate::storage::audit::{self, AuditEvent};
use crate::storage::system_ops::{
    self, BackupFile, Issue, RemotePathMapping,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/system/status", get(system_status))
        .route("/system/health", get(system_health))
        .route(
            "/system/backup",
            get(latest_backup).post(create_backup),
        )
        .route("/system/backups", get(list_backups_handler))
        .route(
            "/remotepathmappings",
            get(list_mappings_handler).post(create_mapping),
        )
        .route("/remotepathmappings/{id}", put(update_mapping_handler).delete(delete_mapping_handler))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolves the SQLite file path from the configured DSN
/// (`file:///abs/path.db` or bare path); `None` for in-memory databases.
fn database_path(config: &AppState) -> Option<PathBuf> {
    let url = &config.config.database_url;
    let raw = url.strip_prefix("file://").unwrap_or(url);
    if raw.is_empty() || raw == ":memory:" {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// Backup directory: a `backups/` sibling of the database file. Without a
/// resolvable database path (in-memory), backups are refused.
fn backup_dir(config: &AppState) -> Result<PathBuf, AppError> {
    let db = database_path(config).ok_or_else(|| {
        AppError::BadRequest("backups require a file-backed database".to_string())
    })?;
    Ok(db
        .parent()
        .map(|p| p.join("backups"))
        .unwrap_or_else(|| PathBuf::from("backups")))
}

async fn record_audit(state: &AppState, actor: Option<String>, action: &str, target: Option<String>) {
    let _ = audit::insert(
        &state.db,
        AuditEvent {
            actor_user_id: actor,
            action: action.to_string(),
            target_type: Some("system".to_string()),
            target_id: target,
            ..Default::default()
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// GET /system/status
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemStatus {
    version: &'static str,
    env: String,
    dry_run: bool,
    database_path: Option<String>,
    /// Size of the live database file in bytes; absent when unresolvable.
    database_size_bytes: Option<u64>,
}

async fn system_status(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
) -> ApiResult<Json<SystemStatus>> {
    let db_path = database_path(&state);
    let size = db_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    Ok(Json(SystemStatus {
        version: env!("CARGO_PKG_VERSION"),
        env: state.config.env.clone(),
        dry_run: state.config.dry_run,
        database_path: db_path.map(|p| p.to_string_lossy().into_owned()),
        database_size_bytes: size,
    }))
}

// ---------------------------------------------------------------------------
// GET /system/health
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct HealthResponse {
    issues: Vec<Issue>,
}

async fn system_health(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
) -> ApiResult<Json<HealthResponse>> {
    Ok(Json(HealthResponse {
        issues: system_ops::health_issues(&state.db).await,
    }))
}

// ---------------------------------------------------------------------------
// GET/POST /system/backup, GET /system/backups
// ---------------------------------------------------------------------------

async fn latest_backup(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
) -> ApiResult<Json<BackupFile>> {
    let dir = backup_dir(&state)?;
    system_ops::list_backups(&dir)
        .into_iter()
        .next()
        .map(Json)
        .ok_or(AppError::NotFoundCode("no_backups".to_string()))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupCreated {
    filename: String,
    size_bytes: u64,
}

async fn create_backup(
    Admin(user): Admin,
    State(state): State<AppState>,
) -> ApiResult<(axum::http::StatusCode, Json<BackupCreated>)> {
    let dir = backup_dir(&state)?;
    let path = system_ops::backup_database(&state.db, &dir).await?;
    record_audit(
        &state,
        Some(user.user.id),
        "system.backup_created",
        Some(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
    )
    .await;
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((
        axum::http::StatusCode::CREATED,
        Json(BackupCreated {
            size_bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            filename,
        }),
    ))
}

async fn list_backups_handler(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<BackupFile>>> {
    let dir = backup_dir(&state)?;
    Ok(Json(system_ops::list_backups(&dir)))
}

// ---------------------------------------------------------------------------
// /remotepathmappings CRUD (admin)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MappingRequest {
    client_path: String,
    local_path: String,
}

fn validate_mapping(req: &MappingRequest) -> Result<(), AppError> {
    if req.client_path.trim().is_empty() || req.local_path.trim().is_empty() {
        return Err(AppError::BadRequest(
            "client_path and local_path are required".to_string(),
        ));
    }
    Ok(())
}

async fn list_mappings_handler(
    Admin(_user): Admin,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<RemotePathMapping>>> {
    Ok(Json(system_ops::list_mappings(&state.db).await?))
}

async fn create_mapping(
    Admin(user): Admin,
    State(state): State<AppState>,
    Json(req): Json<MappingRequest>,
) -> ApiResult<(axum::http::StatusCode, Json<RemotePathMapping>)> {
    validate_mapping(&req)?;
    let mapping =
        system_ops::upsert_mapping(&state.db, req.client_path.trim(), req.local_path.trim())
            .await?;
    record_audit(
        &state,
        Some(user.user.id),
        "remotepathmapping.saved",
        Some(mapping.id.clone()),
    )
    .await;
    Ok((axum::http::StatusCode::CREATED, Json(mapping)))
}

async fn update_mapping_handler(
    Admin(user): Admin,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<MappingRequest>,
) -> ApiResult<(axum::http::StatusCode, Json<RemotePathMapping>)> {
    validate_mapping(&req)?;
    // Upsert keyed on client_path; only report success when the row with the
    // requested id actually exists afterwards.
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT id FROM remote_path_mappings WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let mapping =
        system_ops::upsert_mapping(&state.db, req.client_path.trim(), req.local_path.trim())
            .await?;
    // If the unique client_path moved to a new id, drop the stale row so the
    // collection never holds two entries.
    if existing != mapping.id {
        system_ops::delete_mapping(&state.db, &existing).await?;
    }
    record_audit(
        &state,
        Some(user.user.id),
        "remotepathmapping.saved",
        Some(mapping.id.clone()),
    )
    .await;
    Ok((axum::http::StatusCode::OK, Json(mapping)))
}

async fn delete_mapping_handler(
    Admin(user): Admin,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<axum::http::StatusCode> {
    let deleted = system_ops::delete_mapping(&state.db, &id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }
    record_audit(
        &state,
        Some(user.user.id),
        "remotepathmapping.deleted",
        Some(id),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpapi::middleware::auth::AuthenticatedUser;
    use crate::security::secrets::SecretBox as Box2;
    use crate::ws::hub::Hub;
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::storage::open(&format!("file://{}", dir.path().join("t.db").display()))
            .await
            .unwrap();
        crate::storage::migrations::apply(&pool).await.unwrap();
        let state = AppState {
            db: pool,
            secret_box: Arc::new(Box2::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: format!("file://{}", dir.path().join("t.db").display()),
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

    #[tokio::test]
    async fn status_reports_version_env_dryrun_and_db_info() {
        let (state, _dir) = test_state().await;
        let res = system_status(principal("viewer"), State(state.clone()))
            .await
            .unwrap();
        assert_eq!(res.0.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(res.0.env, "test");
        assert!(res.0.dry_run);
        assert!(res.0.database_path.as_deref().unwrap().ends_with("t.db"));
        assert!(res.0.database_size_bytes.unwrap() > 0);
    }

    #[tokio::test]
    async fn backup_creates_nonempty_listed_file() {
        let (state, dir) = test_state().await;
        let res = create_backup(Admin(principal("admin")), State(state.clone()))
            .await
            .unwrap();
        assert!(res.1.size_bytes > 0);
        let listed = system_ops::list_backups(&dir.path().join("backups"));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, res.1.filename);

        let latest = latest_backup(principal("viewer"), State(state))
            .await
            .unwrap();
        assert_eq!(latest.0.name, listed[0].name);
    }

    /// Seeds a `role` user plus a live session; returns the session id.
    async fn seed_user_with_session(state: &AppState, email: &str, role: &str) -> String {
        let hash = crate::security::password::hash_password("correct-horse-battery").unwrap();
        let user =
            crate::storage::users::create(&state.db, email, &hash, "viewer").await.unwrap();
        sqlx::query("UPDATE users SET role = ? WHERE id = ?")
            .bind(role)
            .bind(&user.id)
            .execute(&state.db)
            .await
            .unwrap();
        crate::storage::sessions::create(
            &state.db,
            &user.id,
            chrono::Duration::hours(1),
            None,
            None,
        )
        .await
        .unwrap()
        .id
    }

    async fn csrf_of(state: &AppState, sid: &str) -> String {
        crate::storage::sessions::get(&state.db, sid)
            .await
            .unwrap()
            .unwrap()
            .csrf_token
    }

    fn authed_req(
        method: &str,
        uri: &str,
        sid: Option<&str>,
        csrf: Option<&str>,
        json: Option<String>,
    ) -> axum::http::Request<axum::body::Body> {
        let mut b = axum::http::Request::builder()
            .method(method)
            .uri(uri);
        if let Some(sid) = sid {
            b = b.header("cookie", format!("rivet_session={sid}"));
        }
        if let Some(csrf) = csrf {
            b = b.header("x-csrf-token", csrf);
        }
        if let Some(body) = json {
            b = b.header("content-type", "application/json");
            b.body(axum::body::Body::from(body)).unwrap()
        } else {
            b.body(axum::body::Body::empty()).unwrap()
        }
    }

    /// Admin-gating verified through the real middleware stack
    /// (load_session -> csrf), since extractor enforcement only happens
    /// during request extraction, not in direct handler calls.
    #[tokio::test]
    async fn admin_gated_routes_enforce_roles() {
        use tower::ServiceExt;

        let (state, _dir) = test_state().await;
        let viewer_sid = seed_user_with_session(&state, "v@x.test", "viewer").await;
        let viewer_csrf = csrf_of(&state, &viewer_sid).await;
        let admin_sid = seed_user_with_session(&state, "a@x.test", "admin").await;
        let admin_csrf = csrf_of(&state, &admin_sid).await;

        let app: axum::Router = Router::new()
            .merge(router())
            .layer(axum::middleware::from_fn(crate::httpapi::middleware::auth::csrf))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::httpapi::middleware::auth::load_session,
            ))
            .with_state(state);

        // Unauthenticated backup creation => 401.
        let res = app
            .clone()
            .oneshot(authed_req("POST", "/system/backup", None, None, None))
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);

        // Viewer backup creation and mapping mutation => 403.
        for uri in ["/system/backup", "/remotepathmappings"] {
            let res = app
                .clone()
                .oneshot(authed_req(
                    "POST",
                    uri,
                    Some(&viewer_sid),
                    Some(&viewer_csrf),
                    (uri != "/system/backup").then(|| {
                        r#"{"clientPath": "/a", "localPath": "/b"}"#.to_string()
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN, "POST {uri}");
        }

        // Admin passes both.
        let res = app
            .clone()
            .oneshot(authed_req(
                "POST",
                "/system/backup",
                Some(&admin_sid),
                Some(&admin_csrf),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::CREATED);

        let res = app
            .oneshot(authed_req(
                "POST",
                "/remotepathmappings",
                Some(&admin_sid),
                Some(&admin_csrf),
                Some(r#"{"clientPath": "/mnt/r", "localPath": "/l"}"#.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::CREATED);
    }

    #[tokio::test]
    async fn health_emits_issue_for_injected_failed_jobs() {
        let (state, _dir) = test_state().await;
        let now = crate::timeutil::now();
        for i in 0..4 {
            sqlx::query(
                "INSERT INTO jobs (id, kind, state, attempt, run_after, created_at, updated_at)
                 VALUES (?, 'search_run', 'failed', 3, ?, ?, ?)",
            )
            .bind(format!("jf{i}"))
            .bind(&now)
            .bind(&now)
            .bind(&now)
            .execute(&state.db)
            .await
            .unwrap();
        }
        let res = system_health(principal("viewer"), State(state))
            .await
            .unwrap();
        assert!(res
            .0
            .issues
            .iter()
            .any(|i| i.source == "failed_jobs" && i.severity == "error"));
    }

    #[tokio::test]
    async fn mapping_crud_end_to_end() {
        let (state, _dir) = test_state().await;
        let created = create_mapping(
            Admin(principal("admin")),
            State(state.clone()),
            Json(MappingRequest {
                client_path: "/mnt/remote/data".into(),
                local_path: "/srv/media".into(),
            }),
        )
        .await
        .unwrap();
        let id = created.1.id.clone();

        let listed = list_mappings_handler(Admin(principal("admin")), State(state.clone()))
            .await
            .unwrap();
        assert_eq!(listed.0.len(), 1);
        assert_eq!(listed.0[0].client_path, "/mnt/remote/data");

        let translated =
            system_ops::translate(&listed.0, "/mnt/remote/data/movies/x.mkv").unwrap();
        assert_eq!(translated, "/srv/media/movies/x.mkv");

        let updated = update_mapping_handler(
            Admin(principal("admin")),
            State(state.clone()),
            AxumPath(id.clone()),
            Json(MappingRequest {
                client_path: "/mnt/remote/data".into(),
                local_path: "/data2".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(updated.1.local_path, "/data2");
        assert_eq!(updated.1.id, id);
        assert_eq!(list_mappings_handler(Admin(principal("admin")), State(state.clone()))
            .await
            .unwrap()
            .0
            .len(), 1);

        let status = delete_mapping_handler(
            Admin(principal("admin")),
            State(state.clone()),
            AxumPath(id),
        )
        .await
        .unwrap();
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        assert!(list_mappings_handler(Admin(principal("admin")), State(state))
            .await
            .unwrap()
            .0
            .is_empty());
    }

    #[tokio::test]
    async fn mapping_validation_and_missing_rows() {
        let (state, _dir) = test_state().await;
        let err = create_mapping(
            Admin(principal("admin")),
            State(state.clone()),
            Json(MappingRequest { client_path: " ".into(), local_path: "/x".into() }),
        )
        .await
        .expect_err("blank client_path must be rejected");
        assert_eq!(err.code().1, axum::http::StatusCode::BAD_REQUEST);
        let err = update_mapping_handler(
            Admin(principal("admin")),
            State(state.clone()),
            AxumPath("missing".into()),
            Json(MappingRequest { client_path: "/a".into(), local_path: "/b".into() }),
        )
        .await
        .expect_err("unknown id must 404");
        assert_eq!(err.code().1, axum::http::StatusCode::NOT_FOUND);
    }
}
