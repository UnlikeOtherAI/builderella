//! Typed journal errors and their mapping onto protocol rejections.

use bella_core::AttemptId;
use bella_core::protocol::{CommandResult, LocalAttemptState, RejectCode};

/// Result of a journal operation.
pub type Result<T, E = JournalError> = std::result::Result<T, E>;

/// Why a journal operation was refused or could not be carried out.
///
/// Two kinds of error share this type. A refusal means the journal looked
/// at its state and said no, having changed nothing; it answers the command
/// that asked, and [`JournalError::reject_code`] gives the protocol code for
/// it. Everything else is a fault on this side (storage, schema, a corrupt
/// row, a missing `recover`) and says nothing about the command.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JournalError {
    /// The journal holds no row for this attempt.
    #[error("attempt {0} is not in the journal")]
    NotFound(AttemptId),

    /// The transition is not in the table documented on
    /// [`can_transition`](crate::can_transition).
    #[error("an attempt cannot go from {from:?} to {to:?}")]
    InvalidState {
        from: LocalAttemptState,
        to: LocalAttemptState,
    },

    /// The operation is not a state change but is still refused in the
    /// attempt's current state.
    #[error("cannot {operation} while the attempt is {state:?}")]
    NotAllowed {
        operation: &'static str,
        state: LocalAttemptState,
    },

    /// The command names a generation older than the one the journal holds
    /// for that attempt or job.
    #[error("generation {offered} is older than generation {held}, which is already held")]
    StaleGeneration { held: u32, offered: u32 },

    /// The command names a generation that cannot belong to the attempt the
    /// journal holds: a newer generation under the same attempt id, or a
    /// second attempt of a job under a generation that is already taken.
    #[error("generation {offered} conflicts with generation {held}, which is already held")]
    GenerationConflict { held: u32, offered: u32 },

    /// The controller epoch is older than the newest one already seen.
    #[error("controller epoch {offered} is older than the known epoch {known}")]
    StaleEpoch { known: u64, offered: u64 },

    /// A start, renewal or repeated offer arrived for an offer that expired
    /// or was released locally without ever being started.
    #[error("the offer for attempt {attempt} is not held: {reason}")]
    OfferNotHeld {
        attempt: AttemptId,
        reason: &'static str,
    },

    /// The attempt's execution authority has run out, so nothing new may
    /// run under it.
    #[error("the execution authority of attempt {attempt} has expired")]
    AuthorityExpired { attempt: AttemptId },

    /// A stop was requested for the attempt, so it may only move towards
    /// cleanup.
    #[error("a stop was requested for attempt {attempt}")]
    CancelRequested { attempt: AttemptId },

    /// A process was about to be recorded as running before the scope that
    /// will own it was journaled.
    #[error("no containment scope is recorded for attempt {attempt}")]
    ScopeNotRecorded { attempt: AttemptId },

    /// A scope of a different kind or name is already recorded.
    #[error("attempt {attempt} already has a different containment scope")]
    ScopeMismatch { attempt: AttemptId },

    /// A different workspace is already recorded.
    #[error("attempt {attempt} already has a different workspace")]
    WorkspaceMismatch { attempt: AttemptId },

    /// A release was attempted with a proof that does not show an empty
    /// scope and a removed workspace.
    #[error("the release proof for attempt {attempt} does not prove the resources are free")]
    IncompleteReleaseProof { attempt: AttemptId },

    /// An acknowledgement names an event sequence or log offset that was
    /// never produced.
    #[error("acknowledged through {acknowledged}, but only {produced} was produced")]
    AckBeyondProduced { produced: u64, acknowledged: u64 },

    /// A log stream was closed by truncation and cannot grow again.
    #[error("log stream `{step}` of attempt {attempt} was truncated at {produced} bytes")]
    LogTruncated {
        attempt: AttemptId,
        step: String,
        produced: u64,
    },

    /// The policy is older than the recorded one, or reuses its revision
    /// with different content.
    #[error("policy revision {offered} does not supersede the recorded revision {known}")]
    StalePolicyRevision { known: u64, offered: u64 },

    /// The journal already belongs to a different enrollment.
    #[error("this journal is already enrolled with a different identity")]
    AlreadyEnrolled,

    /// The operation needs an enrollment and none is recorded.
    #[error("this journal has no enrollment yet")]
    NotEnrolled,

    /// Attempts were touched before [`Journal::recover`](crate::Journal::recover)
    /// ran in this process.
    #[error("attempts cannot change before recover() has run in this process")]
    NotRecovered,

    /// A counter or offset does not fit the storage format.
    #[error("{0} is out of range for the journal")]
    OutOfRange(&'static str),

    /// The database was written by a newer build.
    #[error("journal schema version {found} is newer than the supported version {supported}")]
    SchemaTooNew { found: u32, supported: u32 },

    /// The database needs a migration that this handle may not run.
    #[error("journal schema version {found} is older than the required version {supported}")]
    SchemaTooOld { found: u32, supported: u32 },

    /// The file is a SQLite database that is not a worker journal.
    #[error("the database is not a builderella worker journal")]
    NotAJournal,

    /// The filesystem cannot provide write-ahead logging.
    #[error("the journal could not enter WAL mode (the database reports `{0}`)")]
    WalUnavailable(String),

    /// Another process kept the journal locked for longer than the wait.
    #[error("the journal is locked by another process")]
    Busy,

    /// A stored row could not be decoded.
    #[error("the journal holds an unreadable {what}: {detail}")]
    Corrupt { what: &'static str, detail: String },

    /// SQLite failed.
    #[error("journal storage failed: {0}")]
    Sqlite(#[source] rusqlite::Error),

    /// The reserve file could not be written or removed.
    #[error("journal reserve file: {0}")]
    Io(#[from] std::io::Error),

    /// A transaction was rolled back because an earlier storage failure in
    /// it was not propagated by the caller.
    #[error("the transaction was rolled back after a storage failure: {0}")]
    Aborted(String),
}

impl JournalError {
    /// Wraps a SQLite error, recognising lock contention.
    pub(crate) fn sqlite(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(failure, _)
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                JournalError::Busy
            }
            _ => JournalError::Sqlite(error),
        }
    }

    pub(crate) fn corrupt(what: &'static str, detail: impl std::fmt::Display) -> Self {
        JournalError::Corrupt {
            what,
            detail: detail.to_string(),
        }
    }

    /// Did the write fail because the volume is full?
    ///
    /// When it did, [`Journal::release_reserve`](crate::Journal::release_reserve)
    /// frees enough room for the cleanup records to be written.
    pub fn is_disk_full(&self) -> bool {
        match self {
            JournalError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) => {
                failure.code == rusqlite::ErrorCode::DiskFull
            }
            JournalError::Io(error) => {
                error.kind() == std::io::ErrorKind::StorageFull
                    || error.kind() == std::io::ErrorKind::QuotaExceeded
            }
            _ => false,
        }
    }

    /// The protocol rejection this error stands for, when it is one.
    ///
    /// `None` means the journal itself is at fault (storage, schema, a
    /// corrupt row, a missing `recover`). Such a failure says nothing about
    /// the command, so the worker must not reject the command with it.
    pub fn reject_code(&self) -> Option<RejectCode> {
        match self {
            JournalError::NotFound(_) => Some(RejectCode::UnknownAttempt),
            JournalError::StaleGeneration { .. } => Some(RejectCode::StaleGeneration),
            JournalError::StaleEpoch { .. } => Some(RejectCode::StaleEpoch),
            JournalError::OfferNotHeld { .. } => Some(RejectCode::OfferNotHeld),
            JournalError::StalePolicyRevision { .. } => Some(RejectCode::PolicyRevisionMismatch),
            JournalError::InvalidState { .. }
            | JournalError::NotAllowed { .. }
            | JournalError::GenerationConflict { .. }
            | JournalError::AuthorityExpired { .. }
            | JournalError::CancelRequested { .. }
            | JournalError::ScopeNotRecorded { .. }
            | JournalError::ScopeMismatch { .. }
            | JournalError::WorkspaceMismatch { .. }
            | JournalError::IncompleteReleaseProof { .. }
            | JournalError::AckBeyondProduced { .. }
            | JournalError::LogTruncated { .. }
            | JournalError::AlreadyEnrolled => Some(RejectCode::InvalidState),
            JournalError::NotEnrolled
            | JournalError::NotRecovered
            | JournalError::OutOfRange(_)
            | JournalError::SchemaTooNew { .. }
            | JournalError::SchemaTooOld { .. }
            | JournalError::NotAJournal
            | JournalError::WalUnavailable(_)
            | JournalError::Busy
            | JournalError::Corrupt { .. }
            | JournalError::Sqlite(_)
            | JournalError::Io(_)
            | JournalError::Aborted(_) => None,
        }
    }

    /// The `CommandAck` result for a command this error refused, or `None`
    /// when the error is a fault of the journal rather than a rejection.
    pub fn command_result(&self) -> Option<CommandResult> {
        self.reject_code().map(|code| CommandResult::Rejected {
            code,
            message: self.to_string(),
        })
    }
}

/// The rejection code for a refused command.
///
/// Fails with the error itself when it is a journal fault that has no
/// protocol meaning; see [`JournalError::reject_code`].
impl<'a> TryFrom<&'a JournalError> for RejectCode {
    type Error = &'a JournalError;

    fn try_from(error: &'a JournalError) -> Result<Self, Self::Error> {
        error.reject_code().ok_or(error)
    }
}
