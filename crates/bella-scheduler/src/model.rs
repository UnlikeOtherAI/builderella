//! Scheduler inputs and outputs.
//!
//! The controller builds a [`FleetState`] from its database on every tick.
//! Nothing here is read from a clock, a file or the network.

use bella_core::api::ProjectPolicy;
use bella_core::defaults;
use bella_core::job::{JobSpec, Priority, QueueReason};
use bella_core::machine::{AvailabilityFlag, MachinePolicy};
use bella_core::platform::{Capability, Platform};
use bella_core::resources::{EnforcementReport, ResourceVector};
use bella_core::{JobId, MachineId, ProjectId, SessionId, UnixMillis};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One enrolled machine as the controller currently knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineState {
    pub id: MachineId,
    /// Display name. Also accepted by `PlacementChoice::Machine` when no
    /// other machine in the state carries the same name.
    pub name: String,
    pub platform: Platform,
    pub os_version: String,
    pub capabilities: Vec<Capability>,
    pub policy: MachinePolicy,
    /// Static upper bound on managed work, from policy and hardware.
    pub managed_ceiling: ResourceVector,
    /// What the worker's protection loop grants in total right now: the
    /// `ceiling` of its `bella_protect::Budget`, as sent in the heartbeat.
    /// It names every token the worker's policy declares; a token missing
    /// here is one the worker does not grant (see
    /// [`effective_capacity`](crate::effective_capacity)). `None` until the
    /// worker has reported one; such a machine gets no offers.
    pub effective_budget: Option<ResourceVector>,
    /// Sum of every reservation held on this machine, including offered,
    /// uncertain and quarantined ones.
    pub reserved: ResourceVector,
    /// Number of attempts holding those reservations.
    pub held_attempts: u32,
    pub connected: bool,
    /// The worker's own answer to "would you accept an offer right now".
    pub admitting: bool,
    pub flags: Vec<AvailabilityFlag>,
    /// Age of the newest telemetry sample; `None` when there is none.
    pub telemetry_age_ms: Option<u64>,
    /// `false` until the worker's previous attempts are reconciled with the
    /// ledger. No offers before that.
    pub reconciled: bool,
    /// The machine is on the controller's local network.
    pub on_lan: bool,
    /// What the native executor can enforce. Carried for the caller; no
    /// scheduling decision depends on it in v1.
    pub enforcement: EnforcementReport,
    /// The work volume distinguishes names that differ only by case.
    pub case_sensitive_fs: bool,
    /// The worker can create symbolic links on its work volume.
    pub supports_symlinks: bool,
}

/// One job waiting in the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedJob {
    pub id: JobId,
    pub project: ProjectId,
    pub session: Option<SessionId>,
    pub spec: Arc<JobSpec>,
    /// Platform of the machine that submitted the job. A recipe with no
    /// build hosts is pinned to it.
    pub origin_platform: Platform,
    pub queued_at: UnixMillis,
    /// Attempts already made. Carried for the caller; a retried job keeps
    /// its place by `queued_at` and is ordered like any other.
    pub attempts_made: u32,
    /// `Some(why)` when the snapshot cannot be materialized on Windows.
    pub snapshot_windows_incompatible: Option<String>,
    /// The snapshot has paths that collide on a case-insensitive volume.
    pub snapshot_case_sensitive_only: bool,
    pub snapshot_has_symlinks: bool,
    /// An earlier attempt is still live or its release is unproven.
    pub has_unresolved_attempt: bool,
    /// Machines that declined this job, each with the time until which the
    /// job should not be offered to it again.
    pub recently_rejected_by: Vec<(MachineId, UnixMillis)>,
}

/// One project and what it currently holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectState {
    pub id: ProjectId,
    pub name: String,
    pub policy: ProjectPolicy,
    /// Attempts of this project that hold a reservation.
    pub held_attempts: u32,
    /// Sum of those reservations. The fair-share input.
    pub reserved: ResourceVector,
    /// Live environments, counted against `policy.max_environments`.
    pub environments: u32,
}

