//! Enrollment storage and the machine owner's local controls.
//!
//! The machine token goes to a private file; everything else about the
//! enrollment goes to the journal, so a copy of the journal authenticates
//! nothing. Pause, drain and boost are journal settings that a command-line
//! tool changes while the daemon runs; the daemon reads them on every pass
//! of its protection loop, so they work with the controller unreachable.

use crate::JOURNAL_FILE;
use bella_core::machine::LocalControl;
use bella_core::{Clock, SystemClock, UnixMillis};
use bella_journal::{Enrollment, Journal, JournalError};
use bella_net::{PrivateFileError, PrivateFileMode};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Name of the machine token file inside the data directory.
pub const TOKEN_FILE: &str = "machine-token";

/// Enrollment data could not be stored or read.
#[derive(Debug, thiserror::Error)]
pub enum EnrollmentError {
    #[error("could not create {}: {source}", path.display())]
    DataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Token(#[from] PrivateFileError),
    /// The token file is readable by other users, so the token must be
    /// treated as exposed.
    #[error("{} is accessible to other users (mode {mode:o}); re-enroll this machine", path.display())]
    TokenExposed { path: PathBuf, mode: u32 },
    #[error("the machine token in {} is not text", path.display())]
    TokenNotText { path: PathBuf },
}

/// Record a completed enrollment in `data_dir`.
///
/// The identity and policy go to the journal, which accepts one identity
/// for its lifetime: enrolling again with the same identity only replaces
/// the token, and a different identity fails with
/// [`JournalError::AlreadyEnrolled`] and leaves the stored token untouched.
/// The token is written to [`TOKEN_FILE`] through
/// [`bella_net::write_private`] (mode `0600` on Unix).
pub fn store_enrollment(
    data_dir: &Path,
    enrollment: &Enrollment,
    machine_token: &str,
) -> Result<(), EnrollmentError> {
    std::fs::create_dir_all(data_dir).map_err(|source| EnrollmentError::DataDir {
        path: data_dir.to_path_buf(),
        source,
    })?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let journal = Journal::open(data_dir.join(JOURNAL_FILE), clock)?;
    journal.set_enrollment(enrollment)?;
    bella_net::write_private(&data_dir.join(TOKEN_FILE), machine_token.as_bytes())?;
    Ok(())
}

/// The enrollment recorded in `data_dir`, with the current policy, or
/// `None` when the machine was never enrolled.
pub fn load_enrollment(data_dir: &Path) -> Result<Option<Enrollment>, EnrollmentError> {
    let path = data_dir.join(JOURNAL_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let journal = Journal::open_existing(path, clock)?;
    Ok(journal.enrollment()?)
}

/// The machine token stored by [`store_enrollment`].
///
/// A token file that other users can read is refused with
/// [`EnrollmentError::TokenExposed`]. On Windows permissions are not
/// inspected; see `bella_net::write_private`.
pub fn load_machine_token(data_dir: &Path) -> Result<String, EnrollmentError> {
    let path = data_dir.join(TOKEN_FILE);
    let file = bella_net::read_private(&path)?;
    if let PrivateFileMode::TooOpen { mode } = file.mode {
        return Err(EnrollmentError::TokenExposed { path, mode });
    }
    String::from_utf8(file.bytes)
        .map(|text| text.trim().to_string())
        .map_err(|_| EnrollmentError::TokenNotText { path })
}

/// The machine owner's pause, drain and boost settings in a data directory.
///
/// Obtained from [`local_control`]. Each call is one journal transaction
/// next to the running daemon, which picks the change up on its next
/// protection pass.
pub struct LocalControlHandle {
    journal: Journal,
}

impl LocalControlHandle {
    /// The current settings.
    pub fn get(&self) -> Result<LocalControl, JournalError> {
        self.journal.local_control()
    }

    /// Replace all settings.
    pub fn set(&self, control: LocalControl) -> Result<(), JournalError> {
        self.journal.set_local_control(control)
    }

    fn update(&self, change: impl FnOnce(&mut LocalControl)) -> Result<LocalControl, JournalError> {
        self.journal.with_transaction(|tx| {
            let mut control = tx.local_control()?;
            change(&mut control);
            tx.set_local_control(control)?;
            Ok(control)
        })
    }

    /// Accept no new work (`true`) or accept it again (`false`). Running
    /// work continues either way.
    pub fn set_paused(&self, paused: bool) -> Result<LocalControl, JournalError> {
        self.update(|control| control.paused = paused)
    }

    /// Accept no new work and let the machine run empty, or end that.
    pub fn set_draining(&self, draining: bool) -> Result<LocalControl, JournalError> {
        self.update(|control| control.draining = draining)
    }

    /// Allow managed work the static ceiling until `until`, or end a boost
    /// with `None`. A boost never lowers the foreground floor.
    pub fn set_boost(&self, until: Option<UnixMillis>) -> Result<LocalControl, JournalError> {
        self.update(|control| control.boost_until = until)
    }
}

/// Open the local controls of the worker whose data directory is
/// `data_dir`, through `Journal::open_existing`: nothing is created or
/// migrated, so the journal must exist already.
pub fn local_control(data_dir: &Path) -> Result<LocalControlHandle, JournalError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let journal = Journal::open_existing(data_dir.join(JOURNAL_FILE), clock)?;
    Ok(LocalControlHandle { journal })
}
