//! Worker daemon: admission, attempt runner, logs and cleanup.
//!
//! One daemon runs per physical machine. It is a library: [`run`] takes its
//! collaborators as trait objects ([`Connector`] for the control connection,
//! [`ObjectStore`] for the controller's object store, [`Containment`] for
//! process scopes, [`Clock`] for wall time) and never opens a socket or
//! configures TLS itself. The `bella` binary wires it to the real controller.
//!
//! # What the daemon guarantees
//!
//! * **One daemon per data directory.** [`run`] takes an exclusive lock on
//!   `worker.lock`; a second daemon fails with
//!   [`WorkerError::AlreadyRunning`].
//! * **Recover first.** The journal is recovered against the current boot
//!   before anything else. Attempts from an earlier boot are released once
//!   their workspace is gone, without signalling anything. Attempts of this
//!   boot are re-adopted by scope, stopped and reported as interrupted,
//!   because their output pipes cannot be re-attached.
//! * **Journal, then act.** Every protocol step is journaled before it is
//!   acted on, and every state change is journaled together with the event
//!   that reports it. Events and log bytes are resent after a reconnect from
//!   the last acknowledged position, in order and without duplicates in the
//!   journal's numbering.
//! * **An offer executes nothing.** Accepting one journals a hold. Only a
//!   `Start` whose permit the journal reports as fresh launches the runner; a
//!   retransmitted `Start` is acknowledged as a duplicate.
//! * **Local protection needs no controller.** The protection loop samples,
//!   budgets, limits and evicts on its own thread whether or not a
//!   connection exists, and checks every attempt's execution authority on
//!   each pass, so a machine that slept stops stale work before anything
//!   else runs.
//! * **Resources stay held until release is proven.** An attempt is released
//!   only after its scope was verified empty and its workspace removed.
//!   Anything else is quarantined, keeps its reservation and is retried.
//! * **Steps see a constructed environment.** Nothing from the daemon's own
//!   environment reaches a step except the listed system essentials; no
//!   token and no controller address is ever exported.
//!
//! # Data directory
//!
//! ```text
//! <data_dir>/worker.lock        single-instance lock
//! <data_dir>/journal.sqlite     recovery journal (bella-journal)
//! <data_dir>/machine-token      machine token, private file
//! <data_dir>/commissioning.json cached containment self-test
//! <data_dir>/inventory.json     last inventory, for revision numbering
//! <data_dir>/cache/             local object cache (bella-store layout)
//! <data_dir>/spool/<attempt>/   log spool segments
//! ```
//!
//! # Step semantics
//!
//! Steps run in the order of the specification, which is already
//! topological. Tasks and tests run one at a time. A service starts in the
//! background at its position and the runner waits there for its readiness
//! probe. A step is `Skipped` when a step it depends on did not succeed (for
//! a service: is not ready and alive), and once a stop was requested. Steps
//! that do not depend on a failed step still run; the attempt's outcome is
//! the first failure. The adapters can only stop a scope as a whole, so a
//! step that exceeds its timeout ends every process of the attempt,
//! services included. Cleanup steps always run last.

mod b64;
mod cache;
mod collect;
mod conn;
mod core;
mod enroll;
mod env;
mod inventory;
mod lock;
mod readiness;
mod runner;
mod selftest;
mod spool;

pub use crate::core::{TelemetrySource, WorkerError};
pub use enroll::{
    EnrollmentError, LocalControlHandle, TOKEN_FILE, load_enrollment, load_machine_token,
    local_control, store_enrollment,
};
pub use inventory::executor_info;
pub use selftest::{CommandCheck, SelfTestReport, self_test};

use bella_contain::Containment;
use bella_core::defaults;
use bella_core::machine::Inventory;
use bella_core::protocol::{ControllerMsg, WorkerMsg};
use bella_core::store::ObjectStore;
use bella_core::{BootId, Clock};
use bella_materialize::MaterializeOptions;
use bella_protect::ProtectConfig;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Step name under which the worker writes log lines that belong to the
/// attempt as a whole rather than to one step (preparation, collection and
/// cleanup notes). Always on the `System` stream.
pub const SYSTEM_STEP: &str = "_bella";

/// File names inside the data directory.
pub const JOURNAL_FILE: &str = "journal.sqlite";
pub const LOCK_FILE: &str = "worker.lock";
pub const CACHE_DIR: &str = "cache";
pub const SPOOL_DIR: &str = "spool";

/// Why a control connection could not be opened.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ConnectError {
    pub message: String,
}

impl ConnectError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// One open control connection. Dropping or closing either half means the
/// connection is gone; the worker then reconnects through its
/// [`Connector`].
pub struct Connection {
    pub tx: tokio::sync::mpsc::Sender<WorkerMsg>,
    pub rx: tokio::sync::mpsc::Receiver<ControllerMsg>,
}

/// The future a [`Connector`] returns.
pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Connection, ConnectError>> + Send + 'a>>;

/// Opens control connections to the controller. The production
/// implementation is the worker socket of `bella-client`; tests use a
/// scripted controller over in-process channels.
pub trait Connector: Send + Sync {
    /// Open one connection. The worker sends `Hello` itself as the first
    /// message. A failure is retried with exponential backoff and jitter.
    fn connect(&self) -> ConnectFuture<'_>;
}

