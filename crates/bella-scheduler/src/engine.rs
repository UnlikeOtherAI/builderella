//! One scheduling pass: order the queue, place what fits, explain the rest.
//!
//! The algorithm is described in the crate documentation. Everything here
//! iterates in a fixed order (machines by id, jobs by queue time then id,
//! projects and sessions by id), so equal inputs give equal outputs however
//! the caller ordered its vectors.

use crate::eligibility::{Fleet, MachineLoad, held_back_blocker, label, load_blocker};
use crate::model::{FleetState, PlannedOffer, ProjectState, QueuedJob, Schedule, SchedulerConfig};
use crate::share::cmp_weighted;
use crate::units;
use bella_core::job::{MachineBlocker, Priority, QueueReason, QueueReasonCode as Code};
use bella_core::machine::MachineProfile;
use bella_core::platform::PlacementChoice;
use bella_core::resources::ResourceVector;
use bella_core::{JobId, ProjectId, SessionId, UnixMillis};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const BANDS: usize = 5;
const INTERACTIVE: usize = 3;
const URGENT: usize = 4;
const BAND_ORDER: [Priority; BANDS] = [
    Priority::Maintenance,
    Priority::Background,
    Priority::Normal,
    Priority::Interactive,
    Priority::Urgent,
];

pub(crate) const RECONCILING: &str = "the controller is reconciling with its workers after a \
    restart and offers nothing until that is done";

fn band_index(priority: Priority) -> usize {
    match priority {
        Priority::Maintenance => 0,
        Priority::Background => 1,
        Priority::Normal => 2,
        Priority::Interactive => 3,
        Priority::Urgent => 4,
    }
}

/// The band a job is served in.
///
/// `Urgent` needs the project's `allow_urgent`; without it the job is
/// served as `Interactive`. A job that has waited `aging_after_ms` moves up
/// exactly one band, never past the configured ceiling and never into
/// `Urgent`.
fn effective_band(
    config: &SchedulerConfig,
    job: &QueuedJob,
    project: Option<&ProjectState>,
    now: UnixMillis,
) -> usize {
    let mut band = band_index(job.spec.priority);
    if band == URGENT && !project.is_some_and(|p| p.policy.allow_urgent) {
        band = INTERACTIVE;
    }
    let ceiling = band_index(config.aging_ceiling).min(INTERACTIVE);
    let waited = now.0.saturating_sub(job.queued_at.0);
    if waited >= config.aging_after_ms && band < ceiling {
        band += 1;
    }
    band
}

type SessionKey<'a> = Option<&'a SessionId>;
type BandLanes<'a> = BTreeMap<&'a ProjectId, BTreeMap<SessionKey<'a>, VecDeque<usize>>>;

/// Waiting jobs grouped by band, project and session, each lane oldest
/// first. No empty lane or project is ever stored.
#[derive(Default)]
struct Lanes<'a> {
    bands: [BandLanes<'a>; BANDS],
}

struct ProjectLoad<'a> {
    state: &'a ProjectState,
    reserved: ResourceVector,
    held_attempts: u32,
    environments: u32,
}

/// A machine kept free for one job that has waited past the threshold.
struct Hold {
    job: JobId,
    waited_ms: u64,
}

struct Pass<'a> {
    fleet: &'a Fleet<'a>,
    /// Queue time, then id.
    jobs: Vec<&'a QueuedJob>,
    /// `standing[job][machine]`, machines in fleet order.
    standing: Vec<Vec<Option<MachineBlocker>>>,
    loads: Vec<MachineLoad>,
    projects: BTreeMap<&'a ProjectId, ProjectLoad<'a>>,
    band_reserved: [ResourceVector; BANDS],
    band_attempts: [u64; BANDS],
    session_attempts: BTreeMap<(&'a ProjectId, SessionKey<'a>), u64>,
    holds: BTreeMap<usize, Hold>,
    /// For a job only one machine could ever run: that machine.
    sole_machine: Vec<Option<usize>>,
    /// Per machine: jobs still waiting that could run nowhere else.
    waiting_only_here: Vec<u32>,
    out: Schedule,
}

