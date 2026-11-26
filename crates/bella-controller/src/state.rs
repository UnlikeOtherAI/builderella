//! State shared by the HTTP handlers, the worker hub, the dispatch loop
//! and the sweeps.

use crate::config::Tuning;
use crate::error::Failure;
use crate::hub::Hub;
use crate::ratelimit::EnrollLimiter;
use bella_core::api::EventRecord;
use bella_core::ids::{AttemptId, FleetId, JobId, MachineId, ProjectId, SessionId, SnapshotId};
use bella_core::job::Priority;
use bella_core::{Clock, UnixMillis};
use bella_ledger::{AttemptTarget, Ledger, LedgerError};
use bella_net::{ControllerUrl, Fingerprint};
use bella_store::FsStore;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{Notify, RwLock, broadcast};
use tokio_util::sync::CancellationToken;

pub(crate) type App = Arc<Shared>;

/// Lock a mutex whose protected data stays consistent across a panic: every
/// critical section in this crate is a handful of map operations.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fan-out of ledger events to event streams and long polls.
///
/// One pump task reads new events from the ledger and broadcasts them, so a
/// change costs one query however many clients are listening. A subscriber
/// that falls behind is told so by the channel and catches up from the
/// ledger, which remains the only source of truth.
pub(crate) struct EventBus {
    wake: Notify,
    sender: broadcast::Sender<Arc<EventRecord>>,
    /// Sequence of the newest event broadcast so far.
    position: AtomicU64,
}

impl EventBus {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(4096);
        Self {
            wake: Notify::new(),
            sender,
            position: AtomicU64::new(0),
        }
    }

    /// Tell the pump that the ledger may hold new events.
    pub(crate) fn touch(&self) {
        self.wake.notify_one();
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<EventRecord>> {
        self.sender.subscribe()
    }

    pub(crate) async fn wait_for_touch(&self) {
        self.wake.notified().await;
    }

    pub(crate) fn position(&self) -> u64 {
        self.position.load(Ordering::SeqCst)
    }

    pub(crate) fn set_position(&self, seq: u64) {
        self.position.store(seq, Ordering::SeqCst);
    }

    pub(crate) fn publish(&self, event: EventRecord) {
        let seq = event.seq;
        // An error only means that nobody is listening.
        let _ = self.sender.send(Arc::new(event));
        self.set_position(seq);
    }
}

/// A cancellation the controller keeps sending until the attempt reports
/// that it is stopping.
#[derive(Clone, Debug)]
pub(crate) struct PendingCommand {
    pub target: AttemptTarget,
    pub reason: String,
    /// When it was last sent, on the controller's clock.
    pub last_sent: Option<UnixMillis>,
}

/// What ordering in the scheduler needs to know about a held attempt's job.
/// None of it changes after submission, so it is cached by job.
#[derive(Clone, Debug)]
pub(crate) struct JobMeta {
    pub project: ProjectId,
    pub session: Option<SessionId>,
    pub priority: Priority,
}

