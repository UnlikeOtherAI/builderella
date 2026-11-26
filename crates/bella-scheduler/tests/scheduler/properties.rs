//! Invariants over generated fleets, and determinism.

use crate::support::*;
use bella_core::job::{LeaseSpec, Priority, QueueReasonCode as Code};
use bella_core::machine::{AvailabilityFlag as Flag, MachinePolicy, MachineProfile};
use bella_core::platform::{CapabilityRequirement, Os, PlacementChoice, PlatformSelector};
use bella_core::resources::ResourceVector;
use bella_core::{MachineId, ProjectId, UnixMillis};
use bella_scheduler::{
    FleetState, HeldAttempt, MachineState, ProjectState, QueuedJob, Schedule, effective_capacity,
    evaluate, schedule,
};
use std::collections::{BTreeMap, BTreeSet};

const SEEDS: u64 = 400;

fn pick<T: Clone>(rng: &mut Rng, items: &[T]) -> T {
    items[rng.below(items.len() as u64) as usize].clone()
}

fn random_project(rng: &mut Rng, index: u64) -> ProjectState {
    let mut p = project(&format!("project-{index}"));
    p.policy.weight = 50 + rng.below(300) as u32;
    if rng.chance(3) {
        p.policy.max_concurrent_attempts = Some(1 + rng.below(4) as u32);
    }
    p.policy.max_environments = Some(1 + rng.below(3) as u32);
    p.policy.source_lan_only = rng.chance(6);
    p.policy.allow_urgent = rng.chance(2);
    p.held_attempts = rng.below(3) as u32;
    p.reserved = vector(rng.below(8), rng.below(16));
    p.environments = rng.below(3) as u32;
    p
}

fn random_machine(rng: &mut Rng, index: u64, projects: &[ProjectState]) -> MachineState {
    let cpus = 2 + rng.below(31);
    let memory = 4 + rng.below(125);
    let mut m = machine(&format!("machine-{index}"), cpus, memory);
    m.platform = pick(rng, &[LINUX, LINUX, MAC, WINDOWS]);
    m.policy = MachinePolicy::defaults_for(pick(
        rng,
        &[
            MachineProfile::DedicatedBuilder,
            MachineProfile::DedicatedBuilder,
            MachineProfile::ProtectedWorkstation,
            MachineProfile::PortableBuilder,
            MachineProfile::EphemeralCloud,
            MachineProfile::InfrastructureOnly,
        ],
    ));
    if rng.chance(3) {
        m.policy.max_concurrent_attempts = Some(1 + rng.below(4) as u32);
    }
    if rng.chance(3) {
        m.policy.pools = vec!["release".to_string()];
    }
    if rng.chance(8) {
        m.policy.allowed_projects = Some(vec![pick(rng, projects).id]);
    }
    if rng.chance(3) {
        m.managed_ceiling
            .tokens
            .insert("gpu".to_string(), 1 + rng.below(2));
    }
    m.effective_budget = match rng.below(10) {
        0 => None,
        1..=5 => Some(m.managed_ceiling.clone()),
        _ => {
            let mut budget = m.managed_ceiling.clone();
            budget.cpu_milli = rng.below(m.managed_ceiling.cpu_milli + 4000);
            budget.memory_bytes = rng.below(m.managed_ceiling.memory_bytes + 8 * GIB);
            if rng.chance(2) {
                budget.tokens.clear();
            }
            Some(budget)
        }
    };
    // Sometimes more is reserved than the machine can currently grant.
    m.reserved = vector(rng.below(cpus + 2), rng.below(memory + 4));
    if rng.chance(4) && m.managed_ceiling.token("gpu") > 0 {
        m.reserved.tokens.insert("gpu".to_string(), 1);
    }
    m.held_attempts = rng.below(4) as u32;
    m.connected = !rng.chance(10);
    m.admitting = !rng.chance(10);
    m.reconciled = !rng.chance(15);
    m.on_lan = !rng.chance(6);
    m.telemetry_age_ms = if rng.chance(15) {
        None
    } else {
        Some(rng.below(18_000))
    };
    if rng.chance(8) {
        m.flags = vec![pick(
            rng,
            &[
                Flag::Draining,
                Flag::UserPaused,
                Flag::PressureLimited,
                Flag::OnBattery,
                Flag::Overheated,
                Flag::DiskLow,
                Flag::Sleeping,
                Flag::Stale,
                Flag::Unreachable,
                Flag::Quarantined,
                Flag::Upgrading,
                Flag::BuildsDisabled,
            ],
        )];
    }
    if m.platform == MAC {
        m.capabilities
            .push(capability("xcode", pick(rng, &["14.2", "16.2"])));
        m.case_sensitive_fs = false;
    }
    if m.platform == WINDOWS {
        m.case_sensitive_fs = false;
        m.supports_symlinks = rng.chance(2);
    }
    if rng.chance(2) {
        m.capabilities.push(capability("gui-session", "1"));
    }
    m
}

