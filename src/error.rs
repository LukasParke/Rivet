//! Shared error type. Every HTTP-facing failure maps to a stable
//! machine-readable code, matching the JSON envelopes of the original API.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,

    /// 404 with a custom machine-readable code.
    #[error("{0}")]
    NotFoundCode(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("csrf required")]
    CsrfRequired,

    #[error("conflict: {0}")]
    Conflict(String),

    /// Upstream client call failed (qBittorrent, indexer, ...).
    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("database error")]
    Db(#[from] sqlx::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn internal(msg: impl Into<String>) -> Self {
        AppError::Internal(msg.into())
    }

    /// Returns the stable machine-readable code and HTTP status for this error.
    pub fn code(&self) -> (CowCode, StatusCode) {
        match self {
            AppError::NotFound => (CowCode::Static("not_found"), StatusCode::NOT_FOUND),
            AppError::NotFoundCode(c) => (CowCode::Owned(c.clone()), StatusCode::NOT_FOUND),
            AppError::Unauthorized => (CowCode::Static("unauthorized"), StatusCode::UNAUTHORIZED),
            AppError::Forbidden => (CowCode::Static("forbidden"), StatusCode::FORBIDDEN),
            AppError::BadRequest(c) => (CowCode::Owned(c.clone()), StatusCode::BAD_REQUEST),
            AppError::CsrfRequired => (CowCode::Static("csrf_required"), StatusCode::FORBIDDEN),
            AppError::Conflict(c) => (CowCode::Owned(c.clone()), StatusCode::CONFLICT),
            AppError::Upstream(_) => (CowCode::Static("upstream_error"), StatusCode::BAD_GATEWAY),
            AppError::Db(_) | AppError::Internal(_) => (
                CowCode::Static("internal_error"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }
}

/// Error code that is either a compile-time constant or an owned string.
#[derive(Debug, Clone)]
pub enum CowCode {
    Static(&'static str),
    Owned(String),
}

impl std::fmt::Display for CowCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CowCode::Static(s) => f.write_str(s),
            CowCode::Owned(s) => f.write_str(s),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, status) = self.code();
        if matches!(self, AppError::Db(_) | AppError::Internal(_)) {
            tracing::error!(error = %self, "request failed");
        }
        (status, axum::Json(json!({ "error": code.to_string() }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, AppError>;
