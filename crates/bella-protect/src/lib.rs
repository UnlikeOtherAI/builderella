//! Local workstation protection and admission decisions. Pure.
//!
//! This crate is the decision logic of a worker's local protection loop
//! (`docs/architecture.md` §4 and §6.10). It performs no I/O and reads no
//! clock: every observation, and the current time, is an input.
//!
//! # No controller input
//!
//! Nothing here takes controller state. [`Input`] holds only what the worker
//! knows by itself: the machine owner's policy, the hardware, local
//! telemetry, the grants in the worker's own journal, the owner's local
//! controls and the time. There is no "connected" field anywhere in the API,
//! so the same decisions are made whether the controller is reachable or
//! not. The machine's owner stays protected while the controller is away.
//!
//! # The loop
//!
//! Once per protection interval the worker daemon does:
//!
//! ```text
//! history.observe(&input);                 // record this telemetry sample
//! let todo = actions(&input, &history);    // what to do about it
//! /* apply each action */
//! history.record(&todo, input.now);        // remember what was applied
//! let budget = budget(&input);             // what to report in the heartbeat
//! ```
//!
//! and, when an offer arrives, `admit(&input, &request)` with fresh
//! telemetry. Between [`Action::StopAdmission`] and
//! [`Action::ResumeAdmission`] the daemon refuses offers regardless of what
//! `admit` says; [`History::admission_stopped`] reports that state.
//!
//! # The two memory checks
//!
//! For a request `q`, policy ceiling `C`, grants with reservation `r_i` and
//! measured use `u_i`, available memory `A`, floor `F` and guard `G`:
//!
//! * Policy capacity: `Σ max(r_i, u_i) + q ≤ C` in every dimension. Use
//!   beyond a reservation is charged at once; use below it never shrinks the
//!   promise.
//! * Immediate safety: `A − F − G − Σ max(r_i − u_i, 0) ≥ q.memory`. The
//!   memory a grant already uses has already reduced `A`, so only the part
//!   of each promise that is still unused is subtracted.
//!
//! An offered or start-authorised grant counts with `u_i = 0`. A suspended
//! grant counts exactly like a running one: suspension frees no memory and
//! no tokens, and it is never proposed as memory relief.

#![forbid(unsafe_code)]

mod admission;
mod format;
mod pressure;

pub use admission::{admit, budget};
pub use pressure::{History, actions, pressure_actions};

use bella_core::defaults::MIB;
use bella_core::job::{Priority, QueueReasonCode, RetryClass};
use bella_core::machine::{AvailabilityFlag, Hardware, LocalControl, MachinePolicy, Telemetry};
use bella_core::resources::ResourceVector;
use bella_core::{AttemptId, UnixMillis};
use std::fmt;
use std::time::Duration;

/// One reservation the worker holds, from its journal.
///
/// A grant is in the list from the moment the offer is accepted until its
/// release proof. It holds its whole reservation for that entire time,
/// started or not, suspended or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub attempt_id: AttemptId,
    /// The promise: what was reserved for the attempt.
    pub reserved: ResourceVector,
    /// Memory charged to the attempt's scope. `None` when unknown. Ignored
    /// until the attempt has started.
    pub memory_used_bytes: Option<u64>,
    /// CPU the attempt's scope is using. Informational.
    pub cpu_used_milli: Option<u64>,
    /// Scratch disk the attempt has already written, which has therefore
    /// already reduced the volume's free space. `None` when unknown, which
    /// is treated as nothing consumed (the whole disk promise is still
    /// outstanding). Ignored until the attempt has started.
    pub disk_used_bytes: Option<u64>,
    /// Processes may exist. `false` for an offered or start-authorised
    /// attempt, whose promise counts in full.
    pub started: bool,
    pub priority: Priority,
    pub retry_class: RetryClass,
    pub started_at: Option<UnixMillis>,
    /// The attempt's processes are stopped. This changes nothing in any
    /// sum: a suspended attempt still holds its memory and its tokens.
    pub suspended: bool,
}

impl Grant {
    /// Measured memory use `u_i`: zero until the attempt has started and
    /// while its use is unknown.
    pub fn memory_in_use(&self) -> u64 {
        if self.started {
            self.memory_used_bytes.unwrap_or(0)
        } else {
            0
        }
    }

    /// What the grant costs in the policy-capacity sum: `max(r_i, u_i)`.
    pub fn memory_charged(&self) -> u64 {
        self.reserved.memory_bytes.max(self.memory_in_use())
    }

    /// The part of the memory promise not yet backed by real use:
    /// `max(r_i − u_i, 0)`.
    pub fn memory_promise(&self) -> u64 {
        self.reserved
            .memory_bytes
            .saturating_sub(self.memory_in_use())
    }

    /// Memory used beyond the reservation: `max(u_i − r_i, 0)`.
    pub fn memory_excess(&self) -> u64 {
        self.memory_in_use()
            .saturating_sub(self.reserved.memory_bytes)
    }

    /// The part of the scratch-disk promise not yet written.
    pub fn disk_promise(&self) -> u64 {
        let used = if self.started {
            self.disk_used_bytes.unwrap_or(0)
        } else {
            0
        };
        self.reserved.disk_bytes.saturating_sub(used)
    }
}

