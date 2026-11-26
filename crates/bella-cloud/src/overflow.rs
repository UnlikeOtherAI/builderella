//! The overflow policy: when does queued work justify a new cloud worker?
//!
//! [`evaluate`] is a pure function. It reads no clock and no database; the
//! same inputs always give the same answer. It only proposes capacity. Each
//! proposal still has to pass [`crate::Engine::request_capacity`] and a
//! budget authorization, so the policy cannot spend anything by itself.
//!
//! What keeps it from over-asking:
//!
//! * Capacity that is already requested, provisioning or bootstrapping
//!   counts. Jobs it was requested for are not asked for again, and its
//!   free room is filled before a new worker is proposed, so twenty jobs
//!   that fit one worker produce one request.
//! * Hard limits: most pending workers, most workers in total, most
//!   launches per time window.
//! * A preset whose last worker failed to bootstrap is left alone for a
//!   cooldown.
//! * Hysteresis: a worker is only released after it has been idle for a
//!   while and the queue is well below the level that would request one,
//!   and after a release the queue-length triggers stay quiet for a
//!   cooldown, so capacity is not requested and dropped in turns.
//! * No warm pool unless one is configured.

use crate::engine::{Actor, NewCapacity};
use crate::intent::RequestMode;
use crate::provider::PurchaseModel;
use bella_core::job::{CloudMode, Priority};
use bella_core::platform::Platform;
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::{CloudIntentId, JobId, ProjectId, UnixMillis};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeSet;

/// A machine shape the policy may ask for. The purchase model is part of
/// the choice; the policy never swaps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverflowPreset {
    pub name: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub platform: Platform,
    pub resources: ResourceVector,
    pub lifetime_secs: u64,
}

/// The schedulable capacity of one enrolled machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalMachine {
    pub platform: Platform,
    pub capacity: ResourceVector,
}

/// The standing policy. The default is disabled and asks for nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverflowPolicy {
    pub enabled: bool,
    /// Provider the policy asks; capacity and history of other providers
    /// are ignored.
    pub provider: String,
    /// Presets in order of preference. The first one of the right platform
    /// that fits a job is used.
    pub presets: Vec<OverflowPreset>,
    /// Enrolled machines, for the "larger than any local machine" trigger.
    pub local_machines: Vec<LocalMachine>,
    /// Predicted wait at or above this asks for capacity.
    pub wait_threshold_ms: u64,
    /// Time in the queue at or above this asks for capacity.
    pub queue_age_threshold_ms: u64,
    /// A deadline closer than the predicted wait plus this margin is at risk.
    pub deadline_margin_ms: u64,
    /// Most workers that may be pending at once.
    pub max_pending: u32,
    /// Most workers that may exist at once, pending and closing included.
    pub max_active: u32,
    /// Most launches within `launch_window_ms`.
    pub max_launches: u32,
    pub launch_window_ms: u64,
    /// How long a preset is left alone after one of its workers failed to
    /// bootstrap.
    pub bootstrap_failure_cooldown_ms: u64,
    /// A worker is kept while a job it could run is predicted to wait at
    /// least this long. Lower than `wait_threshold_ms`.
    pub release_wait_ms: u64,
    /// A worker is released only after being idle this long.
    pub idle_release_after_ms: u64,
    /// After a release, the wait and queue-age triggers stay quiet for this
    /// long on that platform. Deadline and size triggers are not delayed.
    pub reacquire_cooldown_ms: u64,
    /// Idle workers to keep ready. Zero by default.
    pub warm_pool: u32,
    /// The project a warm pool is billed to. No warm pool without one.
    pub warm_pool_project: Option<ProjectId>,
}

impl Default for OverflowPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            presets: Vec::new(),
            local_machines: Vec::new(),
            wait_threshold_ms: 5 * 60 * 1000,
            queue_age_threshold_ms: 10 * 60 * 1000,
            deadline_margin_ms: 2 * 60 * 1000,
            max_pending: 1,
            max_active: 2,
            max_launches: 2,
            launch_window_ms: 10 * 60 * 1000,
            bootstrap_failure_cooldown_ms: 15 * 60 * 1000,
            release_wait_ms: 60 * 1000,
            idle_release_after_ms: 5 * 60 * 1000,
            reacquire_cooldown_ms: 10 * 60 * 1000,
            warm_pool: 0,
            warm_pool_project: None,
        }
    }
}

/// One queued job as the policy sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Demand {
    pub job: JobId,
    pub project: ProjectId,
    pub platform: Platform,
    pub envelope: ResourceEnvelope,
    pub queued_ms: u64,
    /// The scheduler's estimate; `None` when it has none.
    pub predicted_wait_ms: Option<u64>,
    pub deadline: Option<UnixMillis>,
    /// Effective cloud mode of the job. Only `AutoOverflow` and `Hybrid`
    /// let the policy act.
    pub cloud: CloudMode,
    /// The source may not leave the LAN. Such a job never causes a request.
    pub source_lan_only: bool,
}

string_enum! {
    /// Where a cloud worker is in its life, as far as the policy cares.
    pub enum CapacityPhase {
        /// Requested, provisioning or bootstrapping: not usable yet, counted.
        Pending => "pending",
        /// Registered and usable.
        Active => "active",
        /// Closing. Counts towards the total, offers no capacity.
        Draining => "draining",
    }
}

