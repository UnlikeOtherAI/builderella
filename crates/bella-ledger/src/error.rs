//! The ledger's error type and its mapping onto the shared API error codes.

use bella_core::ids::{AttemptId, MachineId};
use bella_core::job::JobPhase;
use bella_core::resources::Shortfall;
use bella_core::{ApiError, ErrorCode};

/// Why a ledger operation was refused or could not complete.
///
/// An operation that returns an error has changed nothing: every operation
/// runs in one transaction that is rolled back on error.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The named entity does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// The request collides with existing state (a duplicate name or hash).
    #[error("{0}")]
    Conflict(String),
    /// The idempotency key was already used with a different request body.
    #[error("idempotency key `{key}` was already used with a different request")]
    IdempotencyMismatch { key: String },
    /// The entity is not in a state that allows the operation.
    #[error("{0}")]
    InvalidState(String),
    /// A phase change that `JobPhase::can_transition_to` does not allow.
    #[error("{entity} cannot move from {from:?} to {to:?}")]
    IllegalTransition {
        entity: String,
        from: JobPhase,
        to: JobPhase,
    },
    /// A message named a generation that is not the job's current one.
    #[error("stale generation {got} for {attempt}; the current generation is {current}")]
    StaleGeneration {
        attempt: AttemptId,
        got: u32,
        current: u32,
    },
    /// A command carried a controller epoch that is not the current one, or
    /// this ledger was superseded: the database has since been opened again
    /// and belongs to a newer epoch, so this one may no longer write.
    #[error("stale controller epoch {got}; the current epoch is {current}")]
    StaleEpoch { got: u64, current: u64 },
    /// A message arrived on a connection older than the machine's newest.
    #[error("stale connection {got} for {machine}; the current connection is {current}")]
    StaleConnection {
        machine: MachineId,
        got: u64,
        current: u64,
    },
    /// An attempt event skipped a sequence number. Resend from `expected`.
    #[error("attempt event sequence gap: expected {expected}, got {got}")]
    SequenceGap { expected: u64, got: u64 },
    /// A log chunk skipped bytes. Resend from `expected`.
    #[error("log offset gap: expected {expected}, got {got}")]
    LogGap { expected: u64, got: u64 },
    /// The machine's held reservations plus the request exceed its capacity.
    #[error(
        "insufficient {} on the machine: requested {}, available {}",
        .shortfall.dimension, .shortfall.requested, .shortfall.available
    )]
    Capacity { shortfall: Shortfall },
    /// The machine already holds as many attempts as it may run at once.
    #[error("machine concurrency limit reached: {held} of {limit} attempts held")]
    ConcurrencyLimit { limit: u32, held: u32 },
    /// The job still has an attempt whose reservation has not been released.
    #[error("the job's attempt {attempt} is unresolved")]
    UnresolvedAttempt { attempt: AttemptId },
    /// Machine, project or fleet policy forbids the operation.
    #[error("{0}")]
    PolicyDenied(String),
    /// The principal's role does not allow the operation.
    #[error("{0}")]
    Forbidden(String),
    /// The credential is unknown, revoked, expired or already used.
    #[error("{0}")]
    Unauthorized(String),
    /// The request itself is malformed.
    #[error("{0}")]
    Invalid(String),
    /// The machine has not reconciled since it connected in this epoch.
    #[error("machine {0} must reconcile before it is offered work")]
    NotReconciled(MachineId),
    /// The database was written by a newer version of this software.
    #[error(
        "the `{component}` schema is at version {found}; this build supports up to {supported}"
    )]
    SchemaTooNew {
        component: String,
        found: u32,
        supported: u32,
    },
    /// A stored value could not be decoded.
    #[error("corrupt ledger data: {0}")]
    Corrupt(String),
    /// SQLite refused or failed the operation.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Reading or writing a log file failed.
    #[error("log storage error: {0}")]
    Io(#[from] std::io::Error),
}

