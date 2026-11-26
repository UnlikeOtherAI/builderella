//! The journal handle: opening, transactions and the reserve file.

use crate::attempt::{AttemptRecord, HoldOutcome, RevokeOutcome, StartDecision};
use crate::error::{JournalError, Result};
use crate::logs::{LogCursor, LogOffsets};
use crate::recovery::{ExpireReport, RecoveredAttempt};
use crate::reserve::{RESERVE_BYTES, ReserveFile};
use crate::schema::{self, Upgrade};
use crate::tx::{Enrollment, JournalTx};
use bella_core::job::Outcome;
use bella_core::machine::{LocalControl, MachinePolicy};
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, AttemptRef, AttemptReport, CommandResult, ExecutionAuthority,
    LocalAttemptState, LogStream, Offer, ReleaseProof, ScopeIdentity, StepState, UsageSummary,
};
use bella_core::resources::ResourceVector;
use bella_core::{AttemptId, BootId, Clock, CommandId, UnixMillis};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::cell::RefCell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How long a transaction waits for another process (the CLI setting
/// pause or drain, say) to finish its own.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

struct Inner {
    conn: Connection,
    /// The boot established by `recover` in this process.
    boot: Option<BootId>,
}

/// The worker's recovery journal.
///
/// One SQLite database in WAL mode with `synchronous=FULL`: when an
/// operation returns `Ok`, what it recorded survives a crash or a power
/// cut. The handle is `Send + Sync`; operations run one at a time behind a
/// mutex and block on disk, so call them from a blocking thread.
///
/// Each method here runs the [`JournalTx`] operation of the same name in a
/// transaction of its own. [`with_transaction`](Journal::with_transaction)
/// runs several as one.
pub struct Journal {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
    path: Option<PathBuf>,
    reserve: Option<ReserveFile>,
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal")
            .field("path", &self.path)
            .field("reserve", &self.reserve)
            .finish_non_exhaustive()
    }
}

/// Verifies, configures and migrates a freshly opened connection.
///
/// The database is inspected before anything is written to it, so a file
/// that is not a journal, or one from a newer build, is left exactly as it
/// was, journal mode included.
fn prepare(conn: &mut Connection, file_backed: bool, upgrade: Upgrade) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(JournalError::sqlite)?;
    schema::inspect(conn, upgrade)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(JournalError::sqlite)?;
    if file_backed && !mode.eq_ignore_ascii_case("wal") {
        return Err(JournalError::WalUnavailable(mode));
    }
    conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")
        .map_err(JournalError::sqlite)?;
    schema::migrate(conn, upgrade)
}

impl Journal {
    /// Opens the journal at `path`, creating or migrating it as needed.
    ///
    /// This is how the worker daemon opens its journal. It refuses a
    /// database written by a newer build (`SchemaTooNew`) and one that is
    /// not a journal (`NotAJournal`), and it makes sure the reserve file
    /// next to the database exists; see [`ReserveFile`]. A reserve that
    /// cannot be written, because the disk is already full, does not fail
    /// the open: recovery matters more. Check
    /// [`reserve_present`](Journal::reserve_present) afterwards.
    ///
    /// `clock` supplies wall-clock time for timestamps and for every
    /// expiry decision.
    pub fn open(path: impl AsRef<Path>, clock: Arc<dyn Clock>) -> Result<Self> {
        let path = path.as_ref();
        let mut conn = Connection::open(path).map_err(JournalError::sqlite)?;
        prepare(&mut conn, true, Upgrade::Apply)?;
        let reserve = ReserveFile::beside(path, RESERVE_BYTES);
        // A full disk must not keep the worker from recovering; the caller
        // sees the missing reserve through `reserve_present`.
        let _ = reserve.ensure();
        Ok(Self::assemble(
            conn,
            clock,
            Some(path.to_owned()),
            Some(reserve),
        ))
    }

    /// Opens a journal that another process owns, without creating,
    /// migrating or reserving anything.
    ///
    /// For short-lived tools next to a running worker, such as the command
    /// that sets pause or drain: their change is picked up by the worker
    /// the next time it reads [`local_control`](Journal::local_control).
    /// The database must exist and be at exactly this build's schema
    /// version (`SchemaTooOld` / `SchemaTooNew` otherwise), and the reserve
    /// file is left as the worker has it, so a tool can never take back
    /// space the worker released on a full disk.
    pub fn open_existing(path: impl AsRef<Path>, clock: Arc<dyn Clock>) -> Result<Self> {
        let path = path.as_ref();
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conn = Connection::open_with_flags(path, flags).map_err(JournalError::sqlite)?;
        prepare(&mut conn, true, Upgrade::Refuse)?;
        Ok(Self::assemble(conn, clock, Some(path.to_owned()), None))
    }