/// A cloud worker that exists or is on its way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOrActive {
    pub intent: CloudIntentId,
    pub provider: String,
    pub project: ProjectId,
    pub platform: Platform,
    pub phase: CapacityPhase,
    /// Room not yet promised to anything.
    pub free: ResourceVector,
    /// Jobs this capacity was requested for.
    pub covers: Vec<JobId>,
    pub since: UnixMillis,
    /// Since when an active worker has had nothing to do.
    pub idle_since: Option<UnixMillis>,
}

/// What became of a launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LaunchOutcome {
    /// Requested; in progress, running, or lost for another reason.
    Launched,
    /// The worker never registered.
    BootstrapFailed { at: UnixMillis },
    /// The worker was released after use.
    Released { at: UnixMillis },
}

/// One past launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub provider: String,
    pub platform: Platform,
    pub preset: String,
    pub at: UnixMillis,
    pub outcome: LaunchOutcome,
}

/// Past launches. Must reach back at least as far as the launch window and
/// both cooldowns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchHistory {
    pub launches: Vec<LaunchRecord>,
}

string_enum! {
    /// Why capacity was asked for.
    pub enum Trigger {
        PredictedWait => "predicted_wait",
        QueueAge => "queue_age",
        DeadlineAtRisk => "deadline_at_risk",
        /// No enrolled machine of the job's platform is large enough.
        ExceedsLocalMachines => "exceeds_local_machines",
        WarmPool => "warm_pool",
    }
}

/// A proposal for one new worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityRequest {
    /// Stable for unchanged inputs, so submitting the same proposal twice
    /// yields one intention.
    pub intent_id: CloudIntentId,
    pub provider: String,
    pub preset: String,
    pub region: String,
    pub purchase: PurchaseModel,
    pub platform: Platform,
    pub lifetime_secs: u64,
    /// The project the worker is billed to. A worker is never shared
    /// between projects.
    pub project: ProjectId,
    /// The first of `covers`; `None` for a warm-pool worker.
    pub job: Option<JobId>,
    /// Every job this worker is meant to take, in identifier order.
    pub covers: Vec<JobId>,
    pub triggers: Vec<Trigger>,
}

impl CapacityRequest {
    /// The engine request for this proposal. It is an automatic request, so
    /// the project's cloud mode must permit automatic expansion.
    pub fn to_new_capacity(
        &self,
        requested_by: Actor,
        project_cloud: CloudMode,
        image: &str,
    ) -> NewCapacity {
        NewCapacity {
            id: self.intent_id.clone(),
            project: self.project.clone(),
            job: self.job.clone(),
            covers: self.covers.clone(),
            project_cloud,
            job_cloud: None,
            source_lan_only: false,
            priority: Priority::Normal,
            escalated: false,
            preset: self.preset.clone(),
            region: self.region.clone(),
            purchase: self.purchase,
            image: image.to_string(),
            lifetime_secs: self.lifetime_secs,
            requested_by,
            mode: RequestMode::Automatic,
            retry_of: None,
        }
    }
}

fn within(at: UnixMillis, now: UnixMillis, window_ms: u64) -> bool {
    // A timestamp from the future is treated as just now.
    at > now || now.0 - at.0 < window_ms
}

fn eligible(demand: &Demand) -> bool {
    matches!(demand.cloud, CloudMode::AutoOverflow | CloudMode::Hybrid) && !demand.source_lan_only
}

/// The triggers a job fires. A job with no enrolled machine of its platform
/// counts as larger than any local machine.
pub fn triggers(policy: &OverflowPolicy, demand: &Demand, now: UnixMillis) -> Vec<Trigger> {
    let mut fired = Vec::new();
    if demand
        .predicted_wait_ms
        .is_some_and(|wait| wait >= policy.wait_threshold_ms)
    {
        fired.push(Trigger::PredictedWait);
    }
    if demand.queued_ms >= policy.queue_age_threshold_ms {
        fired.push(Trigger::QueueAge);
    }
    if let Some(deadline) = demand.deadline {
        let earliest_start = now
            .0
            .saturating_add(demand.predicted_wait_ms.unwrap_or(0))
            .saturating_add(policy.deadline_margin_ms);
        if earliest_start > deadline.0 {
            fired.push(Trigger::DeadlineAtRisk);
        }
    }
    let fits_locally = policy.local_machines.iter().any(|machine| {
        machine.platform == demand.platform && demand.envelope.peak.fits_within(&machine.capacity)
    });
    if !fits_locally {
        fired.push(Trigger::ExceedsLocalMachines);
    }
    fired
}

fn is_hard(trigger: Trigger) -> bool {
    matches!(
        trigger,
        Trigger::DeadlineAtRisk | Trigger::ExceedsLocalMachines
    )
}

fn cooling_down(
    policy: &OverflowPolicy,
    history: &LaunchHistory,
    preset: &OverflowPreset,
    now: UnixMillis,
) -> bool {
    history.launches.iter().any(|launch| {
        launch.provider == policy.provider
            && launch.preset == preset.name
            && matches!(
                launch.outcome,
                LaunchOutcome::BootstrapFailed { at }
                    if within(at, now, policy.bootstrap_failure_cooldown_ms)
            )
    })
}

fn recently_released(
    policy: &OverflowPolicy,
    history: &LaunchHistory,
    platform: Platform,
    now: UnixMillis,
) -> bool {
    history.launches.iter().any(|launch| {
        launch.provider == policy.provider
            && launch.platform == platform
            && matches!(
                launch.outcome,
                LaunchOutcome::Released { at } if within(at, now, policy.reacquire_cooldown_ms)
            )
    })
}

