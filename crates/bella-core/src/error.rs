//! Error shape shared by the REST API, the CLI and the MCP tools.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    /// The idempotency key was reused with a different request body.
    IdempotencyMismatch,
    /// The entity is not in a state that allows this operation.
    InvalidState,
    /// A message carried an attempt generation or controller epoch that is
    /// no longer current.
    StaleGeneration,
    /// Machine, project or fleet policy forbids it.
    PolicyDenied,
    /// The snapshot references objects that have not been uploaded.
    SnapshotIncomplete,
    TooLarge,
    /// The controller is reconciling or its database is unavailable.
    Unavailable,
    Internal,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::BadRequest => 400,
            ErrorCode::Unauthorized => 401,
            ErrorCode::Forbidden | ErrorCode::PolicyDenied => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict
            | ErrorCode::IdempotencyMismatch
            | ErrorCode::InvalidState
            | ErrorCode::StaleGeneration => 409,
            ErrorCode::SnapshotIncomplete => 422,
            ErrorCode::TooLarge => 413,
            ErrorCode::Unavailable => 503,
            ErrorCode::Internal => 500,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what} not found"))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }
}
