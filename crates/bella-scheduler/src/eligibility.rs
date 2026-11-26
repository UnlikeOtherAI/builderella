//! Can this machine take this job, and if not, exactly why.
//!
//! Checks come in two groups. *Standing* blockers depend only on the job,
//! the machine's description and its flags; they cannot change while one
//! schedule is computed. *Load* blockers depend on what is reserved, which
//! changes as offers are planned.
//!
//! A blocker is `permanent` when waiting cannot remove it: the wrong
//! platform, a missing toolchain, a policy that excludes the job, a request
//! larger than the machine can ever grant. Permanent blockers are reported
//! in preference to temporary ones on the same machine, so "no machine can
//! ever run this" is never hidden behind "that machine is offline".

use crate::model::{FleetState, MachineState, ProjectState, QueuedJob};
use crate::share::Totals;
use crate::units;
use bella_core::job::{MachineBlocker, QueueReasonCode as Code};
use bella_core::machine::AvailabilityFlag as Flag;
use bella_core::platform::{
    CapabilityMismatch, CapabilityState, IsolationLevel, Os, PlacementChoice, PlatformSelector,
};
use bella_core::resources::ResourceVector;
use bella_core::{MachineId, ProjectId};
use std::collections::BTreeMap;

/// The total a machine can hold right now: the static ceiling, narrowed by
/// the worker's current protection budget.
///
/// CPU, memory and disk take the smaller value. A token is limited by the
/// budget only when the budget names it; a budget that says nothing about a
/// token leaves the ceiling's count in force. Tokens the ceiling does not
/// declare are never granted.
pub fn effective_capacity(
    ceiling: &ResourceVector,
    budget: Option<&ResourceVector>,
) -> ResourceVector {
    let Some(budget) = budget else {
        return ceiling.clone();
    };
    let tokens = ceiling
        .tokens
        .iter()
        .map(|(name, count)| {
            let granted = budget.tokens.get(name).copied().unwrap_or(*count);
            (name.clone(), (*count).min(granted))
        })
        .collect();
    ResourceVector {
        cpu_milli: ceiling.cpu_milli.min(budget.cpu_milli),
        memory_bytes: ceiling.memory_bytes.min(budget.memory_bytes),
        disk_bytes: ceiling.disk_bytes.min(budget.disk_bytes),
        tokens,
    }
}

/// What a machine holds while a schedule is being computed.
#[derive(Clone, Debug)]
pub(crate) struct MachineLoad {
    pub capacity: ResourceVector,
    pub reserved: ResourceVector,
    pub held_attempts: u32,
}

impl MachineLoad {
    pub(crate) fn of(machine: &MachineState) -> Self {
        Self {
            capacity: effective_capacity(
                &machine.managed_ceiling,
                machine.effective_budget.as_ref(),
            ),
            reserved: machine.reserved.clone(),
            held_attempts: machine.held_attempts,
        }
    }
}

/// The state, indexed: machines in id order, projects by id.
pub(crate) struct Fleet<'a> {
    pub state: &'a FleetState,
    pub machines: Vec<&'a MachineState>,
    pub projects: BTreeMap<&'a ProjectId, &'a ProjectState>,
    pub totals: Totals,
    name_counts: BTreeMap<&'a str, usize>,
}

/// The name shown for a machine in explanations.
pub(crate) fn label(machine: &MachineState) -> String {
    if machine.name.is_empty() {
        machine.id.to_string()
    } else {
        machine.name.clone()
    }
}

fn blocker(
    machine: &MachineState,
    code: Code,
    permanent: bool,
    message: impl Into<String>,
) -> MachineBlocker {
    MachineBlocker {
        machine: label(machine),
        code,
        message: message.into(),
        permanent,
    }
}

fn describe(machine: &MachineState) -> String {
    if machine.os_version.is_empty() {
        machine.platform.to_string()
    } else {
        format!("{} {}", machine.platform, machine.os_version)
    }
}