fn random_job(
    rng: &mut Rng,
    index: u64,
    projects: &[ProjectState],
    machines: &[MachineState],
) -> QueuedJob {
    let mut s = spec(1 + rng.below(8), 1 + rng.below(24));
    if rng.chance(8) {
        s.envelope.peak.tokens.insert("gpu".to_string(), 1);
    }
    s.compatibility.build_hosts = match rng.below(4) {
        0 => Vec::new(),
        1 => vec![PlatformSelector::os(pick(
            rng,
            &[Os::Linux, Os::Macos, Os::Windows],
        ))],
        _ => vec![PlatformSelector::default()],
    };
    if rng.chance(8) {
        s.compatibility.test_runtime = Some(PlatformSelector::os(pick(
            rng,
            &[Os::Linux, Os::Macos, Os::Windows],
        )));
    }
    if rng.chance(8) {
        s.compatibility.requires.push(CapabilityRequirement {
            name: "xcode".to_string(),
            version: Some(">=16".to_string()),
            attributes: BTreeMap::new(),
        });
    }
    if rng.chance(8) {
        s.compatibility
            .requires
            .push(CapabilityRequirement::named("gui-session"));
    }
    s.placement = match rng.below(10) {
        0 => PlacementChoice::Prefer {
            platform: PlatformSelector::os(Os::Macos),
        },
        1 => PlacementChoice::Require {
            platform: PlatformSelector::os(Os::Linux),
        },
        2 if !machines.is_empty() => PlacementChoice::Machine {
            machine: pick(rng, machines).name,
        },
        3 => PlacementChoice::Pool {
            pool: "release".to_string(),
        },
        _ => PlacementChoice::Any,
    };
    s.priority = pick(
        rng,
        &[
            Priority::Maintenance,
            Priority::Background,
            Priority::Normal,
            Priority::Normal,
            Priority::Interactive,
            Priority::Urgent,
        ],
    );
    if rng.chance(6) {
        s.environment = Some(LeaseSpec::default());
    }
    s.source_lan_only = rng.chance(10);

    let owner = if rng.chance(25) {
        project("not-in-the-state")
    } else {
        pick(rng, projects)
    };
    let mut queued = job(&format!("job-{index}"), &owner, rng.below(20 * 60_000), s);
    queued.origin_platform = pick(rng, &[LINUX, LINUX, MAC, WINDOWS]);
    if rng.chance(2) {
        queued.session = Some(session(&format!("session-{}", rng.below(3))));
    }
    queued.has_unresolved_attempt = rng.chance(15);
    queued.snapshot_case_sensitive_only = rng.chance(10);
    queued.snapshot_has_symlinks = rng.chance(10);
    if rng.chance(10) {
        queued.snapshot_windows_incompatible = Some("a path ends with a dot".to_string());
    }
    if rng.chance(8) && !machines.is_empty() {
        let until = NOW - 10_000 + rng.below(20_000);
        queued.recently_rejected_by = vec![(pick(rng, machines).id, UnixMillis(until))];
    }
    queued
}

fn random_fleet(seed: u64) -> FleetState {
    let mut rng = Rng::new(seed);
    let projects: Vec<ProjectState> = (0..1 + rng.below(4))
        .map(|i| random_project(&mut rng, i))
        .collect();
    let machines: Vec<MachineState> = (0..rng.below(7))
        .map(|i| random_machine(&mut rng, i, &projects))
        .collect();
    let queue: Vec<QueuedJob> = (0..rng.below(40))
        .map(|i| random_job(&mut rng, i, &projects, &machines))
        .collect();
    let held: Vec<HeldAttempt> = (0..rng.below(10))
        .map(|_| HeldAttempt {
            project: pick(&mut rng, &projects).id,
            session: rng
                .chance(2)
                .then(|| session(&format!("session-{}", rng.below(3)))),
            priority: pick(
                &mut rng,
                &[
                    Priority::Background,
                    Priority::Normal,
                    Priority::Interactive,
                ],
            ),
            reserved: vector(1 + rng.below(4), 1 + rng.below(8)),
        })
        .collect();
    let mut state = fleet(machines, projects, queue);
    state.held = held;
    state.reconciling = rng.chance(40);
    state
}