impl LedgerError {
    /// The API error code this error is reported as.
    pub fn code(&self) -> ErrorCode {
        match self {
            LedgerError::NotFound(_) => ErrorCode::NotFound,
            LedgerError::Conflict(_)
            | LedgerError::Capacity { .. }
            | LedgerError::ConcurrencyLimit { .. }
            | LedgerError::UnresolvedAttempt { .. } => ErrorCode::Conflict,
            LedgerError::IdempotencyMismatch { .. } => ErrorCode::IdempotencyMismatch,
            LedgerError::InvalidState(_)
            | LedgerError::IllegalTransition { .. }
            | LedgerError::SequenceGap { .. }
            | LedgerError::LogGap { .. } => ErrorCode::InvalidState,
            LedgerError::StaleGeneration { .. }
            | LedgerError::StaleEpoch { .. }
            | LedgerError::StaleConnection { .. } => ErrorCode::StaleGeneration,
            LedgerError::PolicyDenied(_) => ErrorCode::PolicyDenied,
            LedgerError::Forbidden(_) => ErrorCode::Forbidden,
            LedgerError::Unauthorized(_) => ErrorCode::Unauthorized,
            LedgerError::Invalid(_) => ErrorCode::BadRequest,
            LedgerError::NotReconciled(_) => ErrorCode::Unavailable,
            LedgerError::Sqlite(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                ErrorCode::Unavailable
            }
            LedgerError::SchemaTooNew { .. }
            | LedgerError::Corrupt(_)
            | LedgerError::Sqlite(_)
            | LedgerError::Io(_) => ErrorCode::Internal,
        }
    }

    /// The error as an API response body. A capacity error carries its
    /// shortfall, and a sequence or offset gap the expected position, in
    /// `details`.
    pub fn to_api_error(&self) -> ApiError {
        let error = ApiError::new(self.code(), self.to_string());
        match self {
            LedgerError::Capacity { shortfall } => {
                error.with_details(serde_json::json!({ "shortfall": shortfall }))
            }
            LedgerError::SequenceGap { expected, .. } | LedgerError::LogGap { expected, .. } => {
                error.with_details(serde_json::json!({ "expected": expected }))
            }
            _ => error,
        }
    }
}

impl From<LedgerError> for ApiError {
    fn from(error: LedgerError) -> Self {
        error.to_api_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_onto_api_codes() {
        let cases = [
            (LedgerError::NotFound("job".into()), ErrorCode::NotFound),
            (LedgerError::Conflict("taken".into()), ErrorCode::Conflict),
            (
                LedgerError::IdempotencyMismatch { key: "k".into() },
                ErrorCode::IdempotencyMismatch,
            ),
            (
                LedgerError::InvalidState("no".into()),
                ErrorCode::InvalidState,
            ),
            (
                LedgerError::IllegalTransition {
                    entity: "job".into(),
                    from: JobPhase::Finished,
                    to: JobPhase::Queued,
                },
                ErrorCode::InvalidState,
            ),
            (
                LedgerError::StaleGeneration {
                    attempt: AttemptId::generate(),
                    got: 1,
                    current: 2,
                },
                ErrorCode::StaleGeneration,
            ),
            (
                LedgerError::StaleEpoch { got: 1, current: 2 },
                ErrorCode::StaleGeneration,
            ),
            (
                LedgerError::ConcurrencyLimit { limit: 1, held: 1 },
                ErrorCode::Conflict,
            ),
            (
                LedgerError::PolicyDenied("no".into()),
                ErrorCode::PolicyDenied,
            ),
            (LedgerError::Forbidden("no".into()), ErrorCode::Forbidden),
            (
                LedgerError::Unauthorized("no".into()),
                ErrorCode::Unauthorized,
            ),
            (LedgerError::Invalid("bad".into()), ErrorCode::BadRequest),
            (
                LedgerError::NotReconciled(MachineId::generate()),
                ErrorCode::Unavailable,
            ),
            (LedgerError::Corrupt("bad".into()), ErrorCode::Internal),
            (
                LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
                ErrorCode::Internal,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
            let api = error.to_api_error();
            assert_eq!(api.code, code);
            assert_eq!(api.message, error.to_string());
        }
    }

    #[test]
    fn a_capacity_error_carries_its_shortfall() {
        let error = LedgerError::Capacity {
            shortfall: Shortfall {
                dimension: "memory".into(),
                requested: 8,
                available: 3,
            },
        };
        assert_eq!(
            error.to_string(),
            "insufficient memory on the machine: requested 8, available 3"
        );
        let api: ApiError = error.into();
        assert_eq!(api.code, ErrorCode::Conflict);
        assert_eq!(api.details["shortfall"]["dimension"], "memory");
        assert_eq!(api.details["shortfall"]["available"], 3);
    }
}
