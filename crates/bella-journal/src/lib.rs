//! Worker-local recovery journal (SQLite).
//!
//! The worker daemon records every protocol step here **before** it acts on
//! it. After a crash, a restart, a sleep or a controller outage the journal
//! therefore says exactly which attempts the machine holds, which of them
//! were authorised to start, which processes belong to them, which events
//! the controller has not acknowledged and which cleanup is unfinished. It
//! needs no controller to answer any of that.
//!
//! # What is recorded
//!
//! * **Identity and counters** — the [`Enrollment`], the current machine
//!   policy, the connection generation, the newest controller epoch seen,
//!   and the machine owner's pause and drain settings.
//! * **Attempts** — one [`AttemptRecord`] per accepted offer, moved through
//!   [`LocalAttemptState`](bella_core::protocol::LocalAttemptState) by
//!   operations that check the current state first.
//! * **Event outbox** — events for the controller, numbered per attempt
//!   from 1 without gaps, kept until acknowledged.
//! * **Seen commands** — the result of each command id, so a
//!   retransmission is answered instead of applied twice.
//! * **Log offsets** — how far each log stream was produced and confirmed.
//!   The bytes themselves stay in the worker's spool files.
//!
//! # Rules the worker follows
//!
//! 1. **Recover first.** After [`Journal::open`], call [`Journal::recover`]
//!    with the current boot id, then [`Journal::expire`]. Until `recover`
//!    has run, attempts cannot be changed.
//! 2. **Journal, then act.** Hold the offer before replying. Journal the
//!    start permit before preparing. Record the workspace before creating
//!    it and **the scope before the first process is spawned**
//!    ([`JournalTx::set_scope`]). Record a log offset before sending the
//!    bytes it covers.
//! 3. **Only `Fresh` launches.** [`Journal::authorize_start`] answers
//!    `Fresh` once per attempt, ever.
//! 4. **Bundle what belongs together.** A state change, the event that
//!    reports it and the result of the command that caused it go into one
//!    [`Journal::with_transaction`].
//! 5. **Resources stay held until release.** An attempt counts in
//!    [`Journal::held_resources`] from the accepted offer until it is
//!    `Released`, through cancellation, quarantine and reboots alike.
//!
//! The allowed state changes are the table on [`can_transition`].
//!
//! # More than one process
//!
//! The daemon owns the journal. A command-line tool may open the same file
//! with [`Journal::open_existing`] to change pause or drain while the
//! daemon runs; the two take turns through SQLite's locking. Two daemons on
//! one journal are not supported and not detected here: the gate that
//! `recover` opens is per process.
//!
//! # A full disk
//!
//! See [`ReserveFile`] and [`Journal::release_reserve`].

mod attempt;
mod codec;
mod error;
mod journal;
mod logs;
mod outbox;
mod recovery;
mod reserve;
mod schema;
mod tx;

pub use attempt::{
    AttemptRecord, CancelRequest, ExpiryReason, HoldOutcome, RevokeOutcome, StartDecision,
    StepRecord, can_transition, local_stop_deadline,
};
pub use error::{JournalError, Result};
pub use journal::Journal;
pub use logs::{LogCursor, LogOffsets};
pub use recovery::{ExpireReport, ExpiredAttempt, RecoveredAttempt, RecoveryAction};
pub use reserve::{RESERVE_BYTES, ReserveFile};
pub use schema::SCHEMA_VERSION;
pub use tx::{Enrollment, JournalTx};