    /// A journal that lives in memory and is gone when dropped. It has no
    /// reserve file. For tests and dry runs.
    pub fn open_in_memory(clock: Arc<dyn Clock>) -> Result<Self> {
        let mut conn = Connection::open_in_memory().map_err(JournalError::sqlite)?;
        prepare(&mut conn, false, Upgrade::Apply)?;
        Ok(Self::assemble(conn, clock, None, None))
    }

    fn assemble(
        conn: Connection,
        clock: Arc<dyn Clock>,
        path: Option<PathBuf>,
        reserve: Option<ReserveFile>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner { conn, boot: None }),
            clock,
            path,
            reserve,
        }
    }

    /// The database file, or `None` for an in-memory journal.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic inside a transaction rolled it back when the transaction
        // was dropped, so the connection behind a poisoned lock is sound.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `work` in one transaction.
    ///
    /// Everything the closure does through the [`JournalTx`] commits
    /// together when it returns `Ok`, and none of it when it returns `Err`
    /// or panics. The transaction takes the write lock up front, so what
    /// the closure reads cannot change under it.
    ///
    /// A refusal from an operation leaves the transaction usable; a
    /// storage failure does not, and then the call fails with
    /// [`JournalError::Aborted`] even if the closure returned `Ok`.
    ///
    /// The closure must use the handle it is given. Calling back into this
    /// `Journal` from inside it deadlocks.
    pub fn with_transaction<T>(&self, work: impl FnOnce(&JournalTx<'_>) -> Result<T>) -> Result<T> {
        let mut inner = self.lock();
        let Inner { conn, boot } = &mut *inner;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::sqlite)?;
        let tx = JournalTx {
            conn: &transaction,
            now: self.clock.now(),
            boot: RefCell::new(boot.clone()),
            failure: RefCell::new(None),
        };
        let outcome = work(&tx);
        let JournalTx {
            boot: established,
            failure,
            ..
        } = tx;
        let value = outcome?;
        if let Some(failure) = failure.into_inner() {
            return Err(JournalError::Aborted(failure));
        }
        transaction.commit().map_err(JournalError::sqlite)?;
        *boot = established.into_inner();
        Ok(value)
    }

    /// The reserve file of this journal, or `None` when this handle does
    /// not manage one (in-memory, or opened with
    /// [`open_existing`](Journal::open_existing)).
    pub fn reserve(&self) -> Option<&ReserveFile> {
        self.reserve.as_ref()
    }

    /// Is the reserve on disk at its full size? `false` when this handle
    /// manages none.
    pub fn reserve_present(&self) -> bool {
        self.reserve.as_ref().is_some_and(ReserveFile::is_present)
    }

    /// Deletes the reserve file so the journal can keep writing on a full
    /// disk. Returns `true` when space was freed.
    ///
    /// Call it when a write fails with
    /// [`JournalError::is_disk_full`], then journal the stop, cleanup and
    /// release of the attempts that are filling the disk, and remove their
    /// workspaces. The freed room is for those records only; accept no new
    /// work until [`restore_reserve`](Journal::restore_reserve) succeeds.
    pub fn release_reserve(&self) -> Result<bool> {
        match &self.reserve {
            Some(reserve) => Ok(reserve.release()?),
            None => Ok(false),
        }
    }

    /// Writes the reserve file again once space is back. Returns `true`
    /// when it had to be written. Fails, leaving no partial file, while
    /// the disk is still too full.
    pub fn restore_reserve(&self) -> Result<bool> {
        match &self.reserve {
            Some(reserve) => Ok(reserve.ensure()?),
            None => Ok(false),
        }
    }
}

/// Generates the `Journal` methods that run one `JournalTx` operation in a
/// transaction of its own.
macro_rules! in_own_transaction {
    ($( fn $name:ident ( $( $arg:ident : $ty:ty ),* ) -> $ret:ty; )*) => {
        impl Journal {
            $(
                #[doc = concat!(
                    "Runs [`JournalTx::", stringify!($name),
                    "`] in a transaction of its own; see there for what it guarantees."
                )]
                pub fn $name(&self $(, $arg: $ty)*) -> Result<$ret> {
                    self.with_transaction(|tx| tx.$name($($arg),*))
                }
            )*
        }
    };
}