struct Bin {
    project: ProjectId,
    platform: Platform,
    free: ResourceVector,
}

struct NewBin<'p> {
    preset: &'p OverflowPreset,
    project: ProjectId,
    free: ResourceVector,
    jobs: Vec<JobId>,
    triggers: BTreeSet<Trigger>,
}

fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Decides which new workers to ask for.
///
/// `capacity` is every cloud worker that exists or is on its way, in
/// creation order (the order [`crate::Engine::overflow_view`] returns).
/// The result never exceeds the policy's pending, total and launch-rate
/// limits. Evaluating again with the same inputs gives the same proposals
/// with the same identifiers; evaluating again after the proposals were
/// recorded as pending capacity gives none.
///
/// A job contributes only when its cloud mode allows automatic expansion,
/// its source is not LAN-only, it fires a trigger, and no existing or
/// pending worker already covers it or has room for it.
pub fn evaluate(
    policy: &OverflowPolicy,
    demand: &[Demand],
    capacity: &[PendingOrActive],
    history: &LaunchHistory,
    now: UnixMillis,
) -> Vec<CapacityRequest> {
    if !policy.enabled || policy.presets.is_empty() {
        return Vec::new();
    }
    let own: Vec<&PendingOrActive> = capacity
        .iter()
        .filter(|entry| entry.provider == policy.provider)
        .collect();
    let pending = count_u32(
        own.iter()
            .filter(|entry| entry.phase == CapacityPhase::Pending)
            .count(),
    );
    let total = count_u32(own.len());
    let provider_launches: Vec<&LaunchRecord> = history
        .launches
        .iter()
        .filter(|launch| launch.provider == policy.provider)
        .collect();
    let recent = count_u32(
        provider_launches
            .iter()
            .filter(|launch| within(launch.at, now, policy.launch_window_ms))
            .count(),
    );
    let mut budget = policy
        .max_pending
        .saturating_sub(pending)
        .min(policy.max_active.saturating_sub(total))
        .min(policy.max_launches.saturating_sub(recent));

    // Room in workers that exist or are on their way, less what the jobs
    // they were requested for will take.
    let mut covered: BTreeSet<&JobId> = BTreeSet::new();
    let mut bins: Vec<Bin> = Vec::new();
    for entry in &own {
        if entry.phase == CapacityPhase::Draining {
            continue;
        }
        let mut free = entry.free.clone();
        for job in &entry.covers {
            covered.insert(job);
            if let Some(queued) = demand.iter().find(|queued| &queued.job == job) {
                free = free.saturating_sub(&queued.envelope.peak);
            }
        }
        bins.push(Bin {
            project: entry.project.clone(),
            platform: entry.platform,
            free,
        });
    }

    let mut seen: BTreeSet<&JobId> = BTreeSet::new();
    let mut candidates: Vec<(&Demand, Vec<Trigger>)> = Vec::new();
    for queued in demand {
        if !eligible(queued) || covered.contains(&queued.job) || !seen.insert(&queued.job) {
            continue;
        }
        let fired = triggers(policy, queued, now);
        if !fired.is_empty() {
            candidates.push((queued, fired));
        }
    }
    // Largest first within a project and platform, so small jobs fill the
    // room large ones leave.
    candidates.sort_by_cached_key(|(queued, _)| {
        (
            queued.project.clone(),
            queued.platform,
            Reverse(queued.envelope.peak.memory_bytes),
            Reverse(queued.envelope.peak.cpu_milli),
            queued.job.clone(),
        )
    });

    let mut opened: Vec<NewBin<'_>> = Vec::new();
    for (queued, fired) in candidates {
        let peak = &queued.envelope.peak;
        if let Some(bin) = bins.iter_mut().find(|bin| {
            bin.project == queued.project
                && bin.platform == queued.platform
                && peak.fits_within(&bin.free)
        }) {
            bin.free = bin.free.saturating_sub(peak);
            continue;
        }
        if let Some(bin) = opened.iter_mut().find(|bin| {
            bin.project == queued.project
                && bin.preset.platform == queued.platform
                && peak.fits_within(&bin.free)
        }) {
            bin.free = bin.free.saturating_sub(peak);
            bin.jobs.push(queued.job.clone());
            bin.triggers.extend(fired);
            continue;
        }
        if budget == 0 {
            continue;
        }
        let urgent = fired.iter().any(|trigger| is_hard(*trigger));
        if !urgent && recently_released(policy, history, queued.platform, now) {
            continue;
        }
        let Some(preset) = policy.presets.iter().find(|preset| {
            preset.platform == queued.platform
                && peak.fits_within(&preset.resources)
                && !cooling_down(policy, history, preset, now)
        }) else {
            continue;
        };
        budget -= 1;
        opened.push(NewBin {
            preset,
            project: queued.project.clone(),
            free: preset.resources.saturating_sub(peak),
            jobs: vec![queued.job.clone()],
            triggers: fired.into_iter().collect(),
        });
    }

    if policy.warm_pool > 0
        && let Some(project) = &policy.warm_pool_project
        && let Some(preset) = policy.presets.first()
        && !cooling_down(policy, history, preset, now)
    {
        let warm = count_u32(
            own.iter()
                .filter(|entry| {
                    entry.phase != CapacityPhase::Draining
                        && entry.covers.is_empty()
                        && &entry.project == project
                })
                .count(),
        );
        let mut missing = policy.warm_pool.saturating_sub(warm);
        while missing > 0 && budget > 0 {
            missing -= 1;
            budget -= 1;
            opened.push(NewBin {
                preset,
                project: project.clone(),
                free: preset.resources.clone(),
                jobs: Vec::new(),
                triggers: BTreeSet::from([Trigger::WarmPool]),
            });
        }
    }

    // The generation changes whenever a launch is recorded, so a proposal
    // made after an earlier worker for the same job came and went gets a
    // new identifier instead of resolving to the old intention.
    let window = policy.launch_window_ms.max(1);
    let generation = provider_launches
        .iter()
        .map(|launch| launch.at.0)
        .max()
        .unwrap_or(0)
        .max(now.0 - now.0 % window);
    let mut warm_slot = 0u32;
    opened
        .into_iter()
        .map(|mut bin| {
            bin.jobs.sort();
            let anchor = match bin.jobs.first() {
                Some(job) => job.as_str().to_string(),
                None => {
                    warm_slot += 1;
                    format!("warm-{warm_slot}")
                }
            };
            let seed = format!(
                "overflow|{}|{}|{}|{}|{}|{}|{}",
                policy.provider,
                bin.preset.name,
                bin.preset.region,
                bin.preset.purchase,
                bin.project,
                anchor,
                generation
            );
            CapacityRequest {
                intent_id: CloudIntentId::derive(seed.as_bytes()),
                provider: policy.provider.clone(),
                preset: bin.preset.name.clone(),
                region: bin.preset.region.clone(),
                purchase: bin.preset.purchase,
                platform: bin.preset.platform,
                lifetime_secs: bin.preset.lifetime_secs,
                project: bin.project,
                job: bin.jobs.first().cloned(),
                covers: bin.jobs,
                triggers: bin.triggers.into_iter().collect(),
            }
        })
        .collect()
}