fn shuffled(state: &FleetState, seed: u64) -> FleetState {
    let mut rng = Rng::new(seed ^ 0x5EED);
    let mut copy = state.clone();
    rng.shuffle(&mut copy.machines);
    rng.shuffle(&mut copy.queue);
    rng.shuffle(&mut copy.projects);
    rng.shuffle(&mut copy.held);
    copy
}

/// Check every guarantee `schedule` documents against one state and plan.
fn check(seed: u64, state: &FleetState, plan: &Schedule) {
    let ctx = format!("seed {seed}");
    let jobs: BTreeMap<_, &QueuedJob> = state.queue.iter().map(|j| (j.id.clone(), j)).collect();
    let machines: BTreeMap<MachineId, &MachineState> =
        state.machines.iter().map(|m| (m.id.clone(), m)).collect();
    let projects: BTreeMap<ProjectId, &ProjectState> =
        state.projects.iter().map(|p| (p.id.clone(), p)).collect();

    // Every queued job gets exactly one of: an offer, a reason.
    let offered: BTreeSet<_> = plan.offers.iter().map(|o| o.job_id.clone()).collect();
    assert_eq!(
        offered.len(),
        plan.offers.len(),
        "{ctx}: a job was offered twice"
    );
    for id in jobs.keys() {
        assert!(
            offered.contains(id) != plan.reasons.contains_key(id),
            "{ctx}: {id} needs exactly one of an offer and a reason"
        );
    }
    assert_eq!(offered.len() + plan.reasons.len(), jobs.len(), "{ctx}");

    if state.reconciling {
        assert!(plan.offers.is_empty(), "{ctx}: offers while reconciling");
    }

    let mut on_machine: BTreeMap<MachineId, (ResourceVector, u32)> = BTreeMap::new();
    let mut on_project: BTreeMap<ProjectId, (u32, u32)> = BTreeMap::new();
    for offer in &plan.offers {
        let queued = jobs[&offer.job_id];
        let machine = machines[&offer.machine_id];
        let project = projects
            .get(&queued.project)
            .unwrap_or_else(|| panic!("{ctx}: offered a job of an unknown project"));

        assert_eq!(offer.reservation, queued.spec.envelope.peak, "{ctx}");
        assert_eq!(
            offer.max_concurrent, machine.policy.max_concurrent_attempts,
            "{ctx}"
        );
        assert!(
            !queued.has_unresolved_attempt,
            "{ctx}: offered with an unresolved attempt"
        );

        // The machine was fit to take the job before this call began.
        let verdict = evaluate(state, queued)
            .into_iter()
            .find(|(id, _)| *id == machine.id)
            .map(|(_, verdict)| verdict);
        assert_eq!(verdict, Some(Ok(())), "{ctx}: offered to a blocked machine");
        assert!(
            machine.connected && machine.admitting && machine.reconciled,
            "{ctx}"
        );
        assert!(
            machine.flags.is_empty(),
            "{ctx}: every flag stops new offers"
        );
        assert!(
            machine.telemetry_age_ms.is_some_and(|age| age <= 15_000),
            "{ctx}"
        );
        assert!(machine.policy.accept_builds, "{ctx}");
        if queued.spec.source_lan_only || project.policy.source_lan_only {
            assert!(machine.on_lan, "{ctx}: LAN-only source left the LAN");
        }

        let entry = on_machine
            .entry(machine.id.clone())
            .or_insert_with(|| (machine.reserved.clone(), machine.held_attempts));
        entry.0 = entry
            .0
            .checked_add(&offer.reservation)
            .expect("no overflow");
        entry.1 += 1;
        let entry = on_project
            .entry(project.id.clone())
            .or_insert((project.held_attempts, project.environments));
        entry.0 += 1;
        if queued.spec.environment.is_some() {
            entry.1 += 1;
        }
    }

    for (id, (total, attempts)) in &on_machine {
        let machine = machines[id];
        let budget = machine
            .effective_budget
            .as_ref()
            .expect("offers need a budget");
        let ceiling = &machine.managed_ceiling;
        // min(ceiling, budget), computed here without the crate's help.
        assert!(
            total.cpu_milli <= ceiling.cpu_milli.min(budget.cpu_milli),
            "{ctx}: cpu"
        );
        assert!(
            total.memory_bytes <= ceiling.memory_bytes.min(budget.memory_bytes),
            "{ctx}: memory"
        );
        assert!(
            total.disk_bytes <= ceiling.disk_bytes.min(budget.disk_bytes),
            "{ctx}: disk"
        );
        for (token, count) in &total.tokens {
            let granted = budget.tokens.get(token).copied().unwrap_or(u64::MAX);
            assert!(
                *count <= ceiling.token(token).min(granted),
                "{ctx}: token {token}"
            );
        }
        let capacity = effective_capacity(ceiling, Some(budget));
        assert!(total.fits_within(&capacity), "{ctx}: over capacity on {id}");
        for offer in plan.offers.iter().filter(|o| o.machine_id == *id) {
            assert_eq!(
                offer.capacity, capacity,
                "{ctx}: the offer names the checked capacity"
            );
        }
        if let Some(max) = machine.policy.max_concurrent_attempts {
            assert!(
                *attempts <= max,
                "{ctx}: {attempts} attempts on {id}, limit {max}"
            );
        }
    }
    for (id, (attempts, environments)) in &on_project {
        let policy = &projects[id].policy;
        if let Some(max) = policy.max_concurrent_attempts {
            assert!(
                *attempts <= max,
                "{ctx}: project attempts {attempts} over {max}"
            );
        }
        if let Some(max) = policy.max_environments
            && *environments > projects[id].environments
        {
            assert!(
                *environments <= max,
                "{ctx}: environments {environments} over {max}"
            );
        }
    }

    for (id, reason) in &plan.reasons {
        assert!(
            !reason.message.is_empty(),
            "{ctx}: {id} has an empty reason"
        );
        assert_ne!(reason.code, Code::Schedulable, "{ctx}");
        match reason.code {
            Code::NoCompatibleMachine => {
                assert!(
                    reason.blockers.iter().all(|b| b.permanent),
                    "{ctx}: {reason:?}"
                );
                assert_eq!(reason.blockers.len(), machines.len(), "{ctx}");
            }
            Code::ControllerReconciling
            | Code::PreviousAttemptUnresolved
            | Code::ProjectNotAllowed
            | Code::ProjectConcurrencyLimit => {
                assert!(reason.blockers.is_empty(), "{ctx}: {reason:?}");
            }
            code => {
                assert_eq!(reason.blockers.len(), machines.len(), "{ctx}");
                assert!(
                    reason
                        .blockers
                        .iter()
                        .any(|b| !b.permanent && b.code == code),
                    "{ctx}: the reason is one of the machines' temporary blockers: {reason:?}"
                );
            }
        }
    }
}