in_own_transaction! {
    fn enrollment() -> Option<Enrollment>;
    fn set_enrollment(enrollment: &Enrollment) -> bool;
    fn policy() -> Option<MachinePolicy>;
    fn set_policy(policy: &MachinePolicy) -> bool;
    fn connection_generation() -> u64;
    fn next_connection_generation() -> u64;
    fn known_epoch() -> u64;
    fn observe_epoch(epoch: u64) -> ();
    fn local_control() -> LocalControl;
    fn set_local_control(control: LocalControl) -> ();

    fn hold_offer(offer: &Offer) -> HoldOutcome;
    fn authorize_start(attempt: &AttemptRef, authority: ExecutionAuthority) -> StartDecision;
    fn revoke(attempt: &AttemptRef) -> RevokeOutcome;
    fn renew_authority(attempt: &AttemptRef, authority: ExecutionAuthority) -> ExecutionAuthority;
    fn request_cancel(attempt: &AttemptRef, reason: &str) -> bool;
    fn set_scope(attempt: &AttemptId, scope: &ScopeIdentity) -> ();
    fn set_workspace(attempt: &AttemptId, workspace: &str) -> ();
    fn set_state(attempt: &AttemptId, state: LocalAttemptState) -> ();
    fn set_step_state(attempt: &AttemptId, step: &str, state: StepState, exit_code: Option<i32>) -> ();
    fn record_outcome(attempt: &AttemptId, outcome: &Outcome, usage: &UsageSummary) -> bool;
    fn release(attempt: &AttemptRef, proof: ReleaseProof) -> AttemptEventMsg;
    fn attempt(attempt: &AttemptId) -> Option<AttemptRecord>;
    fn attempts() -> Vec<AttemptRecord>;
    fn active_attempts() -> Vec<AttemptRecord>;
    fn held_resources() -> ResourceVector;
    fn reports() -> Vec<AttemptReport>;

    fn append_event(attempt: &AttemptRef, event: AttemptEvent) -> AttemptEventMsg;
    fn unacked_events(attempt: &AttemptId) -> Vec<AttemptEventMsg>;
    fn ack_events(attempt: &AttemptId, through_seq: u64) -> ();
    fn command_result(command: &CommandId) -> Option<CommandResult>;
    fn record_command(command: &CommandId, result: &CommandResult) -> bool;
    fn prune(now: UnixMillis, keep: Duration) -> usize;

    fn log_offsets(attempt: &AttemptId, step: &str, stream: LogStream) -> LogOffsets;
    fn log_cursors(attempt: &AttemptId) -> Vec<LogCursor>;
    fn record_log_produced(attempt: &AttemptId, step: &str, stream: LogStream, produced: u64) -> LogOffsets;
    fn ack_log(attempt: &AttemptId, step: &str, stream: LogStream, through_offset: u64) -> LogOffsets;
    fn mark_log_truncated(attempt: &AttemptId, step: &str, stream: LogStream) -> LogOffsets;

    fn recover(current_boot: &BootId) -> Vec<RecoveredAttempt>;
    fn expire(now: UnixMillis) -> ExpireReport;
    fn garbage_collect(now: UnixMillis, keep: Duration) -> Vec<AttemptId>;
    fn forget_attempt(attempt: &AttemptId) -> ();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::ManualClock;

    fn clock() -> Arc<dyn Clock> {
        Arc::new(ManualClock::at(UnixMillis(1_700_000_000_000)))
    }

    fn pragma(journal: &Journal, name: &str) -> String {
        let inner = journal.lock();
        inner
            .conn
            .query_row(&format!("PRAGMA {name}"), [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })
            .map(|value| match value {
                rusqlite::types::Value::Integer(n) => n.to_string(),
                rusqlite::types::Value::Text(text) => text,
                other => format!("{other:?}"),
            })
            .unwrap()
    }

    #[test]
    fn a_file_journal_is_wal_with_full_sync() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db"), clock()).unwrap();
        assert_eq!(pragma(&journal, "journal_mode"), "wal");
        // 2 is FULL.
        assert_eq!(pragma(&journal, "synchronous"), "2");
        assert_eq!(pragma(&journal, "foreign_keys"), "1");
    }

    #[test]
    fn a_second_handle_keeps_full_sync_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let _owner = Journal::open(&path, clock()).unwrap();
        let tool = Journal::open_existing(&path, clock()).unwrap();
        assert_eq!(pragma(&tool, "journal_mode"), "wal");
        assert_eq!(pragma(&tool, "synchronous"), "2");
    }

    #[test]
    fn the_journal_can_be_shared_between_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Journal>();
    }

    #[test]
    fn a_panic_inside_a_transaction_rolls_back_and_leaves_the_journal_usable() {
        let journal = Arc::new(Journal::open_in_memory(clock()).unwrap());
        let inside = Arc::clone(&journal);
        let panicked = std::thread::spawn(move || {
            let _ = inside.with_transaction(|tx| -> Result<()> {
                tx.observe_epoch(9)?;
                panic!("interrupted mid-transaction")
            });
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(journal.known_epoch().unwrap(), 0);
        journal.observe_epoch(3).unwrap();
        assert_eq!(journal.known_epoch().unwrap(), 3);
    }

    #[test]
    fn an_ignored_storage_failure_still_aborts_the_transaction() {
        let journal = Journal::open_in_memory(clock()).unwrap();
        let result = journal.with_transaction(|tx| {
            tx.observe_epoch(4)?;
            // Swallow a storage failure, as a careless caller might.
            let _ = tx.exec("INSERT INTO no_such_table VALUES (1)", rusqlite::params![]);
            Ok(())
        });
        assert!(matches!(result, Err(JournalError::Aborted(_))));
        assert_eq!(journal.known_epoch().unwrap(), 0);
    }
}