/// Intervals and limits of the daemon. The defaults are
/// `bella_core::defaults` where one exists there.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Period of the local protection loop.
    pub protection_interval: Duration,
    /// Heartbeat period. `None` uses the interval the controller names in
    /// `Welcome`.
    pub heartbeat_interval: Option<Duration>,
    /// First and largest delay between connection attempts. Each failure
    /// doubles the delay; a random part of up to half is added.
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    /// How long to wait for `Welcome` after `Hello`.
    pub handshake_timeout: Duration,
    /// How long a forced kill may take to empty a scope before the attempt
    /// is quarantined.
    pub kill_wait: Duration,
    /// How often a running child and the stop conditions are checked.
    pub poll_interval: Duration,
    /// After a step's process exits, how long its output pipes are still
    /// read to end-of-file before the step is reported. A background
    /// process can hold a pipe open; its later output is still captured
    /// until the scope is stopped.
    pub log_drain_grace: Duration,
    /// Log bytes that may be sent and not yet acknowledged, over all
    /// streams of a connection.
    pub max_unacked_log_bytes: u64,
    /// Size at which a spool segment file is closed and a new one started.
    /// Acknowledged segments are deleted.
    pub spool_segment_bytes: u64,
    /// How often an attempt's workspace is measured against its scratch
    /// disk reservation.
    pub disk_check_interval: Duration,
    /// How often cleanup of a quarantined attempt is retried.
    pub quarantine_retry_interval: Duration,
    /// How often the object cache is collected, and how long an object
    /// that no held attempt needs is kept after it was fetched.
    pub cache_gc_interval: Duration,
    pub cache_retention: Duration,
    /// How long a released and fully acknowledged attempt stays in the
    /// journal, and how long a released attempt the controller disowned is
    /// kept for one more chance to deliver its events.
    pub released_retention: Duration,
    /// How long results of seen commands are remembered.
    pub command_retention: Duration,
    /// Tuning of the protection decisions.
    pub protect: ProtectConfig,
    /// Limits of one snapshot materialization. `max_total_bytes` is
    /// lowered to the attempt's scratch disk reservation when that is set.
    pub materialize: MaterializeOptions,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            protection_interval: defaults::PROTECTION_INTERVAL,
            heartbeat_interval: None,
            reconnect_min: Duration::from_millis(500),
            reconnect_max: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(30),
            kill_wait: Duration::from_secs(10),
            poll_interval: Duration::from_millis(25),
            log_drain_grace: Duration::from_secs(2),
            max_unacked_log_bytes: 16 * defaults::LOG_CHUNK_BYTES as u64,
            spool_segment_bytes: defaults::MIB,
            disk_check_interval: Duration::from_secs(5),
            quarantine_retry_interval: Duration::from_secs(10),
            cache_gc_interval: Duration::from_secs(10 * 60),
            cache_retention: Duration::from_secs(7 * 24 * 60 * 60),
            released_retention: Duration::from_secs(60 * 60),
            command_retention: Duration::from_secs(24 * 60 * 60),
            protect: ProtectConfig::default(),
            materialize: MaterializeOptions {
                max_total_bytes: u64::MAX,
                max_files: 5_000_000,
                threads: 4,
            },
        }
    }
}

/// Replacements for what the daemon would otherwise read from the machine.
/// Production leaves all of them `None`.
#[derive(Default)]
pub struct Hooks {
    /// Telemetry source used instead of `bella_probe::Sampler`.
    pub telemetry: Option<Box<dyn TelemetrySource>>,
    /// Inventory used instead of probing the machine. With it the
    /// containment self-test and the capability probe are skipped.
    pub inventory: Option<Inventory>,
    /// Boot id used instead of `bella_probe::boot_id()`.
    pub boot_id: Option<BootId>,
}

/// Everything [`run`] needs.
pub struct WorkerConfig {
    /// Holds the journal, the object cache, the log spool and the lock.
    pub data_dir: PathBuf,
    /// Attempt workspaces are `<work_root>/<short id>`. Keep it short: the
    /// Windows path limit applies to what is materialized below it.
    pub work_root: PathBuf,
    /// The controller's object store. It is blocking and only ever called
    /// from blocking threads.
    pub remote: Arc<dyn ObjectStore>,
    /// Opens control connections.
    pub connector: Arc<dyn Connector>,
    /// `None` uses [`bella_contain::native`].
    pub containment: Option<Box<dyn Containment>>,
    pub clock: Arc<dyn Clock>,
    pub tuning: Tuning,
    pub worker_version: String,
    pub hooks: Hooks,
}

/// Run the worker daemon until `shutdown` completes.
///
/// The journal in `data_dir` must already hold an enrollment (see
/// [`store_enrollment`]). On shutdown every running attempt is stopped,
/// cleaned up and released in the journal; its events stay queued for the
/// next connection.
pub async fn run(
    config: WorkerConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<(), WorkerError> {
    conn::run(config, shutdown).await
}
