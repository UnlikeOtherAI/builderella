//! Pure scheduling: eligibility, fairness, backfill, placement and explanations.
//!
//! [`schedule`] takes a [`FleetState`] and returns which queued jobs to
//! offer to which machines, plus a reason for every job it does not offer.
//! It reads no clock, performs no I/O and uses no randomness: `now` and all
//! state are inputs, and equal inputs give equal outputs regardless of the
//! order of the input vectors. The controller rebuilds the state from its
//! ledger on every tick and turns each [`PlannedOffer`] into a
//! transactional offer; the ledger repeats the capacity check, so a stale
//! state can cost a rejected offer but never an over-commitment.
//!
//! # Eligibility
//!
//! For each job and machine there is at most one [`MachineBlocker`], the
//! most fundamental one. Blockers that waiting cannot remove are
//! `permanent` and are reported before temporary ones:
//!
//! * platform: the recipe's build hosts (or the submitting platform when it
//!   declares none), its test runtime, and a `Require`, `Machine` or `Pool`
//!   placement choice. All of these use `WrongPlatform`;
//! * the snapshot against the machine's filesystem (`WrongPlatform`);
//! * isolation and capability requirements (`MissingCapability`; a
//!   capability that is installed but occupied, unhealthy or unverified is
//!   temporary);
//! * machine and project policy (`BuildsDisabled`, `ProjectNotAllowed`,
//!   `SourceLanOnly`);
//! * a peak larger than the machine's managed ceiling (`MachineTooSmall`).
//!
//! Temporary blockers, in the order they are checked: connection, telemetry
//! freshness, reconciliation, availability flags, the worker's own
//! admission answer, a recent rejection of this job, the machine's
//! concurrency limit, capacity, and a machine kept free for another job.
//! `Overheated` is reported as `ForegroundProtection` and `Upgrading` as
//! `MachineDraining`, the closest existing codes.
//!
//! Capacity is `reserved + peak <= min(managed ceiling, effective budget)`
//! in every dimension; see [`effective_capacity`]. The peak is the job's
//! whole simultaneous envelope, so a job is never placed where only part of
//! it fits.
//!
//! # Order of service
//!
//! Jobs are considered one at a time. After each planned offer the working
//! copy of the state is charged (machine, project, band, session) before
//! the next job is chosen, so two offers in one call never double-book and
//! the order adapts as capacity is handed out.
//!
//! 1. **Bands.** Each job is served in a band: its priority, lowered from
//!    `Urgent` to `Interactive` when its project may not use `Urgent`, and
//!    raised by exactly one band once it has waited `aging_after_ms` (never
//!    above `aging_ceiling`, never to `Urgent`). The next job comes from
//!    the band with work waiting whose held resources, as a dominant share
//!    of the fleet divided by the band's weight (16:8:4:2:1 by default),
//!    are lowest; ties go to the higher band. A band that holds nothing is
//!    therefore served before a band that already holds something, however
//!    many jobs the busier band has queued, and under sustained contention
//!    each band's share converges on its weight. Held resources per band
//!    come from [`FleetState::held`].
//! 2. **Projects.** Within the band, the project with the lowest dominant
//!    share of reserved resources divided by its `weight` goes first. Only
//!    projects carry weight: more sessions or more queued jobs do not buy a
//!    larger share.
//! 3. **Sessions.** Within the project, the session holding the fewest
//!    attempts goes first, then the one whose next job is oldest.
//! 4. **Age.** Within a session, oldest first.
//!
//! # Backfill and starvation
//!
//! A job that does not fit is skipped, and later jobs that do fit are
//! offered: small work passes large work. To bound how long that can go
//! on, jobs that have waited `starvation_threshold_ms` are considered
//! before all others (in the same fair order among themselves), and one
//! that still cannot start because of capacity alone claims a machine: the
//! machine able to take it under the current budget where the smallest
//! part of its request is still missing. For the rest of the call no other
//! job is placed there (`ReservedForOlderJob`). The claim is recomputed
//! from scratch each call, and since nothing new lands on the claimed
//! machine, its reservations only drain until the job fits.
//!
//! # Placement
//!
//! Among machines that can take the job now: one satisfying a `Prefer`
//! choice; then one that is not a protected workstation; then the one the
//! fewest other waiting jobs can run on exclusively (a portable job stays
//! off the only macOS machine while an Xcode job waits); then the one left
//! with the most headroom; then the lowest machine id.

#![forbid(unsafe_code)]

mod eligibility;
mod engine;
mod model;
mod share;
mod units;

pub use eligibility::effective_capacity;
pub use model::{
    BandWeights, FleetState, HeldAttempt, MachineState, PlannedOffer, ProjectState, QueuedJob,
    Schedule, SchedulerConfig,
};

use bella_core::job::{MachineBlocker, QueueReason, QueueReasonCode};
use bella_core::{JobId, MachineId};

/// Decide which queued jobs to offer to which machines.
///
/// Guarantees:
///
/// * every queued job is in exactly one of `offers` and `reasons`;
/// * on every machine, `reserved` plus all reservations offered there fits
///   `min(managed ceiling, effective budget)`, and held attempts plus
///   offers stay within the machine's `max_concurrent_attempts`;
/// * no project exceeds its `max_concurrent_attempts` or
///   `max_environments`;
/// * no offer goes to a machine with a blocker for that job;
/// * the result depends only on the content of `state`, not on the order
///   of its vectors.
pub fn schedule(state: &FleetState) -> Schedule {
    engine::schedule(state)
}

/// Why `job` is still queued, or `Schedulable` when the next [`schedule`]
/// call would offer it. `None` when the job is not in the queue.
///
/// This runs a full scheduling pass, so the answer accounts for the jobs
/// ahead of this one.
pub fn explain(state: &FleetState, job: &JobId) -> Option<QueueReason> {
    if !state.queue.iter().any(|queued| queued.id == *job) {
        return None;
    }
    let plan = schedule(state);
    if let Some(reason) = plan.reasons.get(job) {
        return Some(reason.clone());
    }
    let offer = plan.offers.iter().find(|offer| offer.job_id == *job)?;
    let machine = state
        .machines
        .iter()
        .find(|machine| machine.id == offer.machine_id)
        .map_or_else(|| offer.machine_id.to_string(), eligibility::label);
    Some(QueueReason::new(
        QueueReasonCode::Schedulable,
        format!("nothing is blocking this job; it is about to be offered to {machine}"),
    ))
}

/// Every machine in id order, with `Ok(())` when it could take `job` right
/// now and otherwise the single most fundamental reason it cannot.
///
/// `job` does not have to be in the queue, which is what planning a
/// submission needs. Only the machine side is evaluated: other queued jobs,
/// machines kept free for a starved job and the job-level checks of
/// [`job_blocker`] are not considered.
pub fn evaluate(state: &FleetState, job: &QueuedJob) -> Vec<(MachineId, Result<(), MachineBlocker>)> {
    eligibility::evaluate(state, job)
}

/// The reason `job` cannot be offered to any machine at the moment,
/// whatever the machines look like: the controller is reconciling, an
/// earlier attempt is unresolved, its project is unknown, or its project
/// is at its attempt or environment limit.
pub fn job_blocker(state: &FleetState, job: &QueuedJob) -> Option<QueueReason> {
    if state.reconciling {
        return Some(QueueReason::new(
            QueueReasonCode::ControllerReconciling,
            engine::RECONCILING,
        ));
    }
    let project = state
        .projects
        .iter()
        .find(|project| project.id == job.project)
        .map(|project| (project, project.held_attempts, project.environments));
    engine::job_gate(job, project)
}
