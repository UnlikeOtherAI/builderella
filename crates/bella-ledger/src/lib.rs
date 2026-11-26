//! Controller persistence and transactional state transitions (SQLite).
//!
//! The ledger is the controller's only durable state. Every operation runs
//! in one SQLite transaction: it either commits completely, together with
//! the event rows that describe it, or returns an error and changes
//! nothing. The invariants of `docs/architecture.md` §3 are checked inside
//! those transactions, so no interleaving of callers can break them:
//!
//! * [`Ledger::create_offer`] checks capacity, concurrency and "no
//!   unresolved attempt" in the transaction that creates the reservation,
//!   and a unique index allows one held attempt per job.
//! * [`Ledger::commit_start`] persists the start permit. From then on only
//!   a release proof, a proven reboot or an operator frees the reservation;
//!   rejection, revocation and expiry are refused.
//! * A reservation is released by exactly one statement that flips
//!   `held` from 1 to 0, so a release applies at most once.
//! * Attempt events are fenced by generation and deduplicated by sequence.
//! * A machine is offered nothing until it has reconciled since it last
//!   connected, in the current controller epoch.
//! * Opening the database bumps the controller epoch, and every write
//!   checks that its ledger still holds the newest epoch, so a superseded
//!   controller process can read but no longer change anything.
//!
//! The API is synchronous. A [`Ledger`] wraps one connection behind a
//! mutex; call it from a blocking thread.
//!
//! # Event kinds
//!
//! Each transition appends one [`bella_core::api::EventRecord`]:
//!
//! | Entity | Kinds |
//! | --- | --- |
//! | fleet | `fleet.created`, `fleet.controller_started`, `principal.created`, `principal.revoked`, `pairing_code.created`, `pairing_code.consumed` |
//! | machine | `machine.enrolled`, `machine.connected`, `machine.disconnected`, `machine.reconciled`, `machine.inventory_changed`, `machine.policy_changed`, `machine.renamed`, `machine.flags_changed`, `machine.local_control_changed`, `machine.retired`, `machine.revoked`, and one pair per controller-set condition: `machine.draining` / `machine.draining_cleared`, `machine.stale` / `machine.stale_cleared`, `machine.unreachable` / `machine.unreachable_cleared`, and so on for every `AvailabilityFlag` |
//! | project | `project.created`, `project.policy_changed` |
//! | snapshot | `snapshot.registered` |
//! | job | `job.queued`, `job.offered`, `job.preparing`, `job.executing`, `job.collecting`, `job.cleaning`, `job.quarantined`, `job.finished`, `job.cancel_requested` |
//! | attempt | `attempt.offered`, `attempt.accepted`, `attempt.rejected`, `attempt.revoked`, `attempt.expired`, `attempt.cancelled`, `attempt.forgotten`, `attempt.start_committed`, `attempt.start_delivered`, `attempt.preparing`, `attempt.started`, `attempt.step_changed`, `attempt.environment_ready`, `attempt.collecting`, `attempt.artifact_published`, `attempt.exited`, `attempt.cleanup`, `attempt.released`, `attempt.quarantined`, `attempt.uncertain`, `attempt.uncertain_cleared`, `attempt.log_truncated` |
//! | environment | `environment.ready`, `environment.renewed`, `environment.finishing`, `environment.ended`, `environment.quarantined` |
//!
//! `attempt.rejected`, `.revoked`, `.expired`, `.cancelled` and
//! `.forgotten` release the reservation of an offer that never started;
//! `attempt.released` releases one that did, and its `reason` is `proof`,
//! `reboot`, `never_delivered` or `operator`.
//!
//! Heartbeats, lease touches, authority renewals and queue-reason updates
//! are observations rather than transitions and write no event.

mod artifacts;
mod attempts;
mod db;
mod environments;
mod error;
mod events;
mod handshake;
mod identity;
mod jobs;
mod logs;
mod machines;
mod phase;
mod projects;
mod reconcile;
mod records;
mod schema;
mod views;

pub use error::LedgerError;
pub use records::*;
pub use rusqlite;

