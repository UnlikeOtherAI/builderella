//! Records returned by ledger operations that have no equivalent among the
//! shared `bella_core` types.

use bella_core::UnixMillis;
use bella_core::api::{EnvironmentView, Role, SessionInfo};
use bella_core::ids::{
    AttemptId, BootId, Digest, FleetId, JobId, MachineId, PrincipalId, ProjectId, SessionId,
    SnapshotId,
};
use bella_core::job::{CleanupState, JobPhase, JobSpec, Outcome, Priority};
use bella_core::lease::LeaseEnd;
use bella_core::machine::{Inventory, MachinePolicy, MachineProfile};
use bella_core::platform::Platform;
use bella_core::protocol::{AttemptRef, ExecutionAuthority, Offer, UsageSummary};
use bella_core::resources::ResourceVector;
use bella_core::snapshot::SnapshotMeta;
use std::collections::BTreeSet;

/// A human, automation or service identity. The token itself is never
/// stored or returned; only its hash is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub id: PrincipalId,
    pub name: String,
    pub role: Role,
    pub created_at: UnixMillis,
    pub revoked_at: Option<UnixMillis>,
}

/// Who a bearer token belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authenticated {
    Principal(Principal),
    Machine { id: MachineId, name: String },
}

/// Input of [`crate::Ledger::enroll_machine`].
#[derive(Clone, Copy, Debug)]
pub struct NewMachine<'a> {
    /// Display label. Made unique among live machines by adding a suffix.
    pub name: &'a str,
    pub token_hash: &'a str,
    pub boot_id: &'a BootId,
    pub inventory: &'a Inventory,
    pub profile: MachineProfile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrolledMachine {
    pub fleet_id: FleetId,
    pub machine_id: MachineId,
    /// The name actually assigned.
    pub name: String,
    pub policy: MachinePolicy,
}

/// A machine's API view plus the facts scheduling needs beyond it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineStatus {
    pub view: bella_core::api::MachineView,
    /// The worker said, in its last heartbeat, that it would accept an offer.
    pub admitting: bool,
    /// The machine has reconciled since it connected, in the current
    /// epoch. Only such a machine can be offered work.
    pub reconciled: bool,
    pub boot_id: Option<BootId>,
    pub connection_generation: u64,
}

/// What the controller must do after recording a heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartbeatOutcome {
    /// The boot id changed, the heartbeat sequence went backwards, or the
    /// machine has not reconciled in this epoch: run a full reconcile
    /// before trusting the machine's state or offering it work.
    pub needs_reconcile: bool,
    /// The worker runs with a policy revision other than the stored one.
    pub policy_stale: bool,
    /// The stored policy revision.
    pub policy_revision: u64,
}

/// Input of [`crate::Ledger::register_snapshot`].
#[derive(Clone, Copy, Debug)]
pub struct NewSnapshot<'a> {
    pub id: &'a SnapshotId,
    /// Project name; created with the default policy when unknown.
    pub project: &'a str,
    /// Digest of the manifest object in the object store.
    pub manifest_digest: &'a Digest,
    pub file_count: u64,
    pub total_bytes: u64,
    pub meta: &'a SnapshotMeta,
}

/// A committed reservation and everything needed to send the offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferRecord {
    pub attempt: AttemptRef,
    pub job_id: JobId,
    pub machine_id: MachineId,
    pub project_id: ProjectId,
    pub snapshot_id: SnapshotId,
    pub manifest_digest: Digest,
    pub spec: JobSpec,
    pub reservation: ResourceVector,
    pub policy_revision: u64,
    pub expires_at: UnixMillis,
    /// The controller epoch that created the offer.
    pub epoch: u64,
}

impl OfferRecord {
    /// The wire form of this offer.
    pub fn to_offer(&self) -> Offer {
        Offer {
            attempt: self.attempt.clone(),
            job_id: self.job_id.clone(),
            project_id: self.project_id.clone(),
            snapshot_id: self.snapshot_id.clone(),
            manifest_digest: self.manifest_digest.clone(),
            spec: self.spec.clone(),
            reservation: self.reservation.clone(),
            policy_revision: self.policy_revision,
            expires_at: self.expires_at,
        }
    }
}

/// A durable start permit. Once this exists the attempt is treated as
/// possibly running, whatever messages arrive or fail to arrive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartPermit {
    pub attempt: AttemptRef,
    pub job_id: JobId,
    pub machine_id: MachineId,
    /// The epoch that committed the start.
    pub epoch: u64,
    pub start_committed_at: UnixMillis,
    pub authority: ExecutionAuthority,
    /// The permit already existed; this call changed nothing.
    pub already_committed: bool,
}

/// An attempt and where to send commands about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptTarget {
    pub attempt: AttemptRef,
    pub job_id: JobId,
    pub machine_id: MachineId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferReplyOutcome {
    /// The acceptance is recorded (or already was).
    Accepted,
    /// The rejection released the reservation; the job is queued again.
    Requeued,
    /// The reservation had already been released; nothing changed.
    AlreadyReleased,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// The reservation was released; the job is queued again.
    Revoked,
    /// The reservation had already been released; nothing changed.
    AlreadyReleased,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDisposition {
    Applied,
    /// The sequence number was seen before; nothing changed.
    Duplicate,
    /// The attempt's reservation was already released; the event was
    /// dropped and nothing changed.
    AttemptReleased,
}

/// What became of the job when its attempt's reservation was released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobDisposition {
    Finished,
    /// Queued again, either because the offer never started or because the
    /// outcome is retryable and attempts remain.
    Requeued,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The reservation was released by this call.
    Released(JobDisposition),
    /// A previous call released it; nothing changed.
    AlreadyReleased,
    /// The proof does not show an empty scope and a removed workspace. The
    /// attempt is quarantined and its reservation stays held.
    Quarantined,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The job is finished as cancelled. `revoked` names an offer that was
    /// withdrawn and that the worker should be told to drop.
    Cancelled { revoked: Option<AttemptTarget> },
    /// The job has a start permit. Cancellation is recorded as requested;
    /// tell the worker to cancel `target` and wait for its release.
    Requested { target: AttemptTarget },
    /// The job had already finished; nothing changed.
    AlreadyFinished,
}