/// Everything a decision depends on. All of it is local to the worker.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub policy: &'a MachinePolicy,
    pub hardware: &'a Hardware,
    /// The latest local sample, if any. Without a fresh one nothing is
    /// admitted.
    pub telemetry: Option<&'a Telemetry>,
    pub now: UnixMillis,
    /// Every reservation the journal holds.
    pub grants: &'a [Grant],
    pub control: LocalControl,
    pub config: &'a ProtectConfig,
}

/// Tuning of the protection loop. The defaults are starting values to
/// validate on the actual machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectConfig {
    /// A sample older than this is not fresh, and nothing is admitted or
    /// evicted on it. Default 5 s: five missed protection intervals.
    pub telemetry_max_age: Duration,
    /// Margin `G` kept on top of the available-memory floor in the
    /// immediate-safety check, covering what happens between a sample and
    /// the launch it admits. Default 512 MiB.
    pub memory_guard_bytes: u64,
    /// A grant is an offender once it uses more than its memory
    /// reservation plus this much. Use beyond the reservation is charged
    /// from the first byte; the tolerance only decides when admission stops
    /// and eviction becomes possible. Default 64 MiB.
    pub memory_excess_tolerance_bytes: u64,
    /// Memory pressure must stay at `Warning` or above this long before
    /// admission is stopped and CPU limits are reduced. Default 5 s.
    pub pressure_warning_grace: Duration,
    /// Memory pressure must stay `Critical` this long before an eviction.
    /// Default 3 s.
    pub pressure_critical_grace: Duration,
    /// Available memory must stay below the policy floor this long before
    /// an eviction. Default 2 s.
    pub floor_breach_grace: Duration,
    /// Pressure must stay normal, and memory above the floor, this long
    /// before admission resumes. Default 30 s.
    pub recovery_period: Duration,
    /// Minimum time between two evictions, so the first can take effect
    /// and show up in telemetry. Default 15 s: the 10 s terminate grace
    /// plus time to observe the result.
    pub eviction_cooldown: Duration,
    /// No CPU limit is proposed below this (or below the grant's own
    /// reservation when that is smaller). Default 500.
    pub min_cpu_milli_per_attempt: u64,
    /// While admission is stopped for memory pressure, each started grant
    /// is limited to this share of its CPU reservation. Default 50.
    pub pressure_cpu_percent: u8,
    /// A CPU limit is changed only when it moves by at least this much, or
    /// reaches the grant's reservation or its minimum. Default 250.
    pub cpu_limit_step_milli: u64,
    /// CPU limits follow the highest foreground use seen within this
    /// window: they drop at once and are raised again only after the
    /// foreground has been quieter for this long. Default 10 s.
    pub cpu_restore_delay: Duration,
}

impl Default for ProtectConfig {
    fn default() -> Self {
        Self {
            telemetry_max_age: Duration::from_secs(5),
            memory_guard_bytes: 512 * MIB,
            memory_excess_tolerance_bytes: 64 * MIB,
            pressure_warning_grace: Duration::from_secs(5),
            pressure_critical_grace: Duration::from_secs(3),
            floor_breach_grace: Duration::from_secs(2),
            recovery_period: Duration::from_secs(30),
            eviction_cooldown: Duration::from_secs(15),
            min_cpu_milli_per_attempt: 500,
            pressure_cpu_percent: 50,
            cpu_limit_step_milli: 250,
            cpu_restore_delay: Duration::from_secs(10),
        }
    }
}

/// What the machine can give managed work right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Everything managed work may hold in total, including what is already
    /// granted: the policy ceiling reduced by live foreground CPU use,
    /// available memory and free disk. This is the heartbeat's effective
    /// budget. Without fresh telemetry it is the static policy ceiling.
    pub ceiling: ResourceVector,
    /// `ceiling` minus what the grants are charged. Zero in every dimension
    /// without fresh telemetry. It describes capacity only; whether an
    /// offer would be accepted at all is `admitting`.
    pub free: ResourceVector,
    /// Would an offer be accepted right now? [`admit`] accepts a request
    /// exactly when this is `true` and the request fits within `free`.
    pub admitting: bool,
    /// Sorted, without duplicates.
    pub flags: Vec<AvailabilityFlag>,
    /// Why admission is closed, followed by notes on anything that reduced
    /// the budget. Each entry carries its numbers.
    pub reasons: Vec<String>,
}

/// Why an offer was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: QueueReasonCode,
    /// Specific, with the numbers that decided it.
    pub message: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Rejection {}

/// Something the worker should do to protect the host.
///
/// There is no suspend action: suspending a process frees no memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Refuse offers until [`Action::ResumeAdmission`].
    StopAdmission { reason: String },
    /// Set the CPU limit of a started attempt's scope.
    SetCpuLimit {
        attempt_id: AttemptId,
        cpu_milli: u64,
    },
    /// Terminate the attempt and clean it up. At most one per call.
    Evict {
        attempt_id: AttemptId,
        reason: String,
    },
    /// Accept offers again, subject to [`admit`].
    ResumeAdmission,
}