/// Which idle workers may be released.
///
/// A worker qualifies when it has been idle for `idle_release_after_ms`, no
/// queued job it could run is waiting at least `release_wait_ms` or fires a
/// trigger, and it is not needed for the warm pool. Because
/// `release_wait_ms` is below `wait_threshold_ms`, a queue hovering between
/// the two neither releases a worker nor requests one.
pub fn release_candidates(
    policy: &OverflowPolicy,
    demand: &[Demand],
    capacity: &[PendingOrActive],
    now: UnixMillis,
) -> Vec<CloudIntentId> {
    let mut warm_to_keep = match &policy.warm_pool_project {
        Some(_) => policy.warm_pool,
        None => 0,
    };
    let mut release = Vec::new();
    for entry in capacity {
        if entry.provider != policy.provider || entry.phase != CapacityPhase::Active {
            continue;
        }
        let Some(idle_since) = entry.idle_since else {
            continue;
        };
        if within(idle_since, now, policy.idle_release_after_ms) {
            continue;
        }
        let wanted = demand.iter().any(|queued| {
            eligible(queued)
                && queued.project == entry.project
                && queued.platform == entry.platform
                && queued.envelope.peak.fits_within(&entry.free)
                && (queued.predicted_wait_ms.unwrap_or(0) >= policy.release_wait_ms
                    || entry.covers.contains(&queued.job)
                    || !triggers(policy, queued, now).is_empty())
        });
        if wanted {
            continue;
        }
        let is_warm =
            entry.covers.is_empty() && policy.warm_pool_project.as_ref() == Some(&entry.project);
        if is_warm && warm_to_keep > 0 {
            warm_to_keep -= 1;
            continue;
        }
        release.push(entry.intent.clone());
    }
    release
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::defaults::GIB;
    use bella_core::platform::{Arch, Os};

    const LINUX: Platform = Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    };
    const WINDOWS: Platform = Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
    };
    const MAC: Platform = Platform {
        os: Os::Macos,
        arch: Arch::Aarch64,
    };
    const MINUTE: u64 = 60_000;

    fn vector(cpus: u64, memory_gib: u64) -> ResourceVector {
        ResourceVector::new(cpus * 1000, memory_gib * GIB, 10 * GIB)
    }

    fn preset(name: &str, platform: Platform, cpus: u64, memory_gib: u64) -> OverflowPreset {
        OverflowPreset {
            name: name.to_string(),
            region: "sim-east".to_string(),
            purchase: PurchaseModel::Spot,
            platform,
            resources: ResourceVector::new(cpus * 1000, memory_gib * GIB, 100 * GIB),
            lifetime_secs: 3600,
        }
    }

    fn policy() -> OverflowPolicy {
        OverflowPolicy {
            enabled: true,
            provider: "sim".to_string(),
            presets: vec![
                preset("linux-small", LINUX, 4, 8),
                preset("linux-large", LINUX, 16, 64),
                preset("windows-large", WINDOWS, 16, 64),
            ],
            local_machines: vec![LocalMachine {
                platform: LINUX,
                capacity: vector(32, 128),
            }],
            max_pending: 2,
            max_active: 4,
            max_launches: 3,
            ..OverflowPolicy::default()
        }
    }

    fn job(project: &ProjectId, cpus: u64, memory_gib: u64, wait_ms: u64) -> Demand {
        Demand {
            job: JobId::generate(),
            project: project.clone(),
            platform: LINUX,
            envelope: ResourceEnvelope::fixed(vector(cpus, memory_gib)),
            queued_ms: 0,
            predicted_wait_ms: Some(wait_ms),
            deadline: None,
            cloud: CloudMode::AutoOverflow,
            source_lan_only: false,
        }
    }

    fn as_pending(
        request: &CapacityRequest,
        policy: &OverflowPolicy,
        now: UnixMillis,
    ) -> PendingOrActive {
        let preset = policy
            .presets
            .iter()
            .find(|p| p.name == request.preset)
            .unwrap();
        PendingOrActive {
            intent: request.intent_id.clone(),
            provider: request.provider.clone(),
            project: request.project.clone(),
            platform: request.platform,
            phase: CapacityPhase::Pending,
            free: preset.resources.clone(),
            covers: request.covers.clone(),
            since: now,
            idle_since: None,
        }
    }

    fn launched(request: &CapacityRequest, now: UnixMillis) -> LaunchRecord {
        LaunchRecord {
            provider: request.provider.clone(),
            platform: request.platform,
            preset: request.preset.clone(),
            at: now,
            outcome: LaunchOutcome::Launched,
        }
    }

    const NOW: UnixMillis = UnixMillis(1_800_000_000_000);

    #[test]
    fn the_default_policy_asks_for_nothing_and_keeps_no_warm_pool() {
        let project = ProjectId::generate();
        let demand = vec![job(&project, 2, 4, 60 * MINUTE)];
        let default = OverflowPolicy::default();
        assert_eq!(default.warm_pool, 0);
        assert!(evaluate(&default, &demand, &[], &LaunchHistory::default(), NOW).is_empty());
        // An enabled policy with nothing queued asks for nothing either.
        assert!(evaluate(&policy(), &[], &[], &LaunchHistory::default(), NOW).is_empty());
    }

    #[test]
    fn each_trigger_fires_on_its_own() {
        let project = ProjectId::generate();
        let policy = policy();
        let calm = job(&project, 2, 4, MINUTE);
        assert!(triggers(&policy, &calm, NOW).is_empty());
        assert!(
            evaluate(
                &policy,
                &[calm.clone()],
                &[],
                &LaunchHistory::default(),
                NOW
            )
            .is_empty()
        );

        let waiting = job(&project, 2, 4, 5 * MINUTE);
        assert_eq!(
            triggers(&policy, &waiting, NOW),
            vec![Trigger::PredictedWait]
        );

        let mut old = calm.clone();
        old.queued_ms = 10 * MINUTE;
        assert_eq!(triggers(&policy, &old, NOW), vec![Trigger::QueueAge]);

        let mut late = calm.clone();
        late.deadline = Some(UnixMillis(NOW.0 + 2 * MINUTE));
        assert_eq!(triggers(&policy, &late, NOW), vec![Trigger::DeadlineAtRisk]);
        late.deadline = Some(UnixMillis(NOW.0 + 30 * MINUTE));
        assert!(triggers(&policy, &late, NOW).is_empty());

        // 48 CPUs fit no enrolled machine and no preset: the trigger fires,
        // but there is nothing to ask for.
        let huge = job(&project, 48, 16, 0);
        assert_eq!(
            triggers(&policy, &huge, NOW),
            vec![Trigger::ExceedsLocalMachines]
        );
        assert!(evaluate(&policy, &[huge], &[], &LaunchHistory::default(), NOW).is_empty());

        // No Windows machine is enrolled, so a Windows job exceeds them all.
        let mut windows = calm.clone();
        windows.platform = WINDOWS;
        let requests = evaluate(&policy, &[windows], &[], &LaunchHistory::default(), NOW);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].preset, "windows-large");
        assert_eq!(requests[0].triggers, vec![Trigger::ExceedsLocalMachines]);

        // No preset exists for macOS.
        let mut mac = calm;
        mac.platform = MAC;
        assert!(evaluate(&policy, &[mac], &[], &LaunchHistory::default(), NOW).is_empty());
    }

    #[test]
    fn twenty_simultaneous_jobs_for_one_shortage_produce_one_worker() {
        let project = ProjectId::generate();
        let policy = policy();
        // Twenty jobs of 0.5 CPU and 2 GiB: 10 CPUs and 40 GiB together,
        // which one linux-large holds. The first one is a little larger so
        // the preset chosen for it has room for the rest.
        let small = || {
            let mut small = job(&project, 1, 2, 6 * MINUTE);
            small.envelope = ResourceEnvelope::fixed(ResourceVector::new(500, 2 * GIB, GIB));
            small
        };
        let mut demand: Vec<Demand> = (0..19).map(|_| small()).collect();
        demand.push(job(&project, 6, 10, 6 * MINUTE));

        let history = LaunchHistory::default();
        let first = evaluate(&policy, &demand, &[], &history, NOW);
        assert_eq!(first.len(), 1, "one worker for one shortage");
        assert_eq!(first[0].preset, "linux-large");
        assert_eq!(first[0].covers.len(), 20);

        // Twenty callers evaluating the same state get the same proposal.
        for _ in 0..20 {
            assert_eq!(evaluate(&policy, &demand, &[], &history, NOW), first);
        }

        // Once it is pending, the same shortage asks for nothing more,
        // however often it is evaluated.
        let capacity = vec![as_pending(&first[0], &policy, NOW)];
        let history = LaunchHistory {
            launches: vec![launched(&first[0], NOW)],
        };
        for step in 0..20 {
            let later = UnixMillis(NOW.0 + step * 1000);
            assert!(evaluate(&policy, &demand, &capacity, &history, later).is_empty());
        }

        // A job that arrives later and fits the pending worker's free room
        // does not cause another one.
        demand.push(small());
        assert!(evaluate(&policy, &demand, &capacity, &history, NOW).is_empty());
    }

    #[test]
    fn limits_cap_pending_total_and_launch_rate() {
        let project = ProjectId::generate();
        let mut policy = policy();
        // Five jobs that each need a whole linux-large.
        let demand: Vec<Demand> = (0..5).map(|_| job(&project, 16, 64, 6 * MINUTE)).collect();
        let none = LaunchHistory::default();

        assert_eq!(evaluate(&policy, &demand, &[], &none, NOW).len(), 2);
        policy.max_pending = 10;
        assert_eq!(
            evaluate(&policy, &demand, &[], &none, NOW).len(),
            3,
            "launch rate"
        );
        policy.max_launches = 10;
        assert_eq!(
            evaluate(&policy, &demand, &[], &none, NOW).len(),
            4,
            "total"
        );

        // Launches inside the window count against the rate; older ones do not.
        policy.max_launches = 2;
        let record = |age_ms: u64| LaunchRecord {
            provider: "sim".to_string(),
            platform: LINUX,
            preset: "linux-large".to_string(),
            at: UnixMillis(NOW.0 - age_ms),
            outcome: LaunchOutcome::Launched,
        };
        let history = LaunchHistory {
            launches: vec![record(MINUTE), record(11 * MINUTE)],
        };
        assert_eq!(evaluate(&policy, &demand, &[], &history, NOW).len(), 1);

        // Capacity and launches of another provider are not this policy's.
        let mut foreign = record(MINUTE);
        foreign.provider = "other".to_string();
        let history = LaunchHistory {
            launches: vec![foreign],
        };
        assert_eq!(evaluate(&policy, &demand, &[], &history, NOW).len(), 2);
    }

    #[test]
    fn a_failed_bootstrap_cools_the_preset_down() {
        let project = ProjectId::generate();
        let policy = policy();
        let demand = vec![job(&project, 16, 64, 6 * MINUTE)];
        let failed = |age_ms: u64| LaunchHistory {
            launches: vec![LaunchRecord {
                provider: "sim".to_string(),
                platform: LINUX,
                preset: "linux-large".to_string(),
                at: UnixMillis(NOW.0 - age_ms - MINUTE),
                outcome: LaunchOutcome::BootstrapFailed {
                    at: UnixMillis(NOW.0 - age_ms),
                },
            }],
        };
        assert!(evaluate(&policy, &demand, &[], &failed(MINUTE), NOW).is_empty());
        assert!(evaluate(&policy, &demand, &[], &failed(14 * MINUTE), NOW).is_empty());
        assert_eq!(
            evaluate(&policy, &demand, &[], &failed(15 * MINUTE), NOW).len(),
            1
        );
        // The cooldown is per preset: a job the small preset can take is
        // still served.
        let small = vec![job(&project, 2, 4, 6 * MINUTE)];
        let requests = evaluate(&policy, &small, &[], &failed(MINUTE), NOW);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].preset, "linux-small");
    }

    #[test]
    fn work_that_may_not_use_the_cloud_never_causes_a_request() {
        let project = ProjectId::generate();
        let policy = policy();
        for cloud in [CloudMode::ExistingOnly, CloudMode::Manual] {
            let mut blocked = job(&project, 2, 4, 60 * MINUTE);
            blocked.cloud = cloud;
            blocked.queued_ms = 60 * MINUTE;
            blocked.deadline = Some(NOW);
            assert!(evaluate(&policy, &[blocked], &[], &LaunchHistory::default(), NOW).is_empty());
        }
        let mut hybrid = job(&project, 2, 4, 60 * MINUTE);
        hybrid.cloud = CloudMode::Hybrid;
        assert_eq!(
            evaluate(&policy, &[hybrid], &[], &LaunchHistory::default(), NOW).len(),
            1
        );
    }

    /// A55 at the policy level: a LAN-only job never causes a request,
    /// whatever fires.
    #[test]
    fn a55_lan_only_demand_never_causes_a_request() {
        let project = ProjectId::generate();
        let policy = policy();
        let mut lan = job(&project, 2, 4, 600 * MINUTE);
        lan.source_lan_only = true;
        lan.queued_ms = 600 * MINUTE;
        lan.deadline = Some(UnixMillis(NOW.0 - 1));
        lan.cloud = CloudMode::Hybrid;
        assert_eq!(triggers(&policy, &lan, NOW).len(), 3);
        assert!(evaluate(&policy, &[lan], &[], &LaunchHistory::default(), NOW).is_empty());
    }

    #[test]
    fn workers_are_not_shared_between_projects_and_identifiers_are_stable() {
        let policy = policy();
        let a = ProjectId::generate();
        let b = ProjectId::generate();
        let demand = vec![job(&a, 1, 2, 6 * MINUTE), job(&b, 1, 2, 6 * MINUTE)];
        let requests = evaluate(&policy, &demand, &[], &LaunchHistory::default(), NOW);
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0].project, requests[1].project);
        assert_ne!(requests[0].intent_id, requests[1].intent_id);

        // The order of the queue does not change the proposals.
        let reversed: Vec<Demand> = demand.iter().rev().cloned().collect();
        assert_eq!(
            evaluate(&policy, &reversed, &[], &LaunchHistory::default(), NOW),
            requests
        );

        // After that worker came and went, the same job gets a new identifier.
        let history = LaunchHistory {
            launches: vec![LaunchRecord {
                provider: "sim".to_string(),
                platform: LINUX,
                preset: "linux-small".to_string(),
                at: UnixMillis(NOW.0 + 1),
                outcome: LaunchOutcome::Launched,
            }],
        };
        let again = evaluate(&policy, &demand, &[], &history, UnixMillis(NOW.0 + 2));
        assert_eq!(again.len(), 2);
        assert!(
            again
                .iter()
                .all(|r| !requests.iter().any(|old| old.intent_id == r.intent_id))
        );
    }

    #[test]
    fn hysteresis_neither_requests_nor_releases_in_the_band() {
        let project = ProjectId::generate();
        let policy = policy();
        let worker = |idle_ms: u64| PendingOrActive {
            intent: CloudIntentId::generate(),
            provider: "sim".to_string(),
            project: project.clone(),
            platform: LINUX,
            phase: CapacityPhase::Active,
            free: vector(4, 8),
            covers: Vec::new(),
            since: UnixMillis(NOW.0 - 60 * MINUTE),
            idle_since: Some(UnixMillis(NOW.0 - idle_ms)),
        };
        let none = LaunchHistory::default();

        // A wait between the release and the request thresholds: the idle
        // worker stays, and without it nothing would be requested.
        let band = vec![job(&project, 2, 4, 3 * MINUTE)];
        let idle = vec![worker(30 * MINUTE)];
        assert!(release_candidates(&policy, &band, &idle, NOW).is_empty());
        assert!(evaluate(&policy, &band, &[], &none, NOW).is_empty());

        // A quiet queue releases a worker only after the idle period.
        let quiet = vec![job(&project, 2, 4, 10_000)];
        assert!(release_candidates(&policy, &quiet, &[worker(4 * MINUTE)], NOW).is_empty());
        let long_idle = vec![worker(5 * MINUTE)];
        assert_eq!(
            release_candidates(&policy, &quiet, &long_idle, NOW),
            vec![long_idle[0].intent.clone()]
        );
        // A busy worker has no idle time and is never a candidate.
        let mut busy = worker(0);
        busy.idle_since = None;
        assert!(release_candidates(&policy, &[], &[busy], NOW).is_empty());

        // Right after a release, a long wait does not bring a worker back...
        let released = LaunchHistory {
            launches: vec![LaunchRecord {
                provider: "sim".to_string(),
                platform: LINUX,
                preset: "linux-small".to_string(),
                at: UnixMillis(NOW.0 - 30 * MINUTE),
                outcome: LaunchOutcome::Released {
                    at: UnixMillis(NOW.0 - MINUTE),
                },
            }],
        };
        let waiting = vec![job(&project, 2, 4, 6 * MINUTE)];
        assert!(evaluate(&policy, &waiting, &[], &released, NOW).is_empty());
        // ...a deadline at risk does...
        let mut urgent = waiting.clone();
        urgent[0].deadline = Some(NOW);
        assert_eq!(evaluate(&policy, &urgent, &[], &released, NOW).len(), 1);
        // ...and so does the same wait once the cooldown has passed.
        let later = UnixMillis(NOW.0 + 10 * MINUTE);
        assert_eq!(evaluate(&policy, &waiting, &[], &released, later).len(), 1);
    }

    #[test]
    fn a_configured_warm_pool_is_filled_and_then_left_alone() {
        let project = ProjectId::generate();
        let mut policy = policy();
        policy.warm_pool = 1;
        // Without a project to bill there is no warm pool.
        assert!(evaluate(&policy, &[], &[], &LaunchHistory::default(), NOW).is_empty());
        policy.warm_pool_project = Some(project.clone());
        let requests = evaluate(&policy, &[], &[], &LaunchHistory::default(), NOW);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].triggers, vec![Trigger::WarmPool]);
        assert_eq!(requests[0].job, None);
        let capacity = vec![as_pending(&requests[0], &policy, NOW)];
        let history = LaunchHistory {
            launches: vec![launched(&requests[0], NOW)],
        };
        assert!(evaluate(&policy, &[], &capacity, &history, NOW).is_empty());

        // The warm worker is kept even when idle for a long time.
        let mut warm = capacity[0].clone();
        warm.phase = CapacityPhase::Active;
        warm.idle_since = Some(UnixMillis(NOW.0 - 60 * MINUTE));
        assert!(release_candidates(&policy, &[], &[warm], NOW).is_empty());
    }

    /// A small deterministic generator, so the property test needs no
    /// dependency and every failure can be replayed from its seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    fn random_demand(rng: &mut Rng, projects: &[ProjectId], now: UnixMillis) -> Demand {
        let platform = [LINUX, LINUX, WINDOWS, MAC][rng.below(4) as usize];
        let cloud = [
            CloudMode::AutoOverflow,
            CloudMode::AutoOverflow,
            CloudMode::Hybrid,
            CloudMode::Manual,
            CloudMode::ExistingOnly,
        ][rng.below(5) as usize];
        Demand {
            job: JobId::generate(),
            project: projects[rng.below(projects.len() as u64) as usize].clone(),
            platform,
            envelope: ResourceEnvelope::fixed(vector(1 + rng.below(20), 1 + rng.below(80))),
            queued_ms: rng.below(20 * MINUTE),
            predicted_wait_ms: if rng.chance(80) {
                Some(rng.below(12 * MINUTE))
            } else {
                None
            },
            deadline: if rng.chance(20) {
                Some(UnixMillis(now.0 + rng.below(20 * MINUTE)))
            } else {
                None
            },
            cloud,
            source_lan_only: rng.chance(10),
        }
    }

    fn launches_in_window(
        history: &LaunchHistory,
        policy: &OverflowPolicy,
        now: UnixMillis,
    ) -> usize {
        history
            .launches
            .iter()
            .filter(|l| l.provider == policy.provider && within(l.at, now, policy.launch_window_ms))
            .count()
    }

    /// For random demand streams the proposals never exceed the configured
    /// limits, never serve work that may not use the cloud, never touch a
    /// cooling preset, and evaluating again asks for nothing new.
    #[test]
    fn property_requests_stay_within_limits_and_repeat_evaluation_is_quiet() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let projects = vec![ProjectId::generate(), ProjectId::generate()];
            let mut policy = policy();
            policy.max_pending = 1 + rng.below(3) as u32;
            policy.max_active = policy.max_pending + rng.below(3) as u32;
            policy.max_launches = 1 + rng.below(4) as u32;
            policy.launch_window_ms = MINUTE * (1 + rng.below(10));
            policy.bootstrap_failure_cooldown_ms = MINUTE * rng.below(20);
            policy.reacquire_cooldown_ms = MINUTE * rng.below(10);
            if rng.chance(20) {
                policy.warm_pool = 1 + rng.below(2) as u32;
                policy.warm_pool_project = Some(projects[0].clone());
            }

            let mut now = NOW;
            let mut demand: Vec<Demand> = Vec::new();
            let mut capacity: Vec<PendingOrActive> = Vec::new();
            let mut history = LaunchHistory::default();

            for step in 0..60 {
                now = UnixMillis(now.0 + rng.below(3 * MINUTE));
                for _ in 0..rng.below(5) {
                    demand.push(random_demand(&mut rng, &projects, now));
                }
                demand.retain(|_| !rng.chance(15));
                // Workers progress, fail to bootstrap, or are released.
                let mut index = 0;
                while index < capacity.len() {
                    let roll = rng.below(100);
                    let entry = &mut capacity[index];
                    if entry.phase == CapacityPhase::Pending && roll < 25 {
                        entry.phase = CapacityPhase::Active;
                        entry.idle_since = Some(now);
                    } else if entry.phase == CapacityPhase::Pending && roll < 35 {
                        let gone = capacity.remove(index);
                        if let Some(record) = history.launches.iter_mut().rev().find(|l| {
                            l.platform == gone.platform && l.outcome == LaunchOutcome::Launched
                        }) {
                            record.outcome = LaunchOutcome::BootstrapFailed { at: now };
                        }
                        continue;
                    } else if entry.phase == CapacityPhase::Active && roll < 15 {
                        let gone = capacity.remove(index);
                        if let Some(record) = history.launches.iter_mut().rev().find(|l| {
                            l.platform == gone.platform && l.outcome == LaunchOutcome::Launched
                        }) {
                            record.outcome = LaunchOutcome::Released { at: now };
                        }
                        continue;
                    }
                    index += 1;
                }

                let context = format!("seed {seed} step {step}");
                let requests = evaluate(&policy, &demand, &capacity, &history, now);
                assert_eq!(
                    evaluate(&policy, &demand, &capacity, &history, now),
                    requests,
                    "deterministic: {context}"
                );

                let pending_before = capacity
                    .iter()
                    .filter(|c| c.phase == CapacityPhase::Pending)
                    .count();
                assert!(
                    pending_before + requests.len() <= policy.max_pending as usize
                        || requests.is_empty(),
                    "pending limit: {context}"
                );
                assert!(
                    capacity.len() + requests.len() <= policy.max_active as usize
                        || requests.is_empty(),
                    "total limit: {context}"
                );
                assert!(
                    launches_in_window(&history, &policy, now) + requests.len()
                        <= policy.max_launches as usize
                        || requests.is_empty(),
                    "launch rate: {context}"
                );

                let mut ids = BTreeSet::new();
                let mut served = BTreeSet::new();
                for request in &requests {
                    assert!(
                        ids.insert(request.intent_id.clone()),
                        "unique id: {context}"
                    );
                    let preset = policy
                        .presets
                        .iter()
                        .find(|p| p.name == request.preset)
                        .unwrap();
                    assert!(
                        !cooling_down(&policy, &history, preset, now),
                        "cooling preset: {context}"
                    );
                    assert_eq!(preset.platform, request.platform);
                    let mut used = ResourceVector::default();
                    for job in &request.covers {
                        assert!(served.insert(job.clone()), "job served twice: {context}");
                        let queued = demand.iter().find(|d| &d.job == job).unwrap();
                        assert!(eligible(queued), "ineligible job served: {context}");
                        assert!(!queued.source_lan_only);
                        assert_eq!(queued.project, request.project);
                        assert_eq!(queued.platform, request.platform);
                        assert!(
                            !capacity.iter().any(|c| c.covers.contains(job)),
                            "already covered: {context}"
                        );
                        used = used.checked_add(&queued.envelope.peak).unwrap();
                    }
                    assert!(used.fits_within(&preset.resources), "overpacked: {context}");
                    if request.covers.is_empty() {
                        assert_eq!(request.triggers, vec![Trigger::WarmPool]);
                    }
                }

                // Record the proposals, as the controller would.
                for request in &requests {
                    capacity.push(as_pending(request, &policy, now));
                    history.launches.push(launched(request, now));
                }
                assert!(
                    capacity
                        .iter()
                        .filter(|c| c.phase == CapacityPhase::Pending)
                        .count()
                        <= policy.max_pending as usize
                        || requests.is_empty()
                );
                assert!(
                    evaluate(&policy, &demand, &capacity, &history, now).is_empty(),
                    "repeat evaluation asked for more: {context}"
                );
            }
        }
    }
}