/// Everything the ledger holds about one attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptRecord {
    pub id: AttemptId,
    pub job_id: JobId,
    pub generation: u32,
    pub machine_id: MachineId,
    pub phase: JobPhase,
    pub outcome: Option<Outcome>,
    pub cleanup: CleanupState,
    /// The worker is out of contact; the attempt may still be running.
    pub uncertain: bool,
    pub reserved: ResourceVector,
    /// The reservation still counts against the machine.
    pub held: bool,
    /// Why the reservation was released: `proof`, `rejected`, `revoked`,
    /// `expired`, `cancelled`, `forgotten`, `reboot`, `never_delivered`
    /// or `operator`.
    pub release_reason: Option<String>,
    pub policy_revision: u64,
    pub offer_epoch: u64,
    pub offered_at: UnixMillis,
    pub offer_expires_at: UnixMillis,
    pub accepted_at: Option<UnixMillis>,
    pub start_committed_at: Option<UnixMillis>,
    pub start_epoch: Option<u64>,
    /// The worker acknowledged the start or reported an event.
    pub start_delivered_at: Option<UnixMillis>,
    pub authority: Option<ExecutionAuthority>,
    pub usage: Option<UsageSummary>,
    pub started_at: Option<UnixMillis>,
    pub exited_at: Option<UnixMillis>,
    pub released_at: Option<UnixMillis>,
    /// Highest attempt-event sequence applied.
    pub last_event_seq: u64,
    pub log_bytes: u64,
    pub log_truncated: bool,
}

impl AttemptRecord {
    pub fn reference(&self) -> AttemptRef {
        AttemptRef {
            attempt_id: self.id.clone(),
            generation: self.generation,
        }
    }

    pub fn target(&self) -> AttemptTarget {
        AttemptTarget {
            attempt: self.reference(),
            job_id: self.job_id.clone(),
            machine_id: self.machine_id.clone(),
        }
    }

    pub fn start_committed(&self) -> bool {
        self.start_committed_at.is_some()
    }

    /// The attempt is an offer that can still be rejected, revoked, expired
    /// or cancelled away: it holds its reservation, has no start permit and
    /// is not quarantined.
    pub fn may_be_withdrawn(&self) -> bool {
        self.held && self.start_committed_at.is_none() && self.phase == JobPhase::Offered
    }
}

/// A queued job with what the scheduler needs to place it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedJob {
    pub job_id: JobId,
    /// Position in submission order across all jobs ever submitted.
    pub submit_seq: u64,
    pub project_id: ProjectId,
    pub project_name: String,
    pub principal_id: PrincipalId,
    pub session: Option<SessionInfo>,
    pub session_id: Option<SessionId>,
    pub snapshot_id: SnapshotId,
    pub snapshot_meta: SnapshotMeta,
    pub origin_platform: Platform,
    pub priority: Priority,
    pub spec: JobSpec,
    pub created_at: UnixMillis,
    /// When the job last entered the queue (submission or retry).
    pub queued_at: UnixMillis,
    /// Attempts that received a start permit so far.
    pub started_attempts: u32,
    /// Machine of the most recent attempt, if any.
    pub last_machine: Option<MachineId>,
}

/// Number of jobs in each phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobCounts {
    pub queued: u32,
    pub offered: u32,
    pub preparing: u32,
    pub executing: u32,
    pub collecting: u32,
    pub cleaning: u32,
    pub quarantined: u32,
    pub finished: u32,
}

impl JobCounts {
    /// Jobs that are neither queued nor finished.
    pub fn active(&self) -> u32 {
        self.offered
            + self.preparing
            + self.executing
            + self.collecting
            + self.cleaning
            + self.quarantined
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogAppendStatus {
    Appended,
    /// The bytes were already stored (or overlap stored bytes); ignored.
    Duplicate,
    /// The attempt's log allowance is used up; some or all bytes were
    /// dropped and the truncation is recorded.
    Truncated,
    /// The attempt is released and its log is closed; the bytes were dropped.
    Sealed,
}

/// Result of appending a log chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogAppend {
    /// Bytes of this stream up to this offset need not be sent again.
    pub through_offset: u64,
    pub status: LogAppendStatus,
}

/// Roots of everything the object store must keep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveDigests {
    /// Manifest objects of snapshots used by jobs that are not finished.
    /// Every object such a manifest lists is live too; the ledger does not
    /// store manifests, so the caller expands them through the store.
    pub snapshot_manifests: BTreeSet<Digest>,
    /// Retained artifacts and test reports.
    pub artifacts: BTreeSet<Digest>,
}

/// An environment whose lease has ended and that should be finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredEnvironment {
    pub environment: EnvironmentView,
    pub reason: LeaseEnd,
}

/// Result of asking an environment to finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentFinish {
    pub environment: EnvironmentView,
    /// The attempt to send `FinishEnvironment` to, while it is still held.
    pub target: Option<AttemptTarget>,
}

/// A running attempt whose execution authority is about to lapse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityDue {
    pub target: AttemptTarget,
    pub authority: ExecutionAuthority,
}
