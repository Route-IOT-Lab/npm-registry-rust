//! Typed error hierarchy + the single `IntoResponse` implementation.
//!
//! Handlers return `Result<_, AppError>`; everything funnels through one place so
//! clients only ever see a normalised JSON body and never a stack trace, a raw
//! SQL error, or an internal path.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// The one and only error type handlers need to know about.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Unauthorized(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    /// No upstream could be reached or all of them failed. Deliberately distinct
    /// from `NotFound`: answering 404 here would tell `npm install` that a
    /// package does not exist when in fact we simply could not find out.
    #[error("{0}")]
    Upstream(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Jwt(#[from] jsonwebtoken::errors::Error),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Database(_)
            | AppError::Io(_)
            | AppError::Json(_)
            | AppError::Jwt(_)
            | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Client-facing message. Internal failures are masked.
    fn public_message(&self) -> String {
        match self {
            AppError::Database(_)
            | AppError::Io(_)
            | AppError::Json(_)
            | AppError::Jwt(_)
            | AppError::Internal(_) => "internal server error".to_string(),
            other => other.to_string(),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        AppError::BadRequest(msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        AppError::Unauthorized(msg.into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        AppError::Forbidden(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        AppError::NotFound(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        AppError::Conflict(msg.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();

        // Log the full chain internally, expose nothing.
        if status.is_server_error() {
            tracing::error!(error = ?self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }

        let message = self.public_message();

        // Shape deliberately mirrors CouchDB/npm-registry errors so the npm client
        // and existing tooling render a readable message.
        let body = Json(json!({
            "error": message,
            "reason": message,
        }));

        (status, body).into_response()
    }
}

/// Convenience alias used across handlers.
pub type AppResult<T> = Result<T, AppError>;
