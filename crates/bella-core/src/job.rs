//! Jobs, attempts and their lifecycle.
//!
//! A job is user intent for one immutable snapshot and one frozen
//! specification. An attempt is one execution of it. Result outcome and
//! resource-release state are separate: tests can have passed while cleanup
//! is still blocked.

use crate::ids::Digest;
use crate::platform::{Compatibility, PlacementChoice};
use crate::resources::{ResourceEnvelope, ResourceVector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// User-visible priority bands. Lower bands keep a guaranteed opportunity:
/// an endless stream of interactive work must not starve background work.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Maintenance,
    Background,
    #[default]
    Normal,
    Interactive,
    Urgent,
}

/// What may happen automatically after an attempt is lost or fails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    /// Compilation, lint, deterministic isolated tests.
    #[default]
    Pure,
    /// Touches only attempt-owned state, which is replaced before a retry.
    IsolatedSideEffects,
    /// Publishing, migrating a shared database, flashing hardware. Never
    /// repeated just because a heartbeat vanished.
    ExternalSideEffects,
}

/// Whether paid capacity may be created for a project's jobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudMode {
    /// Enrolled machines only; no paid resource creation.
    #[default]
    ExistingOnly,
    /// A person authorises cloud capacity for one job.
    Manual,
    /// Standing policy authorises expansion when its conditions hold.
    AutoOverflow,
    /// Automatic within its allowance, plus manual exceptions.
    Hybrid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Runs to completion; failure fails the job.
    #[default]
    Task,
    /// Started in the background and kept alive until the job's other steps
    /// finish (or its environment lease ends).
    Service,
    /// Runs to completion like a task; reported separately as test results.
    Test,
    /// Always runs, even when an earlier step failed or was cancelled.
    Cleanup,
}

/// A structured command. Shell syntax is never guessed or rewritten.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandSpec {
    /// Executable and arguments, run without a shell.
    Argv { argv: Vec<String> },
    /// An explicit script. `shell` defaults to `sh -c` on Unix and
    /// `powershell -Command` on Windows.
    Shell {
        script: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<Vec<String>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReadinessProbe {
    /// A TCP connect to loopback succeeds.
    Tcp { port: u16 },
    /// An HTTP GET to loopback returns a status below 500, or `status` exactly.
    Http {
        port: u16,
        #[serde(default)]
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
    },
    /// A line of the service's output contains this text.
    LogLine { contains: String },
    /// A command exits zero.
    Command { command: CommandSpec },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    pub probe: ReadinessProbe,
    pub timeout_secs: u64,
    #[serde(default = "default_probe_interval_ms")]
    pub interval_ms: u64,
}

fn default_probe_interval_ms() -> u64 {
    500
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    /// Unique within the job; `[a-z0-9_-]+`.
    pub name: String,
    #[serde(default)]
    pub kind: StepKind,
    pub command: CommandSpec,
    /// Working directory relative to the materialized source set root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Steps that must have finished (tasks) or become ready (services).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Services only: when the service counts as started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Readiness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// This step's own demand, used to compute the job's peak envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceVector>,
    /// Loopback ports a service listens on, published as environment endpoints.
    #[serde(default)]
    pub ports: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpec {
    /// Globs relative to the source set root, collected as artifacts.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Globs for test reports (JUnit XML and similar).
    #[serde(default)]
    pub test_reports: Vec<String>,
    pub max_log_bytes: u64,
    pub max_artifact_bytes: u64,
}

impl Default for OutputSpec {
    fn default() -> Self {
        Self {
            artifacts: Vec::new(),
            test_reports: Vec::new(),
            max_log_bytes: crate::defaults::MAX_LOG_BYTES,
            max_artifact_bytes: crate::defaults::MAX_ARTIFACT_BYTES,
        }
    }
}

/// Lease terms for an environment kept alive after its steps finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseSpec {
    pub idle_timeout_secs: u64,
    pub lease_secs: u64,
    pub max_lifetime_secs: u64,
}

impl Default for LeaseSpec {
    fn default() -> Self {
        Self {
            idle_timeout_secs: crate::defaults::ENV_IDLE_TIMEOUT.as_secs(),
            lease_secs: crate::defaults::ENV_LEASE.as_secs(),
            max_lifetime_secs: crate::defaults::ENV_MAX_LIFETIME.as_secs(),
        }
    }
}

/// The fully resolved, frozen specification of a job. Changing a default
/// later never mutates a job that already carries one of these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// Recipe name, for display and history.
    pub recipe: String,
    /// Digest of the resolved recipe, part of cache and result identity.
    pub recipe_digest: Digest,
    pub steps: Vec<StepSpec>,
    pub envelope: ResourceEnvelope,
    #[serde(default)]
    pub compatibility: Compatibility,
    #[serde(default)]
    pub placement: PlacementChoice,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub retry_class: RetryClass,
    /// Total attempts allowed, including the first.
    pub max_attempts: u32,
    pub timeout_secs: u64,
    pub cleanup_timeout_secs: u64,
    #[serde(default)]
    pub cloud: CloudMode,
    /// Source may not leave the LAN: never placed on cloud or remote workers.
    #[serde(default)]
    pub source_lan_only: bool,
    #[serde(default)]
    pub outputs: OutputSpec,
    /// `Some` keeps services alive as an environment after the other steps
    /// finish, until the lease ends or someone finishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<LeaseSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// How long the attempt may continue without controller contact. Capped
    /// by the machine's own `max_disconnected_secs`.
    #[serde(default)]
    pub disconnected_continuation_secs: u64,
    /// Suggested build parallelism, exported to steps as `BELLA_JOBS`.
    /// Separate from the CPU quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
}

