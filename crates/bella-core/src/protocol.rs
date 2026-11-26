//! Worker ↔ controller control protocol.
//!
//! The worker opens one outbound WebSocket to the controller and both sides
//! exchange JSON text frames of [`WorkerMsg`] and [`ControllerMsg`]. Bulk
//! source and artifact bytes never travel on this channel.
//!
//! Rules both sides follow:
//!
//! * The controller records desired state; the worker reports observed state.
//! * Every command carries a unique id, the controller epoch and the attempt
//!   generation. A worker journals a command before acting on it and
//!   acknowledges duplicates idempotently.
//! * A worker rejects any command whose epoch is older than the newest epoch
//!   it has seen, and any command for a generation older than the one it
//!   holds for that attempt.
//! * **Start committed** is the durable boundary: once the controller has
//!   persisted a start permit it assumes the command may be running, with or
//!   without an acknowledgement.
//! * A missed heartbeat proves nothing about the processes. Reservations stay
//!   held until the worker reports a release or isolation is proven.
//! * After reconnecting, a worker receives no new offer until its previous
//!   attempts are reconciled.

use crate::ids::{
    AttemptId, BootId, CommandId, Digest, FleetId, JobId, MachineId, ProjectId, SnapshotId,
};
use crate::job::{CleanupState, JobSpec, Outcome, QueueReasonCode};
use crate::machine::{AvailabilityFlag, Inventory, LocalControl, MachinePolicy, Telemetry};
use crate::resources::ResourceVector;
use crate::time::UnixMillis;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// WebSocket path on the controller, authenticated with the machine token.
pub const WORKER_WS_PATH: &str = "/api/v1/worker/ws";

/// An attempt and its fencing token. `generation` increases with every
/// attempt of a job; results from an older generation are never canonical.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptRef {
    pub attempt_id: AttemptId,
    pub generation: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    LinuxCgroup,
    WindowsJob,
    UnixProcessGroup,
}

/// The containment scope that owns an attempt's processes, in a form the
/// worker journals before the first spawn and can re-adopt after a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeIdentity {
    pub kind: ScopeKind,
    /// Unique scope name chosen by the worker (derived from the attempt id).
    pub name: String,
    /// Adapter-private state needed to re-adopt the scope: a cgroup path, a
    /// job object name, or process-group ids with process start times. A PID
    /// alone is never permission to signal anything.
    #[serde(default)]
    pub detail: String,
}

/// Bounded permission to keep running one attempt. It is renewed
/// explicitly; a telemetry packet does not renew it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAuthority {
    /// While connected, the attempt may run until this time unless renewed.
    pub valid_until: UnixMillis,
    /// Absolute deadline for the whole attempt. Never extended.
    pub hard_deadline: UnixMillis,
    /// After `valid_until` passes without contact, the attempt may continue
    /// for this long before the worker stops it on its own.
    pub disconnected_continuation_secs: u64,
}