use bella_core::ids::FleetId;
use bella_core::{Clock, UnixMillis};
use db::Ctx;
use rusqlite::{Connection, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub type Result<T, E = LedgerError> = std::result::Result<T, E>;

/// The controller's durable state. See the crate documentation.
pub struct Ledger {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
    epoch: u64,
    fleet_id: FleetId,
    log_dir: Option<PathBuf>,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("epoch", &self.epoch)
            .field("fleet_id", &self.fleet_id)
            .field("log_dir", &self.log_dir)
            .finish_non_exhaustive()
    }
}

impl Ledger {
    /// Open (creating if needed) the database at `path` and take it over:
    /// apply pending migrations, bump and persist the controller epoch,
    /// create the fleet id on first use and mark every machine
    /// disconnected, all in one transaction. Reservations are untouched:
    /// each machine reconciles before it is offered work again.
    ///
    /// Fails with [`LedgerError::SchemaTooNew`] when the database was
    /// written by a newer schema version. Log bytes are kept in a `logs`
    /// directory next to the database file.
    pub fn open(path: impl AsRef<Path>, clock: Arc<dyn Clock>) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        let log_dir = path.parent().unwrap_or(Path::new("")).join("logs");
        Self::init(conn, clock, Some(log_dir))
    }

    /// A private in-memory database, for tests and tools. Log bytes are
    /// kept in the database.
    pub fn open_in_memory(clock: Arc<dyn Clock>) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, clock, None)
    }

    fn init(mut conn: Connection, clock: Arc<dyn Clock>, log_dir: Option<PathBuf>) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(10))?;
        // The ledger uses more distinct statements than the default cache holds.
        conn.set_prepared_statement_cache_capacity(256);
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let now = clock.now();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema::migrate(&tx, schema::LEDGER, schema::MIGRATIONS)?;
        let (epoch, fleet_id) = schema::begin_epoch(&tx, now)?;
        tx.commit()?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
            epoch,
            fleet_id,
            log_dir,
        })
    }

    /// The controller epoch this ledger was opened with. It is one greater
    /// than the epoch of the previous open of the same database.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn fleet_id(&self) -> &FleetId {
        &self.fleet_id
    }

    /// The ledger clock's current time.
    pub fn now(&self) -> UnixMillis {
        self.clock.now()
    }

    /// Apply a component's numbered migrations in one transaction and
    /// return the resulting version. The first entry of `migrations` is
    /// version 1. Versions are tracked per component, so a second call with
    /// the same list does nothing and a longer list applies only the new
    /// entries. Fails with [`LedgerError::SchemaTooNew`] when the database
    /// is ahead of the list.
    pub fn apply_component_migrations(&self, component: &str, migrations: &[&str]) -> Result<u32> {
        if component.is_empty() || component == schema::LEDGER {
            return Err(LedgerError::Invalid(format!(
                "`{component}` is not a valid component name"
            )));
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema::ensure_owner(&tx, self.epoch)?;
        let version = schema::migrate(&tx, component, migrations)?;
        tx.commit()?;
        Ok(version)
    }

    /// Run `f` in one write transaction on the ledger's connection. The
    /// transaction commits when `f` returns `Ok` and rolls back otherwise.
    /// Like every ledger write it is refused with
    /// [`LedgerError::StaleEpoch`] once a newer controller has opened the
    /// database. Ledger methods must not be called from inside `f`: the
    /// connection is locked for its duration.
    pub fn with_transaction<T, E>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<LedgerError>,
    {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(LedgerError::from)?;
        schema::ensure_owner(&tx, self.epoch)?;
        let out = f(&tx)?;
        tx.commit().map_err(LedgerError::from)?;
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while the lock was held dropped its transaction, which
        // rolled back, so the connection is consistent.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run a state transition in one immediate transaction, provided this
    /// ledger still owns the database.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Ctx<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema::ensure_owner(&tx, self.epoch)?;
        let ctx = Ctx {
            conn: &tx,
            now: self.clock.now(),
            epoch: self.epoch,
            fleet_id: &self.fleet_id,
            log_dir: self.log_dir.as_deref(),
        };
        let out = f(&ctx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Run queries against one consistent snapshot.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Ctx<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let ctx = Ctx {
            conn: &tx,
            now: self.clock.now(),
            epoch: self.epoch,
            fleet_id: &self.fleet_id,
            log_dir: self.log_dir.as_deref(),
        };
        f(&ctx)
    }
}