fn selectors(list: &[PlatformSelector]) -> String {
    list.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a capability that is present but unusable may become usable by
/// waiting. A missing or unauthorised one never does.
fn mismatch_is_permanent(mismatch: &CapabilityMismatch) -> bool {
    !matches!(
        mismatch,
        CapabilityMismatch::NotUsable {
            state: CapabilityState::Occupied
                | CapabilityState::Unhealthy
                | CapabilityState::Unverified,
            ..
        }
    )
}

impl<'a> Fleet<'a> {
    pub(crate) fn new(state: &'a FleetState) -> Self {
        let mut machines: Vec<&MachineState> = state.machines.iter().collect();
        machines.sort_by(|a, b| a.id.cmp(&b.id));
        machines.dedup_by(|later, earlier| later.id == earlier.id);

        let mut name_counts: BTreeMap<&str, usize> = BTreeMap::new();
        for machine in &machines {
            *name_counts.entry(machine.name.as_str()).or_insert(0) += 1;
        }

        let mut projects = BTreeMap::new();
        for project in &state.projects {
            projects.entry(&project.id).or_insert(project);
        }

        let totals = Totals::of(&machines);
        Self {
            state,
            machines,
            projects,
            totals,
            name_counts,
        }
    }

    /// The first reason `machine` cannot take `job` that does not depend on
    /// what is currently reserved. A permanent reason wins over a temporary
    /// one.
    pub(crate) fn standing_blocker(
        &self,
        job: &QueuedJob,
        machine: &MachineState,
    ) -> Option<MachineBlocker> {
        if let Some(found) = self.permanent_blocker(job, machine) {
            return Some(found);
        }
        self.temporary_blocker(job, machine)
    }

    fn permanent_blocker(&self, job: &QueuedJob, machine: &MachineState) -> Option<MachineBlocker> {
        let spec = &*job.spec;
        let compat = &spec.compatibility;
        let here = describe(machine);
        let wrong_platform = |message: String| blocker(machine, Code::WrongPlatform, true, message);

        // Build host: the declared set, or the submitting platform when the
        // recipe declares none.
        if compat.build_hosts.is_empty() {
            if machine.platform != job.origin_platform {
                return Some(wrong_platform(format!(
                    "the recipe declares no build hosts, so the job is pinned to the platform \
                     that submitted it ({}); this machine is {here}",
                    job.origin_platform
                )));
            }
        } else if !compat
            .build_hosts
            .iter()
            .any(|sel| sel.matches(machine.platform, &machine.os_version))
        {
            return Some(wrong_platform(format!(
                "the recipe builds on {}; this machine is {here}",
                selectors(&compat.build_hosts)
            )));
        }

        // Test runtime: a job is never split across hosts, so the machine
        // that builds must also be where validation is allowed to run.
        if let Some(runtime) = &compat.test_runtime
            && !runtime.matches(machine.platform, &machine.os_version)
        {
            return Some(wrong_platform(format!(
                "tests must run on {runtime}; this machine is {here}, and a build is never \
                 reported as validated on a host other than its test runtime"
            )));
        }

        match &spec.placement {
            PlacementChoice::Any | PlacementChoice::Prefer { .. } => {}
            PlacementChoice::Require { platform } => {
                if !platform.matches(machine.platform, &machine.os_version) {
                    return Some(wrong_platform(format!(
                        "the submission requires {platform}; this machine is {here}"
                    )));
                }
            }
            PlacementChoice::Machine { machine: wanted } => {
                if machine.id.as_str() != wanted {
                    if machine.name != *wanted {
                        return Some(wrong_platform(format!(
                            "the submission requires machine `{wanted}`"
                        )));
                    }
                    let sharing = self.name_counts.get(wanted.as_str()).copied().unwrap_or(0);
                    if sharing > 1 {
                        return Some(wrong_platform(format!(
                            "the submission requires machine `{wanted}`, a name {sharing} \
                             machines share; request it by id ({})",
                            machine.id
                        )));
                    }
                }
            }
            PlacementChoice::Pool { pool } => {
                if !machine.policy.pools.iter().any(|p| p == pool) {
                    let member_of = if machine.policy.pools.is_empty() {
                        "no pool".to_string()
                    } else {
                        format!("`{}`", machine.policy.pools.join("`, `"))
                    };
                    return Some(wrong_platform(format!(
                        "the submission requires pool `{pool}`; this machine is in {member_of}"
                    )));
                }
            }
        }

        // The snapshot against this machine's filesystem.
        if machine.platform.os == Os::Windows
            && let Some(why) = &job.snapshot_windows_incompatible
        {
            return Some(wrong_platform(format!(
                "the snapshot cannot be materialized on Windows: {why}"
            )));
        }
        if job.snapshot_case_sensitive_only && !machine.case_sensitive_fs {
            return Some(wrong_platform(
                "the snapshot has paths that differ only by case; this machine's work volume \
                 is case-insensitive"
                    .to_string(),
            ));
        }
        if job.snapshot_has_symlinks && !machine.supports_symlinks {
            return Some(wrong_platform(
                "the snapshot contains symbolic links; this machine cannot create them".to_string(),
            ));
        }

        // v1 machines offer native execution only.
        if compat.isolation > IsolationLevel::Native {
            return Some(blocker(
                machine,
                Code::MissingCapability,
                true,
                format!(
                    "the job requires {:?} isolation; this machine offers native execution only",
                    compat.isolation
                )
                .to_lowercase(),
            ));
        }

        for requirement in &compat.requires {
            if let Err(mismatch) = requirement.check(&machine.capabilities)
                && mismatch_is_permanent(&mismatch)
            {
                return Some(blocker(
                    machine,
                    Code::MissingCapability,
                    true,
                    mismatch.to_string(),
                ));
            }
        }

        // Machine and project policy.
        if !machine.policy.accept_builds || machine.flags.contains(&Flag::BuildsDisabled) {
            return Some(blocker(
                machine,
                Code::BuildsDisabled,
                true,
                "this machine's policy does not accept builds",
            ));
        }
        let Some(project) = self.projects.get(&job.project) else {
            return Some(blocker(
                machine,
                Code::ProjectNotAllowed,
                true,
                format!(
                    "project {} is not known to the scheduler, so its policy cannot be checked",
                    job.project
                ),
            ));
        };
        if let Some(allowed) = &machine.policy.allowed_projects
            && !allowed.contains(&job.project)
        {
            return Some(blocker(
                machine,
                Code::ProjectNotAllowed,
                true,
                format!(
                    "project `{}` is not on this machine's list of allowed projects",
                    project.name
                ),
            ));
        }
        if !machine.on_lan && (spec.source_lan_only || project.policy.source_lan_only) {
            let source = if spec.source_lan_only {
                "the recipe".to_string()
            } else {
                format!("project `{}`", project.name)
            };
            return Some(blocker(
                machine,
                Code::SourceLanOnly,
                true,
                format!(
                    "{source} keeps source on the local network; this machine is not on it, \
                     and no priority or override changes that"
                ),
            ));
        }

        // A request the machine could never grant, even when idle.
        if let Some(short) = spec.envelope.peak.first_shortfall(&machine.managed_ceiling) {
            return Some(blocker(
                machine,
                Code::MachineTooSmall,
                true,
                format!(
                    "needs {}; this machine can grant at most {}",
                    units::amount(&short.dimension, short.requested),
                    units::bare(&short.dimension, short.available)
                ),
            ));
        }

        None
    }

    fn temporary_blocker(&self, job: &QueuedJob, machine: &MachineState) -> Option<MachineBlocker> {
        let config = &self.state.config;
        let has = |flag: Flag| machine.flags.contains(&flag);
        let temporary = |code: Code, message: String| blocker(machine, code, false, message);

        // A required capability that is present but busy or unhealthy.
        for requirement in &job.spec.compatibility.requires {
            if let Err(mismatch) = requirement.check(&machine.capabilities) {
                return Some(temporary(Code::MissingCapability, mismatch.to_string()));
            }
        }

        if !machine.connected {
            return Some(temporary(
                Code::MachineUnreachable,
                "not connected to the controller".to_string(),
            ));
        }
        if has(Flag::Unreachable) {
            return Some(temporary(
                Code::MachineUnreachable,
                "out of contact for too long to be offered work".to_string(),
            ));
        }
        if has(Flag::Sleeping) {
            return Some(temporary(Code::MachineUnreachable, "asleep".to_string()));
        }

        match machine.telemetry_age_ms {
            None => {
                return Some(temporary(
                    Code::StaleTelemetry,
                    "has not reported telemetry yet".to_string(),
                ));
            }
            Some(age) if age > config.stale_telemetry_ms => {
                return Some(temporary(
                    Code::StaleTelemetry,
                    format!(
                        "its last telemetry is {} old; offers need it newer than {}",
                        units::duration_ms(age),
                        units::duration_ms(config.stale_telemetry_ms)
                    ),
                ));
            }
            Some(_) => {}
        }
        if has(Flag::Stale) {
            return Some(temporary(
                Code::StaleTelemetry,
                "its heartbeat is overdue, so its telemetry cannot be trusted".to_string(),
            ));
        }
        if machine.effective_budget.is_none() {
            return Some(temporary(
                Code::StaleTelemetry,
                "has not reported a protection budget yet".to_string(),
            ));
        }

        if !machine.reconciled {
            return Some(temporary(
                Code::CleanupPending,
                "its previous attempts are still being reconciled".to_string(),
            ));
        }
        if has(Flag::Quarantined) {
            return Some(temporary(
                Code::Quarantined,
                "holds capacity whose cleanup could not be verified".to_string(),
            ));
        }
        if has(Flag::Upgrading) {
            return Some(temporary(
                Code::MachineDraining,
                "its worker is being upgraded and accepts no new work".to_string(),
            ));
        }
        if has(Flag::UserPaused) {
            return Some(temporary(
                Code::MachinePaused,
                "paused by its owner".to_string(),
            ));
        }
        if has(Flag::Draining) {
            return Some(temporary(
                Code::MachineDraining,
                "draining: it finishes running work and accepts none".to_string(),
            ));
        }
        if has(Flag::OnBattery) {
            return Some(temporary(Code::OnBattery, "running on battery".to_string()));
        }
        if has(Flag::Overheated) {
            return Some(temporary(
                Code::ForegroundProtection,
                "overheated: thermal protection is holding back managed work".to_string(),
            ));
        }
        if has(Flag::PressureLimited) {
            return Some(temporary(
                Code::MemoryPressure,
                "under memory pressure".to_string(),
            ));
        }
        if has(Flag::DiskLow) {
            return Some(temporary(
                Code::DiskLow,
                "its work volume is below the free-space floor".to_string(),
            ));
        }
        if !machine.admitting {
            return Some(temporary(
                Code::ForegroundProtection,
                "its protection loop is not admitting new work right now".to_string(),
            ));
        }

        let now = self.state.now;
        let avoid_until = job
            .recently_rejected_by
            .iter()
            .filter(|(id, until)| *id == machine.id && *until > now)
            .map(|(_, until)| *until)
            .max();
        if let Some(until) = avoid_until {
            return Some(temporary(
                Code::OfferRejected,
                format!(
                    "declined this job recently; it is offered here again in {}",
                    units::duration_ms(until.0.saturating_sub(now.0))
                ),
            ));
        }

        None
    }
}

/// The reason `machine` cannot take `job` because of what it already holds:
/// its concurrency limit, or the first dimension in which the peak does not
/// fit `capacity - reserved`.
pub(crate) fn load_blocker(
    job: &QueuedJob,
    machine: &MachineState,
    load: &MachineLoad,
) -> Option<MachineBlocker> {
    if let Some(max) = machine.policy.max_concurrent_attempts
        && load.held_attempts >= max
    {
        return Some(blocker(
            machine,
            Code::MachineConcurrencyLimit,
            false,
            format!(
                "runs {} of at most {max} concurrent attempts",
                load.held_attempts
            ),
        ));
    }

    let peak = &job.spec.envelope.peak;
    // `reserved + peak <= capacity`, the same test the ledger repeats. A
    // saturated sum only fits a capacity that is itself unbounded.
    let needed = load.reserved.saturating_add(peak);
    let short = needed.first_shortfall(&load.capacity)?;
    let dim = short.dimension.as_str();
    let capacity = short.available;
    let reserved = units::dimension_of(&load.reserved, dim);
    let wanted = units::dimension_of(peak, dim);
    let free = capacity.saturating_sub(reserved);

    let mut message = if wanted == 0 {
        format!(
            "{} is over-committed: {} reserved of {} grantable",
            dim.strip_prefix("token:").unwrap_or(dim),
            units::bare(dim, reserved),
            units::bare(dim, capacity)
        )
    } else {
        format!(
            "needs {}; {} free ({} of {} reserved)",
            units::amount(dim, wanted),
            units::bare(dim, free),
            units::bare(dim, reserved),
            units::bare(dim, capacity)
        )
    };
    let ceiling = units::dimension_of(&machine.managed_ceiling, dim);
    if capacity < ceiling {
        message.push_str(&format!(
            "; the worker currently grants {} of its {} ceiling",
            units::bare(dim, capacity),
            units::bare(dim, ceiling)
        ));
    }
    Some(blocker(machine, Code::WaitingForCapacity, false, message))
}

/// A blocker for a machine that is being kept free for another job.
pub(crate) fn held_back_blocker(machine: &MachineState, message: String) -> MachineBlocker {
    blocker(machine, Code::ReservedForOlderJob, false, message)
}

/// Every machine, in id order, with `Ok` when it could take the job right
/// now and otherwise the reason it cannot.
pub(crate) fn evaluate(
    state: &FleetState,
    job: &QueuedJob,
) -> Vec<(MachineId, Result<(), MachineBlocker>)> {
    let fleet = Fleet::new(state);
    fleet
        .machines
        .iter()
        .map(|machine| {
            let found = fleet
                .standing_blocker(job, machine)
                .or_else(|| load_blocker(job, machine, &MachineLoad::of(machine)));
            (machine.id.clone(), found.map_or(Ok(()), Err))
        })
        .collect()
}