/// One attempt that currently holds a reservation.
///
/// These records feed ordering only: service across priority bands and
/// round-robin across sessions. Capacity is always checked against
/// [`MachineState::reserved`], and project limits against [`ProjectState`].
/// Leaving the list empty keeps every guarantee about capacity and limits,
/// but then band and session fairness only hold inside a single call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldAttempt {
    pub project: ProjectId,
    pub session: Option<SessionId>,
    /// Priority of the job the attempt belongs to.
    pub priority: Priority,
    pub reserved: ResourceVector,
}

/// Relative service each priority band receives while several bands have
/// work waiting. A weight of zero is treated as one, so no band can be
/// configured to starve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BandWeights {
    pub maintenance: u32,
    pub background: u32,
    pub normal: u32,
    pub interactive: u32,
    pub urgent: u32,
}

impl Default for BandWeights {
    fn default() -> Self {
        Self {
            maintenance: 1,
            background: 2,
            normal: 4,
            interactive: 8,
            urgent: 16,
        }
    }
}

impl BandWeights {
    /// The weight of one band, never less than one.
    pub fn weight(&self, band: Priority) -> u64 {
        let raw = match band {
            Priority::Maintenance => self.maintenance,
            Priority::Background => self.background,
            Priority::Normal => self.normal,
            Priority::Interactive => self.interactive,
            Priority::Urgent => self.urgent,
        };
        u64::from(raw.max(1))
    }
}

/// Tunable scheduling parameters. Every value is a starting point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    pub band_weights: BandWeights,
    /// A job that has waited this long is treated as one band higher.
    pub aging_after_ms: u64,
    /// The highest band aging can reach. `Urgent` is never reached by
    /// aging, whatever this says.
    pub aging_ceiling: Priority,
    /// A job that has waited this long is considered before jobs that have
    /// not, and reserves a machine when it still cannot start.
    pub starvation_threshold_ms: u64,
    /// Telemetry older than this makes a machine ineligible for offers.
    pub stale_telemetry_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            band_weights: BandWeights::default(),
            aging_after_ms: 5 * 60 * 1000,
            aging_ceiling: Priority::Interactive,
            starvation_threshold_ms: 10 * 60 * 1000,
            stale_telemetry_ms: u64::try_from(defaults::TELEMETRY_STALE_AFTER.as_millis())
                .unwrap_or(u64::MAX),
        }
    }
}

/// Everything one scheduling decision depends on.
///
/// Machine, job and project ids are expected to be unique. A repeated id is
/// tolerated (the first entry in id order is used, and a job is never
/// offered twice) but the choice between the duplicates is unspecified.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FleetState {
    pub now: UnixMillis,
    /// The controller is reconciling after a start: nothing is offered.
    pub reconciling: bool,
    pub machines: Vec<MachineState>,
    pub queue: Vec<QueuedJob>,
    pub projects: Vec<ProjectState>,
    /// Attempts currently holding reservations; see [`HeldAttempt`].
    pub held: Vec<HeldAttempt>,
    pub config: SchedulerConfig,
}

/// One decision: offer this job to this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedOffer {
    pub job_id: JobId,
    pub machine_id: MachineId,
    /// What to reserve: the job's peak envelope.
    pub reservation: ResourceVector,
    /// The machine's total capacity the reservation was checked against,
    /// `min(managed ceiling, effective budget)`. The ledger re-checks
    /// `held + reservation <= capacity` inside its transaction.
    pub capacity: ResourceVector,
    /// The machine's concurrent-attempt limit that was checked, if any.
    pub max_concurrent: Option<u32>,
}

/// The result of one scheduling call. Every queued job appears exactly
/// once: in `offers`, or in `reasons`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// In the order the decisions were made. No two offers together exceed
    /// a machine's capacity or any concurrency limit.
    pub offers: Vec<PlannedOffer>,
    pub reasons: BTreeMap<JobId, QueueReason>,
}