pub(crate) struct Shared {
    pub ledger: Arc<Ledger>,
    pub store: Arc<FsStore>,
    pub clock: Arc<dyn Clock>,
    pub tuning: Tuning,
    pub epoch: u64,
    pub fleet_id: FleetId,
    /// Fingerprint of the fleet certificate; `None` when serving plain HTTP.
    pub fingerprint: Option<Fingerprint>,
    /// Where enrollment strings point workers.
    pub public_url: ControllerUrl,
    /// When this process took over the ledger, on the controller's clock.
    pub started_at: UnixMillis,
    /// True until start-up recovery has finished; nothing is offered before.
    pub reconciling: AtomicBool,
    pub hub: Hub,
    dispatch_wake: Notify,
    pub events: EventBus,
    pub enroll_limiter: Mutex<EnrollLimiter>,
    /// Machines that recently declined a job or let its offer expire, each
    /// with the time until which the job is not offered there again.
    pub rejections: Mutex<HashMap<JobId, Vec<(MachineId, UnixMillis)>>>,
    /// Cancellations awaiting the worker's first sign of cleanup.
    pub pending_cancels: Mutex<HashMap<AttemptId, PendingCommand>>,
    /// When `FinishEnvironment` was last sent for an attempt.
    pub finish_sent: Mutex<HashMap<AttemptId, UnixMillis>>,
    /// Machines currently flagged stale or unreachable by the liveness sweep.
    pub out_of_contact: Mutex<HashSet<MachineId>>,
    /// Attempts for which a worker's refusal already forced one reconcile.
    pub forced_reconciles: Mutex<HashSet<AttemptId>>,
    pub job_meta: Mutex<HashMap<JobId, JobMeta>>,
    /// Held shared while a snapshot or job that relies on stored objects is
    /// checked and committed, and exclusively while objects are collected,
    /// so a collection never removes what a concurrent commit just relied on.
    pub gc_gate: RwLock<()>,
    /// Snapshots whose objects were all found in the store since the last
    /// collection. Objects only disappear through a collection, so these
    /// need no second look until the next one.
    pub complete_snapshots: Mutex<HashSet<SnapshotId>>,
    pub shutdown: CancellationToken,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ledger: Arc<Ledger>,
        store: Arc<FsStore>,
        clock: Arc<dyn Clock>,
        tuning: Tuning,
        fingerprint: Option<Fingerprint>,
        public_url: ControllerUrl,
    ) -> Self {
        let limiter = EnrollLimiter::new(tuning.enroll_limits);
        Self {
            epoch: ledger.epoch(),
            fleet_id: ledger.fleet_id().clone(),
            started_at: clock.now(),
            ledger,
            store,
            clock,
            tuning,
            fingerprint,
            public_url,
            reconciling: AtomicBool::new(true),
            hub: Hub::new(),
            dispatch_wake: Notify::new(),
            events: EventBus::new(),
            enroll_limiter: Mutex::new(limiter),
            rejections: Mutex::new(HashMap::new()),
            pending_cancels: Mutex::new(HashMap::new()),
            finish_sent: Mutex::new(HashMap::new()),
            out_of_contact: Mutex::new(HashSet::new()),
            forced_reconciles: Mutex::new(HashSet::new()),
            job_meta: Mutex::new(HashMap::new()),
            gc_gate: RwLock::new(()),
            complete_snapshots: Mutex::new(HashSet::new()),
            shutdown: CancellationToken::new(),
        }
    }

    pub(crate) fn now(&self) -> UnixMillis {
        self.clock.now()
    }

    pub(crate) fn is_reconciling(&self) -> bool {
        self.reconciling.load(Ordering::SeqCst)
    }

    /// Ask for a dispatch pass: capacity, the queue or a machine changed.
    pub(crate) fn kick_dispatch(&self) {
        self.dispatch_wake.notify_one();
    }

    pub(crate) async fn dispatch_kicked(&self) {
        self.dispatch_wake.notified().await;
    }

    /// The ledger changed in a way clients may be waiting for and the
    /// scheduler may act on.
    pub(crate) fn changed(&self) {
        self.events.touch();
        self.kick_dispatch();
    }

    /// Run a ledger operation on a blocking thread. The ledger is
    /// synchronous and each call is one transaction; a task that is dropped
    /// while it waits does not interrupt the transaction.
    pub(crate) async fn db<T, F>(self: &Arc<Self>, f: F) -> Result<T, LedgerError>
    where
        T: Send + 'static,
        F: FnOnce(&Ledger) -> Result<T, LedgerError> + Send + 'static,
    {
        let ledger = self.ledger.clone();
        match tokio::task::spawn_blocking(move || f(&ledger)).await {
            Ok(result) => result,
            Err(join) => Err(LedgerError::Io(std::io::Error::other(format!(
                "a ledger task did not finish: {join}"
            )))),
        }
    }

    /// Run blocking work (object store, manifest parsing, scheduling) that
    /// needs the shared state.
    pub(crate) async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T, Failure>
    where
        T: Send + 'static,
        F: FnOnce(&Arc<Shared>) -> Result<T, Failure> + Send + 'static,
    {
        let app = self.clone();
        match tokio::task::spawn_blocking(move || f(&app)).await {
            Ok(result) => result,
            Err(join) => Err(Failure::internal(format!(
                "a blocking task did not finish: {join}"
            ))),
        }
    }

    /// Remember that `machine` should not be offered `job` again for the
    /// rejection back-off.
    pub(crate) fn note_rejection(&self, job: &JobId, machine: &MachineId) {
        let until = self.now().plus(self.tuning.rejection_backoff);
        let mut rejections = lock(&self.rejections);
        let entry = rejections.entry(job.clone()).or_default();
        entry.retain(|(id, _)| id != machine);
        entry.push((machine.clone(), until));
    }
}
