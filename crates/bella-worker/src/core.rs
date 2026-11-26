//! The daemon's shared state: startup and recovery, the protection loop and
//! the handling of controller messages. Everything here is synchronous and
//! runs on blocking threads.

use crate::cache::ObjectCache;
use crate::lock::InstanceLock;
use crate::runner;
use crate::spool::LogHub;
use crate::{CACHE_DIR, JOURNAL_FILE, SPOOL_DIR, Tuning, WorkerConfig};
use bella_contain::{ContainError, Containment, Limits, Scope};
use bella_core::job::{Outcome, QueueReasonCode};
use bella_core::machine::{
    AvailabilityFlag, Inventory, LocalControl, MachinePolicy, Telemetry,
};
use bella_core::platform::Os;
use bella_core::protocol::{
    AttemptEvent, AttemptRef, CommandEnvelope, CommandResult, ControllerCommand, ControllerMsg,
    ExecutionAuthority, Heartbeat, LocalAttemptState, Offer, OfferDecision, PROTOCOL_VERSION,
    ReconcileAction, ReconcileVerdict, RejectCode, StepState, UsageSummary, WorkerMsg,
};
use bella_core::resources::ResourceVector;
use bella_core::snapshot::Manifest;
use bella_core::store::StoreError;
use bella_core::{AttemptId, BootId, Clock, CommandId, FleetId, MachineId, UnixMillis};
use bella_journal::{
    ExpiryReason, HoldOutcome, Journal, JournalError, JournalTx, RecoveredAttempt, RecoveryAction,
    RevokeOutcome, StartDecision,
};
use bella_probe::{ManagedUsage, Sampler};
use bella_protect::{Action, Budget, Grant, History, Input};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The daemon could not start or had to stop.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Another daemon holds the lock of this data directory.
    #[error("another worker is already running on {}", path.display())]
    AlreadyRunning { path: PathBuf },
    /// The journal holds no enrollment; see `store_enrollment`.
    #[error("this machine is not enrolled: the journal holds no enrollment")]
    NotEnrolled,
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("containment: {0}")]
    Contain(#[from] ContainError),
    #[error("object cache: {0}")]
    Store(#[from] StoreError),
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Internal(String),
}

impl WorkerError {
    pub(crate) fn io(action: &'static str, path: &Path, source: std::io::Error) -> Self {
        WorkerError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Where the protection loop gets its observations.
///
/// The daemon stamps each sample with its own clock, so an implementation
/// does not have to fill `sampled_at`.
pub trait TelemetrySource: Send {
    /// One observation of the host. `managed` is what the daemon's scopes
    /// are using.
    fn sample(&mut self, managed: ManagedUsage) -> Telemetry;
}

struct ProbeSource(Sampler);

impl TelemetrySource for ProbeSource {
    fn sample(&mut self, managed: ManagedUsage) -> Telemetry {
        self.0.sample(managed)
    }
}

/// Why an attempt is being stopped before its steps finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// The controller cancelled it, or disowned it during reconciliation.
    Cancelled,
    /// `valid_until` plus the permitted disconnected time passed.
    AuthorityLapsed,
    /// The attempt's absolute deadline passed.
    HardDeadline,
    /// The protection loop evicted it.
    Evicted {
        detail: String,
        resource_exhausted: bool,
    },
    /// Its workspace outgrew the scratch disk reservation.
    DiskExhausted { used: u64, limit: u64 },
    /// The daemon is shutting down.
    Shutdown,
    /// The journal could not record a step, so nothing more may run.
    JournalFailure(String),
}

impl StopReason {
    pub(crate) fn outcome(&self) -> Outcome {
        match self {
            StopReason::Cancelled => Outcome::Cancelled,
            StopReason::AuthorityLapsed => Outcome::InfraInterrupted {
                detail: "the execution authority lapsed without controller contact; the worker stopped the attempt on its own".to_string(),
            },
            StopReason::HardDeadline => Outcome::TimedOut { step: None },
            StopReason::Evicted {
                detail,
                resource_exhausted: true,
            } => Outcome::ResourceExhausted {
                detail: detail.clone(),
            },
            StopReason::Evicted { detail, .. } => Outcome::InfraInterrupted {
                detail: detail.clone(),
            },
            StopReason::DiskExhausted { used, limit } => Outcome::ResourceExhausted {
                detail: format!(
                    "the workspace grew to {used} bytes, above the scratch disk reservation of {limit} bytes"
                ),
            },
            StopReason::Shutdown => Outcome::InfraInterrupted {
                detail: "the worker was shut down while the attempt was running".to_string(),
            },
            StopReason::JournalFailure(error) => Outcome::InfraInterrupted {
                detail: format!("the worker's journal failed: {error}"),
            },
        }
    }
}

#[derive(Default)]
struct StopState {
    reason: Option<StopReason>,
    /// Grace between the polite stop and the kill, when the request named one.
    grace: Option<Duration>,
    finish_environment: bool,
    /// The attempt's threads have nothing left to wait for.
    done: bool,
}

/// What the daemon keeps in memory about one attempt that holds resources.
pub(crate) struct AttemptHandle {
    pub attempt: AttemptRef,
    pub offer: Arc<Offer>,
    stop: Mutex<StopState>,
    stop_changed: Condvar,
    /// The scope that owns the attempt's processes, once created.
    pub scope: Mutex<Option<Box<dyn Scope>>>,
    usage: Mutex<UsageSummary>,
    pub disk_bytes: AtomicU64,
    /// Processes may exist.
    pub started: AtomicBool,
    started_at: Mutex<Option<UnixMillis>>,
    state: Mutex<LocalAttemptState>,
    /// A runner or cleanup thread is working on the attempt.
    pub busy: AtomicBool,
    /// Why the last cleanup pass could not release the attempt.
    quarantine: Mutex<Option<String>>,
    last_cleanup_try: Mutex<Option<Instant>>,
}

impl AttemptHandle {
    pub(crate) fn new(offer: Offer, state: LocalAttemptState) -> Arc<Self> {
        Arc::new(Self {
            attempt: offer.attempt.clone(),
            offer: Arc::new(offer),
            stop: Mutex::new(StopState::default()),
            stop_changed: Condvar::new(),
            scope: Mutex::new(None),
            usage: Mutex::new(UsageSummary::default()),
            disk_bytes: AtomicU64::new(0),
            started: AtomicBool::new(false),
            started_at: Mutex::new(None),
            state: Mutex::new(state),
            busy: AtomicBool::new(false),
            quarantine: Mutex::new(None),
            last_cleanup_try: Mutex::new(None),
        })
    }

    pub(crate) fn id(&self) -> &AttemptId {
        &self.attempt.attempt_id
    }

    fn stop_state(&self) -> MutexGuard<'_, StopState> {
        self.stop.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ask the attempt to stop. The first reason stands; a later request
    /// can only shorten the grace. Returns whether this call set the reason.
    pub(crate) fn request_stop(&self, reason: StopReason, grace: Option<Duration>) -> bool {
        let mut stop = self.stop_state();
        let first = stop.reason.is_none();
        if first {
            stop.reason = Some(reason);
        }
        if let Some(grace) = grace {
            stop.grace = Some(stop.grace.map_or(grace, |current| current.min(grace)));
        }
        drop(stop);
        self.stop_changed.notify_all();
        first
    }

    pub(crate) fn stop_reason(&self) -> Option<StopReason> {
        self.stop_state().reason.clone()
    }

    pub(crate) fn stop_grace(&self) -> Option<Duration> {
        self.stop_state().grace
    }

    pub(crate) fn finish_environment(&self) {
        self.stop_state().finish_environment = true;
        self.stop_changed.notify_all();
    }

    pub(crate) fn environment_finished(&self) -> bool {
        self.stop_state().finish_environment
    }

    /// Tell the attempt's helper threads that nothing is left to do.
    pub(crate) fn mark_done(&self) {
        self.stop_state().done = true;
        self.stop_changed.notify_all();
    }

    pub(crate) fn is_done(&self) -> bool {
        self.stop_state().done
    }

    /// Sleep for `limit`, waking early when a stop or the end of an
    /// environment is requested or the attempt is done.
    pub(crate) fn sleep(&self, limit: Duration) {
        let stop = self.stop_state();
        if stop.reason.is_some() || stop.done {
            // The caller is already reacting; do not spin without pause.
            drop(stop);
            std::thread::sleep(limit.min(Duration::from_millis(5)));
            return;
        }
        let _ = self
            .stop_changed
            .wait_timeout(stop, limit)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Sleep for `limit` unless the attempt is done first.
    pub(crate) fn sleep_until_done(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut stop = self.stop_state();
        while !stop.done {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let (guard, _) = self
                .stop_changed
                .wait_timeout(stop, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            stop = guard;
        }
    }

    pub(crate) fn state(&self) -> LocalAttemptState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_state(&self, state: LocalAttemptState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }

    pub(crate) fn usage(&self) -> UsageSummary {
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        usage.disk_bytes = self.disk_bytes.load(Ordering::SeqCst);
        usage
    }

    /// Remember a measurement, keeping the highest peak seen.
    pub(crate) fn record_usage(&self, mut measured: UsageSummary) {
        let mut usage = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        measured.memory_peak_bytes = measured
            .memory_peak_bytes
            .max(measured.memory_current_bytes)
            .max(usage.memory_peak_bytes);
        measured.cpu_time_ms = measured.cpu_time_ms.max(usage.cpu_time_ms);
        *usage = measured;
    }

    /// Measure the scope if nobody else is using it right now.
    fn refresh_usage(&self) {
        let measured = match self.scope.try_lock() {
            Ok(guard) => guard.as_ref().and_then(|scope| scope.usage().ok()),
            Err(_) => None,
        };
        if let Some(measured) = measured {
            self.record_usage(measured);
        }
    }

    pub(crate) fn mark_started(&self, now: UnixMillis) {
        self.started.store(true, Ordering::SeqCst);
        *self
            .started_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(now);
    }

    pub(crate) fn quarantine_reason(&self) -> Option<String> {
        self.quarantine
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_quarantine(&self, reason: Option<String>) {
        *self
            .quarantine
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = reason;
        *self
            .last_cleanup_try
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    fn cleanup_retry_due(&self, interval: Duration) -> bool {
        self.last_cleanup_try
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none_or(|at| at.elapsed() >= interval)
    }

    fn grant(&self) -> Grant {
        let started = self.started.load(Ordering::SeqCst);
        let usage = self.usage();
        Grant {
            attempt_id: self.id().clone(),
            reserved: self.offer.reservation.clone(),
            memory_used_bytes: started.then_some(usage.memory_current_bytes),
            cpu_used_milli: None,
            disk_used_bytes: started.then_some(usage.disk_bytes),
            started,
            priority: self.offer.spec.priority,
            retry_class: self.offer.spec.retry_class,
            started_at: *self
                .started_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            suspended: false,
        }
    }
}

/// What changes with every pass of the protection loop or every command.
struct State {
    policy: MachinePolicy,
    control: LocalControl,
    /// Drain requested by the controller, on top of the owner's own.
    controller_drain: bool,
    attempts: BTreeMap<AttemptId, Arc<AttemptHandle>>,
    history: History,
    telemetry: Option<Telemetry>,
    managed: ManagedUsage,
    budget: Option<Budget>,
    admitting: bool,
    flags: Vec<AvailabilityFlag>,
    /// A journal write failed for lack of space and the reserve was released.
    disk_full: bool,
    last_applied_command: Option<CommandId>,
    last_cache_gc: Instant,
    last_journal_gc: Instant,
}

/// State of the current control connection.
#[derive(Default)]
struct ConnState {
    connected: bool,
    generation: u64,
    heartbeat_seq: u64,
    /// `Reconcile` was applied; commands are accepted and the outbox flows.
    reconciled: bool,
    /// Highest event sequence handed to this connection, per attempt.
    event_sent: HashMap<AttemptId, u64>,
}

/// The daemon.
pub(crate) struct Core {
    pub work_root: PathBuf,
    pub tuning: Tuning,
    pub clock: Arc<dyn Clock>,
    pub journal: Journal,
    pub containment: Box<dyn Containment>,
    pub cache: Arc<ObjectCache>,
    pub logs: LogHub,
    /// Events or log bytes are waiting for the connection.
    pub wake: Arc<Notify>,
    /// A heartbeat should go out now.
    pub heartbeat_wake: Arc<Notify>,
    pub machine_id: MachineId,
    pub fleet_id: FleetId,
    pub boot_id: BootId,
    pub inventory: Inventory,
    pub system_env: BTreeMap<String, String>,
    state: Mutex<State>,
    conn: Mutex<ConnState>,
    sampler: Mutex<Box<dyn TelemetrySource>>,
    /// Attempts with events the connection has not been handed yet.
    pending_events: Mutex<BTreeSet<AttemptId>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// CPU time of scopes that are gone, so the managed total only grows.
    finished_cpu_ms: AtomicU64,
    stopping: Mutex<bool>,
    stopping_changed: Condvar,
    _lock: InstanceLock,
}

fn create_dir(path: &Path) -> Result<(), WorkerError> {
    std::fs::create_dir_all(path)
        .map_err(|source| WorkerError::io("create the directory", path, source))
}

impl Core {
    /// Open the data directory, recover the journal against the current
    /// boot and settle every attempt it holds. Returns once the daemon is
    /// ready to connect.
    pub(crate) fn start(config: WorkerConfig) -> Result<Arc<Self>, WorkerError> {
        let WorkerConfig {
            data_dir,
            work_root,
            remote,
            connector: _,
            containment,
            clock,
            tuning,
            worker_version,
            hooks,
        } = config;
        create_dir(&data_dir)?;
        let lock = InstanceLock::acquire(&data_dir)?;
        create_dir(&work_root)?;
        let spool_root = data_dir.join(SPOOL_DIR);
        create_dir(&spool_root)?;

        let journal = Journal::open(data_dir.join(JOURNAL_FILE), clock.clone())?;
        let enrollment = journal.enrollment()?.ok_or(WorkerError::NotEnrolled)?;
        let boot_id = hooks.boot_id.unwrap_or_else(bella_probe::boot_id);
        let recovered = journal.recover(&boot_id)?;
        let now = clock.now();
        if let Err(error) = journal.expire(now) {
            tracing::warn!(%error, "could not expire journaled attempts at startup");
        }

        let containment = match containment {
            Some(containment) => containment,
            None => bella_contain::native()?,
        };
        let cache = Arc::new(ObjectCache::open(&data_dir.join(CACHE_DIR), remote)?);
        let inventory = match hooks.inventory {
            Some(inventory) => inventory,
            None => crate::inventory::build(&data_dir, containment.as_ref(), &worker_version),
        };
        let sampler: Box<dyn TelemetrySource> = match hooks.telemetry {
            Some(source) => source,
            None => Box::new(ProbeSource(Sampler::with_clock(&work_root, clock.clone()))),
        };
        let wake = Arc::new(Notify::new());
        let logs = LogHub::new(spool_root, tuning.spool_segment_bytes, wake.clone());
        let control = journal.local_control()?;

        let core = Arc::new(Self {
            work_root,
            tuning,
            clock,
            journal,
            containment,
            cache,
            logs,
            wake,
            heartbeat_wake: Arc::new(Notify::new()),
            machine_id: enrollment.machine_id,
            fleet_id: enrollment.fleet_id,
            boot_id,
            inventory,
            system_env: crate::env::system_env(),
            state: Mutex::new(State {
                policy: enrollment.policy,
                control,
                controller_drain: false,
                attempts: BTreeMap::new(),
                history: History::new(),
                telemetry: None,
                managed: ManagedUsage::default(),
                budget: None,
                admitting: false,
                flags: Vec::new(),
                disk_full: false,
                last_applied_command: None,
                last_cache_gc: Instant::now(),
                last_journal_gc: Instant::now(),
            }),
            conn: Mutex::new(ConnState::default()),
            sampler: Mutex::new(sampler),
            pending_events: Mutex::new(BTreeSet::new()),
            threads: Mutex::new(Vec::new()),
            finished_cpu_ms: AtomicU64::new(0),
            stopping: Mutex::new(false),
            stopping_changed: Condvar::new(),
            _lock: lock,
        });
        core.recover_attempts(recovered);
        Ok(core)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn conn(&self) -> MutexGuard<'_, ConnState> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn policy(&self) -> MachinePolicy {
        self.state().policy.clone()
    }

    pub(crate) fn handle(&self, attempt: &AttemptId) -> Option<Arc<AttemptHandle>> {
        self.state().attempts.get(attempt).cloned()
    }

    fn handles(&self) -> Vec<Arc<AttemptHandle>> {
        self.state().attempts.values().cloned().collect()
    }

    /// The operating system this daemon runs on, for platform checks.
    pub(crate) fn os(&self) -> Os {
        self.inventory.os.platform.os
    }

    /// Run journal operations as one transaction. When the volume is full
    /// the journal's reserve is released, admission closes and the
    /// transaction is tried once more, so stop, cleanup and release records
    /// can still be written.
    pub(crate) fn tx<T>(
        &self,
        work: impl Fn(&JournalTx<'_>) -> Result<T, JournalError>,
    ) -> Result<T, JournalError> {
        match self.journal.with_transaction(&work) {
            Err(error) if error.is_disk_full() => {
                tracing::error!(%error, "the journal's volume is full; releasing the reserve");
                let _ = self.journal.release_reserve();
                self.state().disk_full = true;
                self.heartbeat_wake.notify_one();
                self.journal.with_transaction(&work)
            }
            other => other,
        }
    }

    /// Note that `attempt` has events for the controller and wake the
    /// connection.
    pub(crate) fn events_pending(&self, attempt: &AttemptId) {
        self.pending_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(attempt.clone());
        self.wake.notify_one();
    }

    /// Journal one event of an attempt and queue it for the controller.
    pub(crate) fn emit(&self, attempt: &AttemptRef, event: AttemptEvent) -> Result<(), JournalError> {
        self.tx(|tx| tx.append_event(attempt, event.clone()))?;
        self.events_pending(&attempt.attempt_id);
        Ok(())
    }

    /// Keep a thread to join at shutdown.
    pub(crate) fn adopt_thread(&self, thread: JoinHandle<()>) {
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        threads.retain(|thread| !thread.is_finished());
        threads.push(thread);
    }

    // ------------------------------------------------------------ recovery

    fn recover_attempts(self: &Arc<Self>, recovered: Vec<RecoveredAttempt>) {
        let known: BTreeSet<String> = self
            .journal
            .attempts()
            .unwrap_or_default()
            .into_iter()
            .map(|record| record.attempt.attempt_id.as_str().to_string())
            .collect();
        self.logs.remove_orphans(&|name| known.contains(name));

        for found in recovered {
            let record = found.record;
            match found.action {
                RecoveryAction::DropOffer => {}
                RecoveryAction::KeepOffer => {
                    let handle = AttemptHandle::new(record.offer, LocalAttemptState::OfferHeld);
                    self.cache
                        .pin(handle.id(), [handle.offer.manifest_digest.clone()]);
                    self.state().attempts.insert(handle.id().clone(), handle);
                }
                RecoveryAction::CleanUpAfterReboot => runner::recover(
                    self,
                    record,
                    "the machine restarted while the attempt was running",
                    Some("machine rebooted: the boot id changed, so no process of the attempt can have survived"),
                ),
                RecoveryAction::ResumeUnspawned => runner::recover(
                    self,
                    record,
                    "the worker restarted before the attempt started any process",
                    Some("no containment scope was created for the attempt, so it never had a process"),
                ),
                RecoveryAction::Readopt => runner::recover(
                    self,
                    record,
                    "the worker restarted while the attempt was running; its output could not be re-attached, so it was stopped",
                    None,
                ),
            }
        }
        // Released attempts whose events or logs are still owed.
        for record in self.journal.attempts().unwrap_or_default() {
            if record.state == LocalAttemptState::Released {
                if record.last_event_seq > record.acked_event_seq {
                    self.events_pending(&record.attempt.attempt_id);
                }
                self.logs.open(&record.attempt, &self.journal);
            }
        }
    }

    /// Track an attempt that holds resources.
    pub(crate) fn insert_handle(&self, handle: Arc<AttemptHandle>) {
        self.state().attempts.insert(handle.id().clone(), handle);
    }

    /// The attempt was released: forget its handle and what it pinned.
    pub(crate) fn released(&self, handle: &AttemptHandle) {
        let usage = handle.usage();
        self.finished_cpu_ms
            .fetch_add(usage.cpu_time_ms, Ordering::SeqCst);
        self.state().attempts.remove(handle.id());
        self.cache.unpin(handle.id());
        if let Some(logs) = self.logs.get(handle.id()) {
            logs.close();
        }
        handle.set_state(LocalAttemptState::Released);
        handle.mark_done();
        self.events_pending(handle.id());
        self.heartbeat_wake.notify_one();
    }

    /// Drop an offer that never started: it holds nothing any more.
    fn drop_offer(&self, attempt: &AttemptId) {
        if let Some(handle) = self.state().attempts.remove(attempt) {
            handle.mark_done();
        }
        self.cache.unpin(attempt);
        self.heartbeat_wake.notify_one();
    }

    // ----------------------------------------------------- protection loop

    /// Run the protection loop on its own thread until [`Core::shutdown`].
    pub(crate) fn spawn_protection_loop(self: &Arc<Self>) -> Result<(), WorkerError> {
        let core = self.clone();
        let thread = std::thread::Builder::new()
            .name("bella-protect".to_string())
            .spawn(move || {
                loop {
                    core.tick();
                    let stopping = core
                        .stopping
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if *stopping {
                        return;
                    }
                    let (stopping, _) = core
                        .stopping_changed
                        .wait_timeout(stopping, core.tuning.protection_interval)
                        .unwrap_or_else(PoisonError::into_inner);
                    if *stopping {
                        return;
                    }
                }
            })
            .map_err(|error| WorkerError::Internal(format!("could not start a thread: {error}")))?;
        self.adopt_thread(thread);
        Ok(())
    }

    fn sample(&self, managed: ManagedUsage) -> Telemetry {
        let mut sample = self
            .sampler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sample(managed);
        sample.sampled_at = self.clock.now();
        sample
    }

    /// One pass of the local protection loop. It needs no controller.
    pub(crate) fn tick(self: &Arc<Self>) {
        let now = self.clock.now();

        // Authority and offer expiry first: after a sleep or a clock jump
        // stale work is stopped before anything else is decided.
        match self.journal.expire(now) {
            Ok(report) => {
                for offer in report.released_offers {
                    self.drop_offer(&offer.attempt_id);
                }
                for expired in report.stop {
                    if let Some(handle) = self.handle(&expired.attempt.attempt_id) {
                        let reason = match expired.reason {
                            ExpiryReason::HardDeadline => StopReason::HardDeadline,
                            ExpiryReason::AuthorityLapsed => StopReason::AuthorityLapsed,
                        };
                        if handle.request_stop(reason, None) {
                            tracing::warn!(attempt = %handle.id(), reason = ?expired.reason, "stopping an attempt whose authority ended");
                        }
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "could not check execution authorities"),
        }
        let control = self.journal.local_control().ok();

        let handles = self.handles();
        let mut managed = ManagedUsage {
            cpu_time_ms_total: self.finished_cpu_ms.load(Ordering::SeqCst),
            memory_bytes: 0,
        };
        for handle in &handles {
            handle.refresh_usage();
            let usage = handle.usage();
            managed.cpu_time_ms_total = managed.cpu_time_ms_total.saturating_add(usage.cpu_time_ms);
            managed.memory_bytes = managed
                .memory_bytes
                .saturating_add(usage.memory_current_bytes);
        }
        let telemetry = self.sample(managed);

        let mut evictions = Vec::new();
        let changed = {
            let mut guard = self.state();
            let state = &mut *guard;
            if let Some(control) = control {
                state.control = control;
            }
            state.telemetry = Some(telemetry);
            state.managed = managed;
            let grants: Vec<Grant> = state.attempts.values().map(|h| h.grant()).collect();
            let mut effective = state.control;
            effective.draining |= state.controller_drain;
            let input = Input {
                policy: &state.policy,
                hardware: &self.inventory.hardware,
                telemetry: state.telemetry.as_ref(),
                now,
                grants: &grants,
                control: effective,
                config: &self.tuning.protect,
            };
            state.history.observe(&input);
            let proposed = bella_protect::actions(&input, &state.history);
            let mut applied = Vec::with_capacity(proposed.len());
            for action in proposed {
                match &action {
                    Action::StopAdmission { reason } => {
                        tracing::warn!(%reason, "admission stopped by the protection loop");
                        applied.push(action);
                    }
                    Action::ResumeAdmission => {
                        tracing::info!("admission resumed by the protection loop");
                        applied.push(action);
                    }
                    Action::SetCpuLimit {
                        attempt_id,
                        cpu_milli,
                    } => {
                        let done = state
                            .attempts
                            .get(attempt_id)
                            .is_some_and(|handle| set_cpu_limit(handle, &state.policy, *cpu_milli));
                        if done {
                            applied.push(action);
                        }
                    }
                    Action::Evict { attempt_id, reason } => {
                        if let Some(handle) = state.attempts.get(attempt_id) {
                            let grant = handle.grant();
                            let over = grant.memory_excess()
                                > self.tuning.protect.memory_excess_tolerance_bytes;
                            let detail = if over {
                                format!(
                                    "evicted for exceeding its memory reservation: used {} bytes (peak {} bytes) against a reservation of {} bytes; {reason}",
                                    grant.memory_in_use(),
                                    handle.usage().memory_peak_bytes,
                                    grant.reserved.memory_bytes
                                )
                            } else {
                                format!("evicted to protect the machine: {reason}")
                            };
                            evictions.push((handle.clone(), detail, over));
                            applied.push(action);
                        }
                    }
                }
            }
            state.history.record(&applied, now);
            let budget = bella_protect::budget(&input);
            let stopped = state.history.admission_stopped().is_some();
            let mut flags = budget.flags.clone();
            if stopped {
                flags.push(AvailabilityFlag::PressureLimited);
            }
            if state.disk_full {
                flags.push(AvailabilityFlag::DiskLow);
            }
            if state
                .attempts
                .values()
                .any(|h| h.state() == LocalAttemptState::Quarantined)
            {
                flags.push(AvailabilityFlag::Quarantined);
            }
            flags.sort();
            flags.dedup();
            let admitting = budget.admitting && !stopped && !state.disk_full;
            let changed = admitting != state.admitting || flags != state.flags;
            state.admitting = admitting;
            state.flags = flags;
            state.budget = Some(budget);
            changed
        };
        for (handle, detail, over) in evictions {
            tracing::warn!(attempt = %handle.id(), %detail, "evicting an attempt");
            let _ = self.journal.request_cancel(&handle.attempt, &detail);
            handle.request_stop(
                StopReason::Evicted {
                    detail,
                    resource_exhausted: over,
                },
                None,
            );
        }
        if changed {
            self.heartbeat_wake.notify_one();
        }
        self.housekeeping(now);
    }

    /// Retry cleanup of quarantined attempts, restore the journal reserve
    /// and collect what is no longer needed.
    fn housekeeping(self: &Arc<Self>, now: UnixMillis) {
        for handle in self.handles() {
            if handle.state() == LocalAttemptState::Quarantined
                && handle.cleanup_retry_due(self.tuning.quarantine_retry_interval)
                && !handle.busy.swap(true, Ordering::SeqCst)
            {
                runner::retry_cleanup(self, handle);
            }
        }

        let (cache_due, journal_due, disk_full, idle) = {
            let mut state = self.state();
            let cache_due = state.last_cache_gc.elapsed() >= self.tuning.cache_gc_interval;
            if cache_due {
                state.last_cache_gc = Instant::now();
            }
            let journal_due =
                state.last_journal_gc.elapsed() >= self.tuning.quarantine_retry_interval;
            if journal_due {
                state.last_journal_gc = Instant::now();
            }
            (
                cache_due,
                journal_due,
                state.disk_full,
                state.attempts.is_empty(),
            )
        };
        if disk_full && idle && self.journal.restore_reserve().is_ok() && self.journal.reserve_present()
        {
            self.state().disk_full = false;
            self.heartbeat_wake.notify_one();
        }
        if journal_due {
            match self
                .journal
                .garbage_collect(now, self.tuning.released_retention)
            {
                Ok(removed) => {
                    for attempt in removed {
                        self.logs.remove(&attempt);
                    }
                }
                Err(error) => tracing::warn!(%error, "could not collect finished attempts"),
            }
            let _ = self.journal.prune(now, self.tuning.command_retention);
        }
        if cache_due
            && let Err(error) = self.cache.gc(self.tuning.cache_retention)
        {
            tracing::warn!(%error, "object cache collection failed");
        }
    }

    // ------------------------------------------------------------ shutdown

    /// Stop every running attempt, wait for its cleanup and stop the
    /// protection loop.
    pub(crate) fn shutdown(&self) {
        for handle in self.handles() {
            if handle.state() != LocalAttemptState::OfferHeld {
                handle.request_stop(StopReason::Shutdown, None);
            }
        }
        // Runners first: the protection loop keeps watching while they
        // clean up.
        loop {
            let busy = self
                .handles()
                .iter()
                .any(|handle| handle.busy.load(Ordering::SeqCst));
            if !busy {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        *self
            .stopping
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.stopping_changed.notify_all();
        let threads: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self.threads.lock().unwrap_or_else(PoisonError::into_inner),
        );
        for thread in threads {
            let _ = thread.join();
        }
    }

    // ---------------------------------------------------------- connection

    /// A new connection is about to say `Hello`.
    pub(crate) fn hello(&self) -> Result<WorkerMsg, JournalError> {
        let generation = self.journal.next_connection_generation()?;
        let attempts = self.journal.reports()?;
        let known_epoch = self.journal.known_epoch()?;
        let local_control = self.journal.local_control()?;
        {
            let mut conn = self.conn();
            *conn = ConnState {
                connected: false,
                generation,
                ..ConnState::default()
            };
        }
        Ok(WorkerMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            machine_id: self.machine_id.clone(),
            boot_id: self.boot_id.clone(),
            connection_generation: generation,
            known_epoch,
            inventory: Box::new(self.inventory.clone()),
            policy_revision: self.state().policy.revision,
            local_control,
            attempts,
        })
    }

    /// Apply `Welcome`. An error means the connection must be dropped.
    pub(crate) fn welcome(
        &self,
        protocol_version: u32,
        controller_epoch: u64,
        fleet_id: &FleetId,
        policy: &MachinePolicy,
    ) -> Result<(), String> {
        if protocol_version != PROTOCOL_VERSION {
            return Err(format!(
                "the controller speaks protocol {protocol_version}, this worker speaks {PROTOCOL_VERSION}"
            ));
        }
        if *fleet_id != self.fleet_id {
            return Err(format!(
                "the controller belongs to fleet {fleet_id}, this worker to {}",
                self.fleet_id
            ));
        }
        self.journal
            .observe_epoch(controller_epoch)
            .map_err(|error| error.to_string())?;
        self.adopt_policy(policy);
        // Everything unacknowledged goes out again, in order.
        self.logs.reset_sent();
        let unacked: Vec<AttemptId> = self
            .journal
            .attempts()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|record| record.last_event_seq > record.acked_event_seq)
            .map(|record| record.attempt.attempt_id)
            .collect();
        self.pending_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(unacked);
        self.conn().connected = true;
        Ok(())
    }

    pub(crate) fn disconnected(&self) {
        let mut conn = self.conn();
        conn.connected = false;
        conn.reconciled = false;
    }

    /// Take a policy whose revision is newer than the one in force.
    fn adopt_policy(&self, policy: &MachinePolicy) -> bool {
        let current = self.state().policy.revision;
        if policy.revision <= current {
            return false;
        }
        match self.journal.set_policy(policy) {
            Ok(_) => {
                self.state().policy = policy.clone();
                self.heartbeat_wake.notify_one();
                true
            }
            Err(error) => {
                tracing::warn!(%error, "could not adopt the controller's policy");
                false
            }
        }
    }

    /// The next heartbeat of the current connection.
    pub(crate) fn heartbeat(&self) -> Result<WorkerMsg, JournalError> {
        let mut attempts = self.journal.reports()?;
        let (generation, seq) = {
            let mut conn = self.conn();
            conn.heartbeat_seq += 1;
            (conn.generation, conn.heartbeat_seq)
        };
        let now = self.clock.now();
        let state = self.state();
        for report in &mut attempts {
            if let Some(handle) = state.attempts.get(&report.attempt.attempt_id)
                && handle.started.load(Ordering::SeqCst)
            {
                report.usage = Some(handle.usage());
            }
        }
        let effective_budget = match &state.budget {
            Some(budget) => budget.ceiling.clone(),
            None => state.policy.managed_ceiling(&self.inventory.hardware),
        };
        let mut local_control = state.control;
        local_control.draining |= state.controller_drain;
        let heartbeat = Heartbeat {
            machine_id: self.machine_id.clone(),
            boot_id: self.boot_id.clone(),
            connection_generation: generation,
            seq,
            sent_at: now,
            inventory_revision: self.inventory.revision,
            policy_revision: state.policy.revision,
            last_applied_command: state.last_applied_command.clone(),
            telemetry: state.telemetry.clone().unwrap_or_default(),
            local_control,
            effective_budget,
            admitting: state.admitting,
            flags: state.flags.clone(),
            attempts,
        };
        Ok(WorkerMsg::Heartbeat {
            heartbeat: Box::new(heartbeat),
        })
    }

    /// Events and log chunks to send next. Nothing flows before the
    /// controller's reconciliation was applied.
    pub(crate) fn collect_outbound(&self) -> Vec<WorkerMsg> {
        if !self.conn().reconciled {
            return Vec::new();
        }
        let mut out = Vec::new();
        let pending: Vec<AttemptId> = std::mem::take(
            &mut *self
                .pending_events
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
        .into_iter()
        .collect();
        for attempt in pending {
            let Ok(events) = self.journal.unacked_events(&attempt) else {
                continue;
            };
            let mut conn = self.conn();
            let sent = conn.event_sent.entry(attempt).or_insert(0);
            for event in events {
                if event.seq > *sent {
                    *sent = event.seq;
                    out.push(WorkerMsg::AttemptEvent { event });
                }
            }
        }
        let chunks = self
            .logs
            .next_chunks(&self.journal, self.tuning.max_unacked_log_bytes, 32);
        out.extend(chunks.into_iter().map(|chunk| WorkerMsg::Log { chunk }));
        out
    }

    /// Handle one message from the controller and return the replies.
    /// `Err` asks for the connection to be closed.
    pub(crate) fn on_message(self: &Arc<Self>, message: ControllerMsg) -> Result<Vec<WorkerMsg>, String> {
        match message {
            ControllerMsg::Welcome { .. } => Ok(Vec::new()),
            ControllerMsg::Ping { nonce } => Ok(vec![WorkerMsg::Pong { nonce }]),
            ControllerMsg::Close { reason } => Err(format!("closed by the controller: {reason}")),
            ControllerMsg::EventsAcked {
                attempt_id,
                through_seq,
            } => {
                if let Err(error) = self.journal.ack_events(&attempt_id, through_seq) {
                    tracing::debug!(attempt = %attempt_id, %error, "ignoring an event acknowledgement");
                }
                Ok(Vec::new())
            }
            ControllerMsg::LogsAcked {
                attempt_id,
                step,
                stream,
                through_offset,
            } => {
                self.logs
                    .ack(&self.journal, &attempt_id, &step, stream, through_offset);
                Ok(Vec::new())
            }
            ControllerMsg::Command { envelope } => Ok(self.on_command(envelope)),
        }
    }

    fn on_command(self: &Arc<Self>, envelope: CommandEnvelope) -> Vec<WorkerMsg> {
        let id = envelope.command_id.clone();
        let ack = |result: CommandResult| WorkerMsg::CommandAck {
            command_id: id.clone(),
            result,
        };

        // A command from an older controller epoch is never applied.
        if let Err(error) = self.journal.observe_epoch(envelope.controller_epoch) {
            return match error.command_result() {
                Some(result) => vec![ack(result)],
                None => {
                    tracing::error!(%error, "journal failure while checking a command's epoch");
                    Vec::new()
                }
            };
        }

        // A retransmission is answered from the journal.
        match self.journal.command_result(&id) {
            Ok(Some(stored)) => {
                if let ControllerCommand::Offer { offer } = &envelope.command {
                    return vec![WorkerMsg::OfferReply {
                        command_id: id.clone(),
                        attempt: offer.attempt.clone(),
                        decision: stored_offer_decision(&stored),
                    }];
                }
                let result = match stored {
                    CommandResult::Applied | CommandResult::Duplicate => CommandResult::Duplicate,
                    rejected => rejected,
                };
                return vec![ack(result)];
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, "journal failure while looking up a command");
                return Vec::new();
            }
        }

        let reconciled = self.conn().reconciled;
        if let ControllerCommand::Reconcile { verdicts } = &envelope.command {
            self.reconcile(verdicts);
            let _ = self.journal.record_command(&id, &CommandResult::Applied);
            self.conn().reconciled = true;
            self.applied(&id);
            self.wake.notify_one();
            self.heartbeat_wake.notify_one();
            return vec![ack(CommandResult::Applied)];
        }
        if !reconciled {
            // Verdicts come before anything else on a connection.
            let message = "this connection has not been reconciled yet".to_string();
            if let ControllerCommand::Offer { offer } = &envelope.command {
                return vec![WorkerMsg::OfferReply {
                    command_id: id.clone(),
                    attempt: offer.attempt.clone(),
                    decision: OfferDecision::Rejected {
                        code: QueueReasonCode::ControllerReconciling,
                        message,
                    },
                }];
            }
            return vec![ack(CommandResult::Rejected {
                code: RejectCode::InvalidState,
                message,
            })];
        }

        if let ControllerCommand::Offer { offer } = envelope.command {
            let attempt = offer.attempt.clone();
            let decision = self.on_offer(&id, *offer);
            return vec![WorkerMsg::OfferReply {
                command_id: id.clone(),
                attempt,
                decision,
            }];
        }

        let outcome = match envelope.command {
            ControllerCommand::Start { attempt, authority } => self.on_start(&id, &attempt, authority),
            ControllerCommand::Revoke { attempt, reason } => self.on_revoke(&id, &attempt, &reason),
            ControllerCommand::RenewAuthority { attempt, authority } => {
                self.on_renew(&id, &attempt, authority)
            }
            ControllerCommand::Cancel {
                attempt,
                reason,
                grace_secs,
            } => self.on_cancel(&id, &attempt, &reason, grace_secs),
            ControllerCommand::FinishEnvironment { attempt, reason } => {
                self.on_finish_environment(&id, &attempt, &reason)
            }
            ControllerCommand::UpdatePolicy { policy } => self.on_update_policy(&id, &policy),
            ControllerCommand::SetDrain { draining } => {
                self.state().controller_drain = draining;
                self.heartbeat_wake.notify_one();
                self.journal
                    .record_command(&id, &CommandResult::Applied)
                    .map(|_| CommandResult::Applied)
            }
            ControllerCommand::Offer { .. } | ControllerCommand::Reconcile { .. } => {
                Ok(CommandResult::Applied)
            }
        };
        match outcome {
            Ok(result) => {
                if result == CommandResult::Applied {
                    self.applied(&id);
                }
                vec![ack(result)]
            }
            Err(error) => match error.command_result() {
                Some(result) => {
                    let _ = self.journal.record_command(&id, &result);
                    vec![ack(result)]
                }
                None => {
                    // A fault of the journal says nothing about the
                    // command; the controller will send it again.
                    tracing::error!(%error, "journal failure while applying a command");
                    Vec::new()
                }
            },
        }
    }

    fn applied(&self, command: &CommandId) {
        self.state().last_applied_command = Some(command.clone());
    }

    /// A sample no older than one protection interval, for an admission
    /// decision.
    fn fresh_telemetry(&self) {
        let now = self.clock.now();
        let (stale, managed) = {
            let state = self.state();
            let stale = state.telemetry.as_ref().is_none_or(|sample| {
                sample.sampled_at > now
                    || now.since(sample.sampled_at) > self.tuning.protection_interval
            });
            (stale, state.managed)
        };
        if stale {
            let sample = self.sample(managed);
            self.state().telemetry = Some(sample);
        }
    }

    /// The worker's final local decision on an offer. Accepting journals a
    /// hold and executes nothing.
    fn on_offer(&self, command: &CommandId, offer: Offer) -> OfferDecision {
        let decision = self.decide_offer(command, &offer);
        match &decision {
            OfferDecision::Accepted => {
                if self.handle(&offer.attempt.attempt_id).is_none() {
                    self.cache
                        .pin(&offer.attempt.attempt_id, [offer.manifest_digest.clone()]);
                    self.insert_handle(AttemptHandle::new(offer, LocalAttemptState::OfferHeld));
                }
                self.applied(command);
                self.heartbeat_wake.notify_one();
            }
            OfferDecision::Rejected { code, message } => {
                tracing::info!(attempt = %offer.attempt.attempt_id, ?code, %message, "offer rejected");
                let stored = CommandResult::Rejected {
                    code: RejectCode::InvalidState,
                    message: serde_json::to_string(&decision).unwrap_or_default(),
                };
                let _ = self.journal.record_command(command, &stored);
            }
        }
        decision
    }

    fn decide_offer(&self, command: &CommandId, offer: &Offer) -> OfferDecision {
        let reject = |code: QueueReasonCode, message: String| OfferDecision::Rejected { code, message };
        let now = self.clock.now();
        let revision = self.state().policy.revision;
        if offer.policy_revision != revision {
            return reject(
                QueueReasonCode::OfferRejected,
                format!(
                    "the offer was made against policy revision {}, this machine is at revision {revision}",
                    offer.policy_revision
                ),
            );
        }
        if now >= offer.expires_at {
            return reject(
                QueueReasonCode::OfferRejected,
                "the offer had already expired when it arrived".to_string(),
            );
        }
        // The snapshot's platform fit, when the manifest is at hand.
        if self.cache.has_local(&offer.manifest_digest)
            && let Ok(bytes) =
                bella_core::store::get_bytes(self.cache.as_ref(), &offer.manifest_digest)
            && let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes)
        {
            let case_sensitive =
                bella_materialize::is_case_sensitive(&self.work_root).unwrap_or(true);
            if let Err(problems) =
                bella_materialize::check_platform(&manifest, self.os(), case_sensitive)
            {
                return reject(
                    QueueReasonCode::WrongPlatform,
                    describe_incompatibilities(&problems),
                );
            }
        }

        self.fresh_telemetry();
        let state = self.state();
        if let Some(reason) = state.history.admission_stopped() {
            return reject(
                QueueReasonCode::MemoryPressure,
                format!("admission is stopped by the local protection loop: {reason}"),
            );
        }
        if state.disk_full {
            return reject(
                QueueReasonCode::DiskLow,
                "the worker's journal volume is full; no new work until space is back".to_string(),
            );
        }
        // A repeated offer for a hold that stands needs no second decision.
        let held = state.attempts.contains_key(&offer.attempt.attempt_id);
        if !held {
            let grants: Vec<Grant> = state.attempts.values().map(|h| h.grant()).collect();
            let mut control = state.control;
            control.draining |= state.controller_drain;
            let input = Input {
                policy: &state.policy,
                hardware: &self.inventory.hardware,
                telemetry: state.telemetry.as_ref(),
                now,
                grants: &grants,
                control,
                config: &self.tuning.protect,
            };
            if let Err(rejection) = bella_protect::admit(&input, &offer.reservation) {
                return reject(rejection.code, rejection.message);
            }
        }
        // Journal the hold while the state is locked, so two offers cannot
        // both be admitted against the same capacity.
        // (`with_transaction` directly: `tx` would lock the state again.)
        let journaled = self.journal.with_transaction(|tx| {
            let outcome = tx.hold_offer(offer)?;
            tx.record_command(command, &CommandResult::Applied)?;
            Ok(outcome)
        });
        drop(state);
        match journaled {
            Ok(HoldOutcome::Held | HoldOutcome::Duplicate) => OfferDecision::Accepted,
            Err(error) => reject(
                QueueReasonCode::OfferRejected,
                format!("the worker's journal refused the offer: {error}"),
            ),
        }
    }

    fn on_start(
        self: &Arc<Self>,
        command: &CommandId,
        attempt: &AttemptRef,
        mut authority: ExecutionAuthority,
    ) -> Result<CommandResult, JournalError> {
        // The recipe's own limit on running disconnected also applies; the
        // journal caps both by the machine policy.
        if let Some(handle) = self.handle(&attempt.attempt_id) {
            authority.disconnected_continuation_secs = authority
                .disconnected_continuation_secs
                .min(handle.offer.spec.disconnected_continuation_secs);
        }
        let decision = self.tx(|tx| {
            let decision = tx.authorize_start(attempt, authority)?;
            let result = match decision {
                StartDecision::Fresh => CommandResult::Applied,
                StartDecision::AlreadyAuthorized => CommandResult::Duplicate,
            };
            tx.record_command(command, &result)?;
            Ok(decision)
        })?;
        match decision {
            StartDecision::AlreadyAuthorized => Ok(CommandResult::Duplicate),
            StartDecision::Fresh => {
                let handle = match self.handle(&attempt.attempt_id) {
                    Some(handle) => handle,
                    None => {
                        // The hold is in the journal but not in memory;
                        // rebuild it from the record.
                        let record = self
                            .journal
                            .attempt(&attempt.attempt_id)?
                            .ok_or_else(|| JournalError::NotFound(attempt.attempt_id.clone()))?;
                        let handle = AttemptHandle::new(record.offer, record.state);
                        self.insert_handle(handle.clone());
                        handle
                    }
                };
                handle.set_state(LocalAttemptState::StartAuthorized);
                handle.busy.store(true, Ordering::SeqCst);
                runner::launch(self, handle);
                self.heartbeat_wake.notify_one();
                Ok(CommandResult::Applied)
            }
        }
    }

    fn on_revoke(
        &self,
        command: &CommandId,
        attempt: &AttemptRef,
        reason: &str,
    ) -> Result<CommandResult, JournalError> {
        let outcome = self.tx(|tx| {
            let outcome = tx.revoke(attempt)?;
            tx.record_command(command, &CommandResult::Applied)?;
            Ok(outcome)
        })?;
        if outcome == RevokeOutcome::Revoked {
            tracing::info!(attempt = %attempt.attempt_id, %reason, "offer revoked");
        }
        self.drop_offer(&attempt.attempt_id);
        Ok(CommandResult::Applied)
    }

    fn on_renew(
        &self,
        command: &CommandId,
        attempt: &AttemptRef,
        authority: ExecutionAuthority,
    ) -> Result<CommandResult, JournalError> {
        self.tx(|tx| {
            tx.renew_authority(attempt, authority)?;
            tx.record_command(command, &CommandResult::Applied)?;
            Ok(())
        })?;
        Ok(CommandResult::Applied)
    }

    fn on_cancel(
        &self,
        command: &CommandId,
        attempt: &AttemptRef,
        reason: &str,
        grace_secs: u64,
    ) -> Result<CommandResult, JournalError> {
        let state = self.tx(|tx| {
            let record = tx
                .attempt(&attempt.attempt_id)?
                .ok_or_else(|| JournalError::NotFound(attempt.attempt_id.clone()))?;
            if record.state == LocalAttemptState::OfferHeld {
                // Nothing ran: the hold is simply dropped.
                tx.revoke(attempt)?;
            } else {
                tx.request_cancel(attempt, reason)?;
            }
            tx.record_command(command, &CommandResult::Applied)?;
            Ok(record.state)
        })?;
        if state == LocalAttemptState::OfferHeld {
            self.drop_offer(&attempt.attempt_id);
        } else if let Some(handle) = self.handle(&attempt.attempt_id) {
            let grace = Duration::from_secs(grace_secs);
            if handle.request_stop(StopReason::Cancelled, Some(grace)) {
                tracing::info!(attempt = %attempt.attempt_id, %reason, "cancel requested");
            }
        }
        Ok(CommandResult::Applied)
    }

    fn on_finish_environment(
        &self,
        command: &CommandId,
        attempt: &AttemptRef,
        reason: &str,
    ) -> Result<CommandResult, JournalError> {
        let record = self
            .journal
            .attempt(&attempt.attempt_id)?
            .ok_or_else(|| JournalError::NotFound(attempt.attempt_id.clone()))?;
        if attempt.generation < record.attempt.generation {
            return Err(JournalError::StaleGeneration {
                held: record.attempt.generation,
                offered: attempt.generation,
            });
        }
        let result = match record.state {
            LocalAttemptState::EnvironmentHeld => {
                if let Some(handle) = self.handle(&attempt.attempt_id) {
                    tracing::info!(attempt = %attempt.attempt_id, %reason, "environment finished");
                    handle.finish_environment();
                }
                CommandResult::Applied
            }
            // Already past the hold: there is nothing left to finish.
            LocalAttemptState::Collecting
            | LocalAttemptState::Cleaning
            | LocalAttemptState::Quarantined
            | LocalAttemptState::Released => CommandResult::Applied,
            state => CommandResult::Rejected {
                code: RejectCode::InvalidState,
                message: format!("the attempt is {state:?}, not an environment"),
            },
        };
        self.journal.record_command(command, &result)?;
        Ok(result)
    }

    fn on_update_policy(
        &self,
        command: &CommandId,
        policy: &MachinePolicy,
    ) -> Result<CommandResult, JournalError> {
        self.tx(|tx| {
            tx.set_policy(policy)?;
            tx.record_command(command, &CommandResult::Applied)?;
            Ok(())
        })?;
        self.state().policy = policy.clone();
        self.heartbeat_wake.notify_one();
        Ok(CommandResult::Applied)
    }

    /// Apply the controller's verdicts on the attempts this worker
    /// reported in `Hello`.
    fn reconcile(&self, verdicts: &[ReconcileVerdict]) {
        let now = self.clock.now();
        for verdict in verdicts {
            let id = &verdict.attempt.attempt_id;
            let Ok(Some(record)) = self.journal.attempt(id) else {
                continue;
            };
            if record.attempt.generation != verdict.attempt.generation {
                continue;
            }
            match (verdict.action, record.state) {
                (ReconcileAction::Continue, _) => {}
                // An unstarted offer the controller no longer stands behind.
                (
                    ReconcileAction::Forget | ReconcileAction::Abort,
                    LocalAttemptState::OfferHeld,
                ) => match self.journal.revoke(&verdict.attempt) {
                    Ok(_) => self.drop_offer(id),
                    Err(error) => {
                        tracing::warn!(attempt = %id, %error, "could not drop a held offer");
                    }
                },
                (ReconcileAction::Forget, LocalAttemptState::Released) => {
                    if self.journal.forget_attempt(id).is_ok() {
                        self.logs.remove(id);
                    }
                }
                // Released here, not wanted there: its events get this
                // connection to be delivered, then it is forgotten.
                (ReconcileAction::Abort, LocalAttemptState::Released) => {
                    let old = record.released_at.is_some_and(|at| {
                        now.since(at) >= self.tuning.released_retention
                    });
                    if old && self.journal.forget_attempt(id).is_ok() {
                        self.logs.remove(id);
                    }
                }
                // A started attempt is never just dropped: it is stopped,
                // cleaned up and released with proof.
                (ReconcileAction::Abort | ReconcileAction::Forget, _) => {
                    let reason = "the controller no longer wants this attempt (reconciliation)";
                    let _ = self.journal.request_cancel(&verdict.attempt, reason);
                    if let Some(handle) = self.handle(id) {
                        let grace = Duration::from_secs(self.state().policy.cleanup_grace_secs);
                        handle.request_stop(StopReason::Cancelled, Some(grace));
                    }
                }
            }
        }
    }
}

/// Apply a CPU limit proposed by the protection loop. `true` when the
/// limit is in force or the adapter cannot limit CPU at all, in which case
/// proposing it again would change nothing.
fn set_cpu_limit(handle: &AttemptHandle, policy: &MachinePolicy, cpu_milli: u64) -> bool {
    let Ok(mut scope) = handle.scope.try_lock() else {
        return false;
    };
    let Some(scope) = scope.as_mut() else {
        return false;
    };
    let limits = Limits {
        cpu_milli: Some(cpu_milli),
        memory_bytes: nonzero(handle.offer.reservation.memory_bytes),
        max_processes: None,
        low_priority: policy.low_priority,
    };
    match scope.set_limits(&limits) {
        Ok(()) | Err(ContainError::Unsupported(_)) => true,
        Err(error) => {
            tracing::warn!(attempt = %handle.id(), %error, "could not change a CPU limit");
            false
        }
    }
}

pub(crate) fn nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

/// The limits of a new scope for a reservation.
pub(crate) fn scope_limits(reservation: &ResourceVector, policy: &MachinePolicy) -> Limits {
    Limits {
        cpu_milli: nonzero(reservation.cpu_milli),
        memory_bytes: nonzero(reservation.memory_bytes),
        max_processes: None,
        low_priority: policy.low_priority,
    }
}

pub(crate) fn describe_incompatibilities(problems: &[bella_materialize::Incompatibility]) -> String {
    match problems {
        [] => "the snapshot cannot be materialized on this machine".to_string(),
        [only] => format!("the snapshot cannot be materialized on this machine: {only}"),
        [first, rest @ ..] => format!(
            "the snapshot cannot be materialized on this machine: {first} (and {} more)",
            rest.len()
        ),
    }
}

/// The decision recorded for an offer command that was seen before.
fn stored_offer_decision(stored: &CommandResult) -> OfferDecision {
    match stored {
        CommandResult::Applied | CommandResult::Duplicate => OfferDecision::Accepted,
        CommandResult::Rejected { message, .. } => serde_json::from_str(message).unwrap_or_else(|_| {
            OfferDecision::Rejected {
                code: QueueReasonCode::OfferRejected,
                message: message.clone(),
            }
        }),
    }
}

/// Mark every step the journal still shows as running or ready as stopped.
pub(crate) fn stop_recorded_steps(
    tx: &JournalTx<'_>,
    attempt: &AttemptRef,
    steps: &BTreeMap<String, bella_journal::StepRecord>,
) -> Result<(), JournalError> {
    for (name, step) in steps {
        if matches!(step.state, StepState::Running | StepState::Ready) {
            tx.set_step_state(&attempt.attempt_id, name, StepState::Stopped, None)?;
            tx.append_event(
                attempt,
                AttemptEvent::StepChanged {
                    step: name.clone(),
                    state: StepState::Stopped,
                    exit_code: None,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests_support {
    use bella_core::ids::Digest;
    use bella_core::job::{CommandSpec, JobSpec, OutputSpec, StepKind, StepSpec};
    use bella_core::protocol::{AttemptRef, Offer};
    use bella_core::resources::{ResourceEnvelope, ResourceVector};
    use bella_core::{AttemptId, JobId, ProjectId, SnapshotId, UnixMillis};
    use std::collections::BTreeMap;

    pub(crate) fn offer(seed: &str) -> Offer {
        let reservation = ResourceVector::new(1000, 1 << 20, 0);
        Offer {
            attempt: AttemptRef {
                attempt_id: AttemptId::derive(seed.as_bytes()),
                generation: 1,
            },
            job_id: JobId::derive(seed.as_bytes()),
            project_id: ProjectId::derive(b"project"),
            snapshot_id: SnapshotId(Digest::of_bytes(b"snapshot")),
            manifest_digest: Digest::of_bytes(b"snapshot"),
            spec: JobSpec {
                recipe: "test".into(),
                recipe_digest: Digest::of_bytes(b"recipe"),
                steps: vec![StepSpec {
                    name: "build".into(),
                    kind: StepKind::Task,
                    command: CommandSpec::Argv {
                        argv: vec!["true".into()],
                    },
                    cwd: None,
                    env: BTreeMap::new(),
                    depends_on: vec![],
                    readiness: None,
                    timeout_secs: None,
                    resources: None,
                    ports: vec![],
                }],
                envelope: ResourceEnvelope::fixed(reservation.clone()),
                compatibility: Default::default(),
                placement: Default::default(),
                priority: Default::default(),
                retry_class: Default::default(),
                max_attempts: 1,
                timeout_secs: 60,
                cleanup_timeout_secs: 30,
                cloud: Default::default(),
                source_lan_only: false,
                outputs: OutputSpec::default(),
                environment: None,
                env: BTreeMap::new(),
                disconnected_continuation_secs: 0,
                parallelism: None,
            },
            reservation,
            policy_revision: 1,
            expires_at: UnixMillis(u64::MAX / 4),
        }
    }

    pub(crate) fn b64_decode(text: &str) -> Vec<u8> {
        let value = |c: u8| -> u32 {
            match c {
                b'A'..=b'Z' => u32::from(c - b'A'),
                b'a'..=b'z' => u32::from(c - b'a') + 26,
                b'0'..=b'9' => u32::from(c - b'0') + 52,
                b'+' => 62,
                _ => 63,
            }
        };
        let mut out = Vec::new();
        for quad in text.as_bytes().chunks(4) {
            let pad = quad.iter().filter(|c| **c == b'=').count();
            let bits = quad
                .iter()
                .map(|c| if *c == b'=' { 0 } else { value(*c) })
                .fold(0u32, |acc, v| (acc << 6) | v);
            let bytes = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
            out.extend_from_slice(&bytes[..3 - pad]);
        }
        out
    }
}
