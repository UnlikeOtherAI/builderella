//! Builders and a small fleet simulator shared by the test modules.

use bella_core::api::ProjectPolicy;
use bella_core::job::{JobSpec, MachineBlocker, QueueReason};
use bella_core::machine::{MachinePolicy, MachineProfile};
use bella_core::platform::{
    Arch, Capability, CapabilityState, Compatibility, Os, Platform, PlatformSelector,
};
use bella_core::resources::{EnforcementReport, ResourceEnvelope, ResourceVector};
use bella_core::{Digest, JobId, MachineId, ProjectId, SessionId, UnixMillis};
use bella_scheduler::{
    FleetState, HeldAttempt, MachineState, PlannedOffer, ProjectState, QueuedJob, Schedule,
    SchedulerConfig, schedule,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const NOW: u64 = 1_700_000_000_000;

pub const LINUX: Platform = Platform {
    os: Os::Linux,
    arch: Arch::X86_64,
};
pub const MAC: Platform = Platform {
    os: Os::Macos,
    arch: Arch::Aarch64,
};
pub const WINDOWS: Platform = Platform {
    os: Os::Windows,
    arch: Arch::X86_64,
};

pub fn vector(cpus: u64, memory_gib: u64) -> ResourceVector {
    ResourceVector::new(cpus * 1000, memory_gib * GIB, 0)
}

/// A healthy Linux builder that can grant `cpus` and `memory_gib` in total.
pub fn machine(name: &str, cpus: u64, memory_gib: u64) -> MachineState {
    let ceiling = ResourceVector::new(cpus * 1000, memory_gib * GIB, 500 * GIB);
    MachineState {
        id: MachineId::derive(name.as_bytes()),
        name: name.to_string(),
        platform: LINUX,
        os_version: "24.04".to_string(),
        capabilities: Vec::new(),
        policy: MachinePolicy::defaults_for(MachineProfile::DedicatedBuilder),
        managed_ceiling: ceiling.clone(),
        effective_budget: Some(ceiling),
        reserved: ResourceVector::ZERO,
        held_attempts: 0,
        connected: true,
        admitting: true,
        flags: Vec::new(),
        telemetry_age_ms: Some(1000),
        reconciled: true,
        on_lan: true,
        enforcement: EnforcementReport::none(),
        case_sensitive_fs: true,
        supports_symlinks: true,
    }
}

pub fn project(name: &str) -> ProjectState {
    ProjectState {
        id: ProjectId::derive(name.as_bytes()),
        name: name.to_string(),
        policy: ProjectPolicy::default(),
        held_attempts: 0,
        reserved: ResourceVector::ZERO,
        environments: 0,
    }
}

pub fn session(name: &str) -> SessionId {
    SessionId::derive(name.as_bytes())
}

/// A job specification that may build on any platform.
pub fn spec(cpus: u64, memory_gib: u64) -> JobSpec {
    JobSpec {
        recipe: "test".to_string(),
        recipe_digest: Digest::of_bytes(b"recipe"),
        steps: Vec::new(),
        envelope: ResourceEnvelope::fixed(vector(cpus, memory_gib)),
        compatibility: Compatibility {
            build_hosts: vec![PlatformSelector::default()],
            ..Compatibility::default()
        },
        placement: Default::default(),
        priority: Default::default(),
        retry_class: Default::default(),
        max_attempts: 1,
        timeout_secs: 3600,
        cleanup_timeout_secs: 120,
        cloud: Default::default(),
        source_lan_only: false,
        outputs: Default::default(),
        environment: None,
        env: BTreeMap::new(),
        disconnected_continuation_secs: 0,
        parallelism: None,
    }
}

/// A job of `project`, submitted from Linux, that has waited `age_ms`.
pub fn job(name: &str, project: &ProjectState, age_ms: u64, spec: JobSpec) -> QueuedJob {
    QueuedJob {
        id: JobId::derive(name.as_bytes()),
        project: project.id.clone(),
        session: None,
        spec: Arc::new(spec),
        origin_platform: LINUX,
        queued_at: UnixMillis(NOW - age_ms),
        attempts_made: 0,
        snapshot_windows_incompatible: None,
        snapshot_case_sensitive_only: false,
        snapshot_has_symlinks: false,
        has_unresolved_attempt: false,
        recently_rejected_by: Vec::new(),
    }
}

pub fn capability(name: &str, version: &str) -> Capability {
    Capability {
        name: name.to_string(),
        version: Some(version.to_string()),
        state: CapabilityState::Installed,
        attributes: BTreeMap::new(),
    }
}

pub fn fleet(
    machines: Vec<MachineState>,
    projects: Vec<ProjectState>,
    queue: Vec<QueuedJob>,
) -> FleetState {
    FleetState {
        now: UnixMillis(NOW),
        reconciling: false,
        machines,
        queue,
        projects,
        held: Vec::new(),
        config: SchedulerConfig::default(),
    }
}

/// A configuration in which neither aging nor the starvation rule fires.
pub fn patient() -> SchedulerConfig {
    SchedulerConfig {
        aging_after_ms: u64::MAX,
        starvation_threshold_ms: u64::MAX,
        ..SchedulerConfig::default()
    }
}

pub fn offer_for<'a>(plan: &'a Schedule, job: &QueuedJob) -> Option<&'a PlannedOffer> {
    plan.offers.iter().find(|offer| offer.job_id == job.id)
}