#[test]
fn generated_fleets_never_overcommit_and_account_for_every_job() {
    let (mut offers, mut reasons, mut reason_codes) = (0, 0, BTreeSet::new());
    for seed in 0..SEEDS {
        let state = random_fleet(seed);
        let plan = schedule(&state);
        check(seed, &state, &plan);
        offers += plan.offers.len();
        reasons += plan.reasons.len();
        reason_codes.extend(plan.reasons.values().map(|r| format!("{:?}", r.code)));
    }
    // The generator has to exercise both outcomes and a wide set of reasons,
    // or the checks above prove little.
    assert!(offers > 400, "only {offers} offers in {SEEDS} fleets");
    assert!(reasons > 2000, "only {reasons} reasons in {SEEDS} fleets");
    assert!(
        reason_codes.len() >= 12,
        "only these reasons occurred: {reason_codes:?}"
    );
}

#[test]
fn the_same_state_always_gives_the_same_schedule() {
    for seed in 0..SEEDS {
        let state = random_fleet(seed);
        assert_eq!(schedule(&state), schedule(&state), "seed {seed}");
        assert_eq!(schedule(&state), schedule(&state.clone()), "seed {seed}");
    }
}

#[test]
fn the_order_of_the_input_vectors_does_not_matter() {
    for seed in 0..SEEDS {
        let state = random_fleet(seed);
        let expected = schedule(&state);
        for round in 0..3 {
            let reordered = shuffled(&state, seed * 7 + round);
            assert_eq!(
                schedule(&reordered),
                expected,
                "seed {seed}, shuffle {round}"
            );
        }
    }
}

/// The invariants also hold tick after tick as reservations come and go.
#[test]
fn a_running_fleet_never_overcommits_across_ticks() {
    for seed in 0..40 {
        let mut state = random_fleet(seed);
        state.reconciling = false;
        // Existing reservations belong to nobody the simulator will release,
        // so start from an idle fleet and let the queue fill it.
        for machine in &mut state.machines {
            machine.reserved = ResourceVector::ZERO;
            machine.held_attempts = 0;
        }
        for project in &mut state.projects {
            project.reserved = ResourceVector::ZERO;
            project.held_attempts = 0;
            project.environments = 0;
        }
        state.held.clear();

        let mut sim = Sim::new(state, 1_000);
        let mut rng = Rng::new(seed);
        for _ in 0..25 {
            let before = sim.view();
            let plan = sim.step(|_| 1 + rng.below(4));
            check(seed, &before, &plan);
        }
    }
}