pub(crate) fn schedule(state: &FleetState) -> Schedule {
    let mut jobs: Vec<&QueuedJob> = state.queue.iter().collect();
    jobs.sort_by(|a, b| (a.queued_at, &a.id).cmp(&(b.queued_at, &b.id)));
    let mut seen = BTreeSet::new();
    jobs.retain(|job| seen.insert(&job.id));

    if state.reconciling {
        let reasons = jobs
            .iter()
            .map(|job| {
                let reason = QueueReason::new(Code::ControllerReconciling, RECONCILING);
                (job.id.clone(), reason)
            })
            .collect();
        return Schedule {
            offers: Vec::new(),
            reasons,
        };
    }

    let fleet = Fleet::new(state);
    let mut pass = Pass::new(&fleet, jobs);
    let threshold = state.config.starvation_threshold_ms;
    let (starving, fresh): (Vec<usize>, Vec<usize>) =
        (0..pass.jobs.len()).partition(|&idx| pass.waited_ms(idx) >= threshold);
    pass.run(&starving, true);
    pass.run(&fresh, false);
    pass.out
}

impl<'a> Pass<'a> {
    fn new(fleet: &'a Fleet<'a>, jobs: Vec<&'a QueuedJob>) -> Self {
        let state = fleet.state;
        let standing: Vec<Vec<Option<MachineBlocker>>> = jobs
            .iter()
            .map(|job| {
                fleet
                    .machines
                    .iter()
                    .map(|machine| fleet.standing_blocker(job, machine))
                    .collect()
            })
            .collect();

        let mut waiting_only_here = vec![0u32; fleet.machines.len()];
        let sole_machine: Vec<Option<usize>> = standing
            .iter()
            .map(|row| {
                let mut possible = row
                    .iter()
                    .enumerate()
                    .filter(|(_, found)| !found.as_ref().is_some_and(|b| b.permanent))
                    .map(|(mi, _)| mi);
                match (possible.next(), possible.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            })
            .collect();
        for only in sole_machine.iter().flatten() {
            waiting_only_here[*only] += 1;
        }

        let projects = fleet
            .projects
            .iter()
            .map(|(id, project)| {
                let load = ProjectLoad {
                    state: project,
                    reserved: project.reserved.clone(),
                    held_attempts: project.held_attempts,
                    environments: project.environments,
                };
                (*id, load)
            })
            .collect();

        let mut band_reserved: [ResourceVector; BANDS] =
            std::array::from_fn(|_| ResourceVector::ZERO);
        let mut band_attempts = [0u64; BANDS];
        let mut session_attempts: BTreeMap<(&ProjectId, SessionKey<'_>), u64> = BTreeMap::new();
        for held in &state.held {
            let band = band_index(held.priority);
            band_reserved[band] = band_reserved[band].saturating_add(&held.reserved);
            band_attempts[band] += 1;
            *session_attempts
                .entry((&held.project, held.session.as_ref()))
                .or_insert(0) += 1;
        }

        Self {
            fleet,
            jobs,
            standing,
            loads: fleet
                .machines
                .iter()
                .copied()
                .map(MachineLoad::of)
                .collect(),
            projects,
            band_reserved,
            band_attempts,
            session_attempts,
            holds: BTreeMap::new(),
            sole_machine,
            waiting_only_here,
            out: Schedule::default(),
        }
    }

    fn waited_ms(&self, idx: usize) -> u64 {
        self.fleet
            .state
            .now
            .0
            .saturating_sub(self.jobs[idx].queued_at.0)
    }

    /// Consider every job in `indices` once, in service order.
    fn run(&mut self, indices: &[usize], starving: bool) {
        let state = self.fleet.state;
        let mut lanes = Lanes::default();
        for &idx in indices {
            let job = self.jobs[idx];
            let project = self.fleet.projects.get(&job.project).copied();
            let band = effective_band(&state.config, job, project, state.now);
            lanes.bands[band]
                .entry(&job.project)
                .or_default()
                .entry(job.session.as_ref())
                .or_default()
                .push_back(idx);
        }
        while let Some((idx, band)) = self.next(&mut lanes) {
            self.consider(idx, band, starving);
        }
    }

    /// Remove and return the job to consider next, with its band.
    fn next(&self, lanes: &mut Lanes<'a>) -> Option<(usize, usize)> {
        let band = self.pick_band(lanes)?;
        let (project, session) = self.pick_lane(&lanes.bands[band])?;
        let sessions = lanes.bands[band].get_mut(project)?;
        let lane = sessions.get_mut(&session)?;
        let idx = lane.pop_front()?;
        if lane.is_empty() {
            sessions.remove(&session);
        }
        if sessions.is_empty() {
            lanes.bands[band].remove(project);
        }
        Some((idx, band))
    }

    /// The band with work waiting that holds the least per unit of weight.
    /// Ties go to the higher band.
    fn pick_band(&self, lanes: &Lanes<'a>) -> Option<usize> {
        let weights = &self.fleet.state.config.band_weights;
        let mut best: Option<(usize, u64, u64, u64)> = None;
        for band in (0..BANDS).rev() {
            if lanes.bands[band].is_empty() {
                continue;
            }
            let weight = weights.weight(BAND_ORDER[band]);
            let share = self.fleet.totals.dominant_share(&self.band_reserved[band]);
            let attempts = self.band_attempts[band];
            let better = best.is_none_or(|(_, best_share, best_attempts, best_weight)| {
                cmp_weighted(share, weight, best_share, best_weight)
                    .then_with(|| cmp_weighted(attempts, weight, best_attempts, best_weight))
                    == Ordering::Less
            });
            if better {
                best = Some((band, share, attempts, weight));
            }
        }
        best.map(|(band, ..)| band)
    }

    /// Within a band: the project holding the least per unit of weight,
    /// then inside it the session holding the fewest attempts. Remaining
    /// ties go to the lane whose head has waited longest.
    fn pick_lane(&self, band: &BandLanes<'a>) -> Option<(&'a ProjectId, SessionKey<'a>)> {
        struct Choice<'a> {
            project: &'a ProjectId,
            session: SessionKey<'a>,
            share: u64,
            attempts: u64,
            weight: u64,
            head: (UnixMillis, &'a JobId),
        }
        let mut best: Option<Choice<'a>> = None;
        for (project, sessions) in band {
            let lane = sessions
                .iter()
                .filter_map(|(session, lane)| {
                    let head = self.jobs[*lane.front()?];
                    let held = self
                        .session_attempts
                        .get(&(*project, *session))
                        .copied()
                        .unwrap_or(0);
                    Some((held, (head.queued_at, &head.id), *session))
                })
                .min();
            let Some((_, head, session)) = lane else {
                continue;
            };
            let (share, attempts, weight) = match self.projects.get(project) {
                Some(load) => (
                    self.fleet.totals.dominant_share(&load.reserved),
                    u64::from(load.held_attempts),
                    u64::from(load.state.policy.weight.max(1)),
                ),
                None => (0, 0, 1),
            };
            let better = best.as_ref().is_none_or(|b| {
                cmp_weighted(share, weight, b.share, b.weight)
                    .then_with(|| cmp_weighted(attempts, weight, b.attempts, b.weight))
                    .then_with(|| head.cmp(&b.head))
                    == Ordering::Less
            });
            if better {
                best = Some(Choice {
                    project,
                    session,
                    share,
                    attempts,
                    weight,
                    head,
                });
            }
        }
        best.map(|b| (b.project, b.session))
    }

    /// Reasons that stop a job whatever machine is considered.
    fn gate(&self, job: &QueuedJob) -> Option<QueueReason> {
        job_gate(
            job,
            self.projects
                .get(&job.project)
                .map(|load| (load.state, load.held_attempts, load.environments)),
        )
    }

    fn consider(&mut self, idx: usize, band: usize, starving: bool) {
        let job = self.jobs[idx];
        if let Some(reason) = self.gate(job) {
            self.out.reasons.insert(job.id.clone(), reason);
            return;
        }

        let fleet = self.fleet;
        let mut blockers = Vec::with_capacity(fleet.machines.len());
        let mut open = Vec::new();
        let mut holdable = Vec::new();
        for (mi, machine) in fleet.machines.iter().enumerate() {
            if let Some(found) = &self.standing[idx][mi] {
                blockers.push(found.clone());
            } else if let Some(found) = load_blocker(job, machine, &self.loads[mi]) {
                if !self.holds.contains_key(&mi) {
                    holdable.push(mi);
                }
                blockers.push(found);
            } else if let Some(hold) = self.holds.get(&mi) {
                blockers.push(held_back_blocker(
                    machine,
                    format!(
                        "kept free for job {}, which has waited {} and still does not fit; \
                         nothing else is placed here until it does",
                        hold.job,
                        units::duration_ms(hold.waited_ms)
                    ),
                ));
            } else {
                open.push(mi);
            }
        }

        if let Some(mi) = self.best_machine(idx, job, &open) {
            self.offer(idx, band, mi);
            return;
        }

        let mut reason = summarize(blockers);
        if starving && let Some(mi) = self.machine_to_hold(job, &holdable) {
            let waited_ms = self.waited_ms(idx);
            reason.message.push_str(&format!(
                "; this job has waited {}, so {} takes no other work until it fits",
                units::duration_ms(waited_ms),
                label(fleet.machines[mi])
            ));
            self.holds.insert(
                mi,
                Hold {
                    job: job.id.clone(),
                    waited_ms,
                },
            );
        }
        self.out.reasons.insert(job.id.clone(), reason);
    }

    /// The best of the machines that can take the job now: one that
    /// satisfies a stated preference; then one that is not a protected
    /// workstation; then the one fewest other waiting jobs depend on; then
    /// the one left with most headroom; then the lowest id.
    fn best_machine(&self, idx: usize, job: &QueuedJob, open: &[usize]) -> Option<usize> {
        let fleet = self.fleet;
        let peak = &job.spec.envelope.peak;
        open.iter().copied().min_by_key(|&mi| {
            let machine = fleet.machines[mi];
            let misses_preference = match &job.spec.placement {
                PlacementChoice::Prefer { platform } => {
                    !platform.matches(machine.platform, &machine.os_version)
                }
                _ => false,
            };
            let protected = machine.policy.profile == MachineProfile::ProtectedWorkstation;
            let own = u32::from(self.sole_machine[idx] == Some(mi));
            let depended_on = self.waiting_only_here[mi].saturating_sub(own);
            let load = &self.loads[mi];
            let free_after = load
                .capacity
                .saturating_sub(&load.reserved)
                .saturating_sub(peak);
            let headroom = fleet.totals.headroom(&free_after);
            (
                misses_preference,
                protected,
                depended_on,
                Reverse(headroom),
                mi,
            )
        })
    }

    fn offer(&mut self, idx: usize, band: usize, mi: usize) {
        let job = self.jobs[idx];
        let machine = self.fleet.machines[mi];
        let peak = &job.spec.envelope.peak;

        let load = &mut self.loads[mi];
        let capacity = load.capacity.clone();
        load.reserved = load.reserved.saturating_add(peak);
        load.held_attempts = load.held_attempts.saturating_add(1);

        if let Some(project) = self.projects.get_mut(&job.project) {
            project.reserved = project.reserved.saturating_add(peak);
            project.held_attempts = project.held_attempts.saturating_add(1);
            if job.spec.environment.is_some() {
                project.environments = project.environments.saturating_add(1);
            }
        }
        self.band_reserved[band] = self.band_reserved[band].saturating_add(peak);
        self.band_attempts[band] += 1;
        *self
            .session_attempts
            .entry((&job.project, job.session.as_ref()))
            .or_insert(0) += 1;
        if let Some(only) = self.sole_machine[idx] {
            self.waiting_only_here[only] = self.waiting_only_here[only].saturating_sub(1);
        }

        self.out.offers.push(PlannedOffer {
            job_id: job.id.clone(),
            machine_id: machine.id.clone(),
            reservation: peak.clone(),
            capacity,
            max_concurrent: machine.policy.max_concurrent_attempts,
        });
    }

    /// Among machines the job is only waiting for room on, the one it would
    /// fit soonest: the smallest part of the request still missing, then
    /// fewest attempts to wait for, then the lowest id.
    ///
    /// A machine whose current budget is below the request is not a
    /// candidate: draining it would not let the job start, so it goes on
    /// serving work that does fit until the budget recovers.
    fn machine_to_hold(&self, job: &QueuedJob, holdable: &[usize]) -> Option<usize> {
        let peak = &job.spec.envelope.peak;
        holdable
            .iter()
            .copied()
            .filter(|&mi| peak.fits_within(&self.loads[mi].capacity))
            .min_by_key(|&mi| {
                let load = &self.loads[mi];
                let free = load.capacity.saturating_sub(&load.reserved);
                (missing_fraction(peak, &free), load.held_attempts, mi)
            })
    }
}

/// The largest part of the request, in parts per million of that dimension,
/// that `free` does not yet cover.
fn missing_fraction(peak: &ResourceVector, free: &ResourceVector) -> u64 {
    let part = |wanted: u64, available: u64| -> u64 {
        if wanted == 0 {
            return 0;
        }
        let missing = wanted.saturating_sub(available);
        u64::try_from(u128::from(missing) * 1_000_000 / u128::from(wanted)).unwrap_or(u64::MAX)
    };
    let mut worst = part(peak.cpu_milli, free.cpu_milli)
        .max(part(peak.memory_bytes, free.memory_bytes))
        .max(part(peak.disk_bytes, free.disk_bytes));
    for (name, wanted) in &peak.tokens {
        worst = worst.max(part(*wanted, free.token(name)));
    }
    worst
}

/// Job-level blockers. `project` is the job's project with its current
/// held attempts and environments, or `None` when the state has no such
/// project.
pub(crate) fn job_gate(
    job: &QueuedJob,
    project: Option<(&ProjectState, u32, u32)>,
) -> Option<QueueReason> {
    if job.has_unresolved_attempt {
        return Some(QueueReason::new(
            Code::PreviousAttemptUnresolved,
            "an earlier attempt of this job is not resolved yet; a job has at most one live \
             or unresolved attempt",
        ));
    }
    let Some((project, held_attempts, environments)) = project else {
        return Some(QueueReason::new(
            Code::ProjectNotAllowed,
            format!(
                "project {} is not known to the scheduler, so its policy cannot be checked",
                job.project
            ),
        ));
    };
    if let Some(max) = project.policy.max_concurrent_attempts
        && held_attempts >= max
    {
        return Some(QueueReason::new(
            Code::ProjectConcurrencyLimit,
            format!(
                "project `{}` runs {held_attempts} of at most {max} concurrent attempts",
                project.name
            ),
        ));
    }
    if job.spec.environment.is_some()
        && let Some(max) = project.policy.max_environments
        && environments >= max
    {
        return Some(QueueReason::new(
            Code::EnvironmentQuota,
            format!(
                "project `{}` keeps {environments} of at most {max} environments, and this \
                 job would start another",
                project.name
            ),
        ));
    }
    None
}

/// How close a temporary blocker is to clearing by itself; lower is closer.
fn rank(code: Code) -> u8 {
    match code {
        Code::ReservedForOlderJob => 0,
        Code::WaitingForCapacity => 1,
        Code::MachineConcurrencyLimit => 2,
        Code::MissingCapability => 3,
        Code::OfferRejected => 4,
        Code::ForegroundProtection => 5,
        Code::Overheated => 6,
        Code::MemoryPressure => 7,
        Code::DiskLow => 8,
        Code::OnBattery => 9,
        Code::MachinePaused => 10,
        Code::MachineUpgrading => 11,
        Code::MachineDraining => 12,
        Code::CleanupPending => 13,
        Code::Quarantined => 14,
        Code::StaleTelemetry => 15,
        Code::MachineUnreachable => 16,
        _ => 17,
    }
}

/// Turn one blocker per machine into the job's reason.
///
/// When no machine could ever run the job the reason is
/// `NoCompatibleMachine`. Otherwise it is the temporary blocker closest to
/// clearing, taken from the first machine (in id order) that has it.
fn summarize(blockers: Vec<MachineBlocker>) -> QueueReason {
    const LISTED: usize = 3;
    let temporary: Vec<&MachineBlocker> = blockers.iter().filter(|b| !b.permanent).collect();
    let (code, message) = match temporary.iter().min_by_key(|b| rank(b.code)) {
        Some(first) => {
            let mut message = format!("{}: {}", first.machine, first.message);
            let others = temporary.len() - 1;
            if others == 1 {
                message.push_str("; 1 other compatible machine cannot take it right now either");
            } else if others > 1 {
                message.push_str(&format!(
                    "; {others} other compatible machines cannot take it right now either"
                ));
            }
            (first.code, message)
        }
        None if blockers.is_empty() => (
            Code::NoCompatibleMachine,
            "no machines are enrolled in this fleet".to_string(),
        ),
        None => {
            let mut listed: Vec<String> = blockers
                .iter()
                .take(LISTED)
                .map(|b| format!("{}: {}", b.machine, b.message))
                .collect();
            if blockers.len() > LISTED {
                listed.push(format!("and {} more", blockers.len() - LISTED));
            }
            let count = blockers.len();
            let noun = if count == 1 { "machine" } else { "machines" };
            (
                Code::NoCompatibleMachine,
                format!(
                    "none of the {count} enrolled {noun} can run this job as specified: {}",
                    listed.join("; ")
                ),
            )
        }
    };
    QueueReason {
        code,
        message,
        blockers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocker(machine: &str, code: Code, permanent: bool) -> MachineBlocker {
        MachineBlocker {
            machine: machine.to_string(),
            code,
            message: format!("{code:?}"),
            permanent,
        }
    }

    #[test]
    fn an_empty_fleet_has_no_compatible_machine() {
        let reason = summarize(Vec::new());
        assert_eq!(reason.code, Code::NoCompatibleMachine);
        assert!(reason.message.contains("no machines are enrolled"));
    }

    #[test]
    fn only_permanent_blockers_mean_no_compatible_machine() {
        let reason = summarize(vec![
            blocker("a", Code::WrongPlatform, true),
            blocker("b", Code::MachineTooSmall, true),
        ]);
        assert_eq!(reason.code, Code::NoCompatibleMachine);
        assert!(reason.message.contains("none of the 2 enrolled machines"));
        assert!(reason.message.contains("a: WrongPlatform"));
        assert!(reason.message.contains("b: MachineTooSmall"));
        assert_eq!(reason.blockers.len(), 2);
    }

    #[test]
    fn long_lists_are_shortened_in_the_message_but_not_in_the_blockers() {
        let blockers: Vec<MachineBlocker> = (0..5)
            .map(|i| blocker(&format!("m{i}"), Code::WrongPlatform, true))
            .collect();
        let reason = summarize(blockers);
        assert!(reason.message.contains("and 2 more"));
        assert!(!reason.message.contains("m3:"));
        assert_eq!(reason.blockers.len(), 5);
    }

    #[test]
    fn the_temporary_blocker_closest_to_clearing_wins() {
        let reason = summarize(vec![
            blocker("a", Code::WrongPlatform, true),
            blocker("b", Code::MachineUnreachable, false),
            blocker("c", Code::WaitingForCapacity, false),
            blocker("d", Code::MachinePaused, false),
        ]);
        assert_eq!(reason.code, Code::WaitingForCapacity);
        assert!(reason.message.starts_with("c: WaitingForCapacity"));
        assert!(reason.message.contains("2 other compatible machines"));
        assert_eq!(reason.blockers.len(), 4);
    }

    #[test]
    fn missing_fraction_reports_the_worst_dimension() {
        let peak = ResourceVector::new(4000, 100, 0).with_token("gpu", 2);
        assert_eq!(
            missing_fraction(
                &peak,
                &ResourceVector::new(4000, 100, 0).with_token("gpu", 2)
            ),
            0
        );
        assert_eq!(
            missing_fraction(
                &peak,
                &ResourceVector::new(4000, 75, 0).with_token("gpu", 2)
            ),
            250_000
        );
        assert_eq!(
            missing_fraction(&peak, &ResourceVector::new(4000, 75, 0)),
            1_000_000
        );
    }
}