/// Where a job is in its life. Reachability and uncertainty are tracked
/// separately on the attempt; a missing worker never moves a job straight
/// to `Finished`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Capturing,
    Queued,
    Offered,
    Preparing,
    Executing,
    Collecting,
    Cleaning,
    Finished,
    Quarantined,
}

impl JobPhase {
    pub fn can_transition_to(self, next: JobPhase) -> bool {
        use JobPhase::*;
        matches!(
            (self, next),
            (Capturing, Queued)
                | (Capturing, Finished) // capture failed or cancelled before upload
                | (Queued, Offered)
                | (Queued, Finished) // cancelled or rejected while queued
                | (Offered, Queued) // rejected or revoked before a start permit
                | (Offered, Preparing)
                | (Offered, Cleaning) // cancelled after preparation allocated something
                | (Preparing, Executing)
                | (Preparing, Cleaning)
                | (Executing, Collecting)
                | (Executing, Cleaning)
                | (Collecting, Cleaning)
                | (Cleaning, Finished)
                | (Cleaning, Queued) // retry after a verified release
                | (Cleaning, Quarantined)
                | (Quarantined, Cleaning)
                | (Quarantined, Finished) // operator resolved it
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, JobPhase::Finished)
    }

    /// Phases in which an attempt owns a resource reservation.
    pub fn holds_resources(self) -> bool {
        use JobPhase::*;
        matches!(
            self,
            Offered | Preparing | Executing | Collecting | Cleaning | Quarantined
        )
    }
}

/// The result of an attempt. Infrastructure failures are never reported as
/// ordinary test failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    Cancelled,
    TimedOut {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
    },
    /// Hit a resource limit; carries peak and limit data.
    ResourceExhausted {
        detail: String,
    },
    /// Worker lost, machine slept, cloud interruption, snapshot unavailable.
    InfraInterrupted {
        detail: String,
    },
    /// Could never run as specified.
    Rejected {
        detail: String,
    },
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Succeeded)
    }

    /// May a fresh attempt be created automatically for this outcome?
    pub fn is_retryable(&self, class: RetryClass) -> bool {
        match self {
            Outcome::InfraInterrupted { .. } => class != RetryClass::ExternalSideEffects,
            _ => false,
        }
    }
}

/// Progress of cancellation and cleanup. Cancel is a request followed by
/// observed cleanup, not a declaration that cleanup already happened.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    #[default]
    NotStarted,
    Requested,
    WorkerAcknowledged,
    ProcessesStopped,
    CleanupComplete,
    /// Reservation released after an accepted cleanup proof.
    Released,
    /// Cleanup could not be verified; the capacity stays held.
    Quarantined,
}

/// Machine-readable reason a job is waiting, with a readable explanation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReasonCode {
    /// About to be offered; nothing is blocking.
    Schedulable,
    /// Compatible machines exist but are fully reserved.
    WaitingForCapacity,
    /// No enrolled machine can ever run this as specified.
    NoCompatibleMachine,
    WrongPlatform,
    MissingCapability,
    /// The request exceeds what this machine could ever grant.
    MachineTooSmall,
    ForegroundProtection,
    MemoryPressure,
    DiskLow,
    StaleTelemetry,
    MachineUnreachable,
    MachinePaused,
    MachineDraining,
    OnBattery,
    BuildsDisabled,
    ProjectNotAllowed,
    MachineConcurrencyLimit,
    ProjectConcurrencyLimit,
    FairShare,
    /// Capacity is being held back so an older, larger job can start.
    ReservedForOlderJob,
    PreviousAttemptUnresolved,
    CleanupPending,
    Quarantined,
    SnapshotUnavailable,
    SourceLanOnly,
    CloudManualOnly,
    ControllerReconciling,
    OfferRejected,
}

/// Why one machine cannot take a job right now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineBlocker {
    pub machine: String,
    pub code: QueueReasonCode,
    pub message: String,
    /// `true` when waiting cannot fix it (wrong OS, missing SDK, too small).
    pub permanent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueReason {
    pub code: QueueReasonCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<MachineBlocker>,
}

impl QueueReason {
    pub fn new(code: QueueReasonCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            blockers: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_orders_by_urgency() {
        assert!(Priority::Urgent > Priority::Interactive);
        assert!(Priority::Interactive > Priority::Normal);
        assert!(Priority::Background > Priority::Maintenance);
    }

    #[test]
    fn a_job_cannot_skip_cleanup() {
        assert!(!JobPhase::Executing.can_transition_to(JobPhase::Finished));
        assert!(!JobPhase::Preparing.can_transition_to(JobPhase::Queued));
        assert!(JobPhase::Offered.can_transition_to(JobPhase::Queued));
        assert!(JobPhase::Cleaning.can_transition_to(JobPhase::Quarantined));
        assert!(!JobPhase::Finished.can_transition_to(JobPhase::Queued));
    }

    #[test]
    fn external_side_effects_are_not_retried() {
        let lost = Outcome::InfraInterrupted {
            detail: "worker lost".into(),
        };
        assert!(lost.is_retryable(RetryClass::Pure));
        assert!(!lost.is_retryable(RetryClass::ExternalSideEffects));
        assert!(
            !Outcome::Failed {
                step: None,
                exit_code: Some(1)
            }
            .is_retryable(RetryClass::Pure)
        );
    }
}