/// A proposal to run one attempt. Accepting it holds resources locally but
/// must not execute anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub attempt: AttemptRef,
    pub job_id: JobId,
    pub project_id: ProjectId,
    pub snapshot_id: SnapshotId,
    /// Digest of the manifest object in the object store.
    pub manifest_digest: Digest,
    pub spec: JobSpec,
    /// What the controller reserved on this machine for the attempt.
    pub reservation: ResourceVector,
    pub policy_revision: u64,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileAction {
    /// The attempt is current; keep going.
    Continue,
    /// The attempt is no longer wanted; stop it, clean up, report release.
    Abort,
    /// The controller never committed a start for this offer; drop the hold.
    Forget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileVerdict {
    pub attempt: AttemptRef,
    pub action: ReconcileAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControllerCommand {
    Offer {
        offer: Box<Offer>,
    },
    /// Run a previously accepted offer. Retransmission for the same attempt
    /// must not launch a second copy.
    Start {
        attempt: AttemptRef,
        authority: ExecutionAuthority,
    },
    /// Withdraw an offer that has no start permit.
    Revoke {
        attempt: AttemptRef,
        reason: String,
    },
    RenewAuthority {
        attempt: AttemptRef,
        authority: ExecutionAuthority,
    },
    /// Stop the attempt: polite termination, `grace_secs`, forced cleanup.
    Cancel {
        attempt: AttemptRef,
        reason: String,
        grace_secs: u64,
    },
    /// End a retained environment and run cleanup.
    FinishEnvironment {
        attempt: AttemptRef,
        reason: String,
    },
    UpdatePolicy {
        policy: MachinePolicy,
    },
    SetDrain {
        draining: bool,
    },
    /// Sent once after `Hello`, before any new offer.
    Reconcile {
        verdicts: Vec<ReconcileVerdict>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub command_id: CommandId,
    /// Incremented each time a controller process takes over the database.
    pub controller_epoch: u64,
    pub sent_at: UnixMillis,
    pub command: ControllerCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControllerMsg {
    Welcome {
        protocol_version: u32,
        controller_epoch: u64,
        fleet_id: FleetId,
        policy: MachinePolicy,
        server_time: UnixMillis,
        heartbeat_interval_ms: u64,
    },
    Command {
        envelope: CommandEnvelope,
    },
    /// Attempt events up to and including `through_seq` are durable.
    EventsAcked {
        attempt_id: AttemptId,
        through_seq: u64,
    },
    /// Log bytes of one stream up to `through_offset` are durable.
    LogsAcked {
        attempt_id: AttemptId,
        step: String,
        stream: LogStream,
        through_offset: u64,
    },
    Ping {
        nonce: u64,
    },
    /// The controller is about to close the connection, with the reason.
    Close {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandResult {
    Applied,
    /// Seen before; the earlier result stands.
    Duplicate,
    Rejected {
        code: RejectCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    StaleEpoch,
    StaleGeneration,
    UnknownAttempt,
    /// A start arrived for an offer that expired or was revoked locally.
    OfferNotHeld,
    PolicyRevisionMismatch,
    InvalidState,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OfferDecision {
    /// Resources are held locally. Nothing has been executed.
    Accepted,
    Rejected {
        code: QueueReasonCode,
        message: String,
    },
}

/// What the worker's journal says about an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalAttemptState {
    OfferHeld,
    StartAuthorized,
    Preparing,
    Running,
    /// Steps finished; services kept alive under an environment lease.
    EnvironmentHeld,
    Collecting,
    Cleaning,
    Released,
    Quarantined,
    /// Journaled before a reboot; its processes cannot be verified.
    LostAcrossBoot,
}

impl LocalAttemptState {
    /// Does the attempt still occupy resources on the machine?
    pub fn holds_resources(self) -> bool {
        !matches!(self, LocalAttemptState::Released)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub cpu_time_ms: u64,
    pub memory_current_bytes: u64,
    pub memory_peak_bytes: u64,
    pub processes: u32,
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReport {
    pub attempt: AttemptRef,
    pub job_id: JobId,
    pub state: LocalAttemptState,
    /// Highest attempt-event sequence the worker has produced.
    pub last_event_seq: u64,
    pub reserved: ResourceVector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSummary>,
}

/// Periodic worker report: observations plus what is actually active.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub machine_id: MachineId,
    pub boot_id: BootId,
    /// Increases with every WebSocket connection of this installation.
    pub connection_generation: u64,
    /// Increases with every message on this connection; a gap or regression
    /// triggers a full reconciliation rather than an assumption.
    pub seq: u64,
    pub sent_at: UnixMillis,
    pub inventory_revision: u64,
    pub policy_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_applied_command: Option<CommandId>,
    pub telemetry: Telemetry,
    pub local_control: LocalControl,
    /// What the local protection loop would currently grant to managed work
    /// in total, including what is already granted.
    pub effective_budget: ResourceVector,
    /// Would the worker accept any new offer right now?
    pub admitting: bool,
    #[serde(default)]
    pub flags: Vec<AvailabilityFlag>,
    #[serde(default)]
    pub attempts: Vec<AttemptReport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
    /// Lines written by the worker itself: step boundaries, truncation
    /// markers, cleanup progress.
    System,
}

/// A slice of one log stream. Offsets are byte offsets within that stream,
/// so retransmission after a reconnect is idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunk {
    pub attempt: AttemptRef,
    pub step: String,
    pub stream: LogStream,
    pub offset: u64,
    /// Raw bytes, base64. Build output is not guaranteed to be UTF-8.
    pub data_b64: String,
    /// The producer exceeded its allowance; this is the final chunk.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    /// A service passed its readiness probe.
    Ready,
    Succeeded,
    Failed,
    TimedOut,
    /// Not run because a dependency failed or the job was cancelled.
    Skipped,
    /// Stopped by cancellation or cleanup.
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// The service step that listens here.
    pub step: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Artifact,
    TestReport,
}

/// Evidence that an attempt's resources are free again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseProof {
    /// The containment scope was verified empty.
    pub scope_empty: bool,
    pub workspace_removed: bool,
    /// How emptiness was established, e.g. `cgroup.procs empty`,
    /// `job object ActiveProcesses=0`, `process group has no members`.
    pub method: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttemptEvent {
    /// Start authorised; materializing the snapshot.
    Preparing,
    /// The first managed process exists.
    Started {
        scope: ScopeIdentity,
    },
    StepChanged {
        step: String,
        state: StepState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// All services are ready and the non-service steps finished; the
    /// attempt is now held as an environment.
    EnvironmentReady {
        endpoints: Vec<Endpoint>,
    },
    Collecting,
    /// The object is durably in the object store.
    ArtifactPublished {
        path: String,
        kind: ArtifactKind,
        digest: Digest,
        size: u64,
    },
    /// The result. Capacity is still held until `Released`.
    Exited {
        outcome: Outcome,
        usage: UsageSummary,
    },
    Cleanup {
        state: CleanupState,
        detail: String,
    },
    Released {
        proof: ReleaseProof,
    },
    /// Cleanup could not be verified; the capacity stays held.
    Quarantined {
        reason: String,
    },
}

/// One journaled event of an attempt. `seq` starts at 1 and has no gaps;
/// the worker resends everything after the last acknowledged sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptEventMsg {
    pub attempt: AttemptRef,
    pub seq: u64,
    pub at: UnixMillis,
    pub event: AttemptEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMsg {
    /// First message on every connection.
    Hello {
        protocol_version: u32,
        machine_id: MachineId,
        boot_id: BootId,
        connection_generation: u64,
        /// Newest controller epoch this worker has seen.
        known_epoch: u64,
        inventory: Box<Inventory>,
        policy_revision: u64,
        local_control: LocalControl,
        /// Every attempt in the journal that is not yet released and
        /// acknowledged, for reconciliation.
        attempts: Vec<AttemptReport>,
    },
    Heartbeat {
        heartbeat: Box<Heartbeat>,
    },
    OfferReply {
        command_id: CommandId,
        attempt: AttemptRef,
        decision: OfferDecision,
    },
    CommandAck {
        command_id: CommandId,
        result: CommandResult,
    },
    AttemptEvent {
        event: AttemptEventMsg,
    },
    Log {
        chunk: LogChunk,
    },
    /// The inventory changed (toolchain installed, device attached).
    InventoryChanged {
        inventory: Box<Inventory>,
    },
    Pong {
        nonce: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_json() {
        let msg = ControllerMsg::Ping { nonce: 7 };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"ping","nonce":7}"#
        );
        let ack = WorkerMsg::CommandAck {
            command_id: CommandId::generate(),
            result: CommandResult::Rejected {
                code: RejectCode::StaleEpoch,
                message: "old".into(),
            },
        };
        let json = serde_json::to_string(&ack).unwrap();
        assert_eq!(serde_json::from_str::<WorkerMsg>(&json).unwrap(), ack);
    }

    #[test]
    fn a_released_attempt_holds_nothing() {
        assert!(LocalAttemptState::Quarantined.holds_resources());
        assert!(LocalAttemptState::LostAcrossBoot.holds_resources());
        assert!(!LocalAttemptState::Released.holds_resources());
    }
}