#[track_caller]
pub fn offered_machine(plan: &Schedule, job: &QueuedJob) -> MachineId {
    match offer_for(plan, job) {
        Some(offer) => offer.machine_id.clone(),
        None => panic!(
            "expected an offer for {}, got reason {:?}",
            job.id,
            plan.reasons.get(&job.id)
        ),
    }
}

#[track_caller]
pub fn reason_for<'a>(plan: &'a Schedule, job: &QueuedJob) -> &'a QueueReason {
    match plan.reasons.get(&job.id) {
        Some(reason) => reason,
        None => panic!(
            "expected a reason for {}, got offer {:?}",
            job.id,
            offer_for(plan, job)
        ),
    }
}

#[track_caller]
pub fn blocker_on<'a>(reason: &'a QueueReason, machine: &str) -> &'a MachineBlocker {
    match reason.blockers.iter().find(|b| b.machine == machine) {
        Some(blocker) => blocker,
        None => panic!("no blocker for machine {machine} in {reason:?}"),
    }
}

/// Deterministic pseudo-random numbers (SplitMix64) for generated fleets.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// `true` once in `one_in` calls on average.
    pub fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

struct Running {
    job: QueuedJob,
    machine: MachineId,
    ends_at_tick: u64,
}

/// Runs the scheduler tick after tick, starting every offered job at once
/// and releasing its reservation when it "finishes".
///
/// `base` holds the fleet without any simulated work; each tick the
/// running jobs are added to machines, projects and `held` before
/// `schedule` is called, the way the controller would rebuild its state.
pub struct Sim {
    pub base: FleetState,
    pub tick_ms: u64,
    pub tick: u64,
    running: Vec<Running>,
    /// `(tick, job, machine)` for every start, in order.
    pub started: Vec<(u64, QueuedJob, MachineId)>,
}

impl Sim {
    pub fn new(base: FleetState, tick_ms: u64) -> Self {
        Self {
            base,
            tick_ms,
            tick: 0,
            running: Vec::new(),
            started: Vec::new(),
        }
    }

    /// The state the scheduler sees at the current tick.
    pub fn view(&self) -> FleetState {
        let mut state = self.base.clone();
        for run in &self.running {
            let peak = &run.job.spec.envelope.peak;
            for machine in &mut state.machines {
                if machine.id == run.machine {
                    machine.reserved = machine.reserved.saturating_add(peak);
                    machine.held_attempts += 1;
                }
            }
            for project in &mut state.projects {
                if project.id == run.job.project {
                    project.reserved = project.reserved.saturating_add(peak);
                    project.held_attempts += 1;
                }
            }
            state.held.push(HeldAttempt {
                project: run.job.project.clone(),
                session: run.job.session.clone(),
                priority: run.job.spec.priority,
                reserved: peak.clone(),
            });
        }
        state
    }

    /// One tick: release finished jobs, schedule, start what was offered.
    /// `duration` gives each started job's length in ticks.
    pub fn step(&mut self, mut duration: impl FnMut(&QueuedJob) -> u64) -> Schedule {
        let tick = self.tick;
        self.running.retain(|run| run.ends_at_tick > tick);
        let plan = schedule(&self.view());
        for offer in &plan.offers {
            let position = self
                .base
                .queue
                .iter()
                .position(|queued| queued.id == offer.job_id)
                .expect("an offer names a queued job");
            let job = self.base.queue.remove(position);
            self.started
                .push((tick, job.clone(), offer.machine_id.clone()));
            self.running.push(Running {
                ends_at_tick: tick + duration(&job).max(1),
                machine: offer.machine_id.clone(),
                job,
            });
        }
        self.tick += 1;
        self.base.now = UnixMillis(self.base.now.0 + self.tick_ms);
        plan
    }

    pub fn start_tick(&self, job: &QueuedJob) -> Option<u64> {
        self.started
            .iter()
            .find(|(_, started, _)| started.id == job.id)
            .map(|(tick, ..)| *tick)
    }

    /// Queue a job that arrives now.
    pub fn submit(&mut self, mut job: QueuedJob) {
        job.queued_at = self.base.now;
        self.base.queue.push(job);
    }
}
