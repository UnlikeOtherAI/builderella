//! Rule 3: bands, project fair share, sessions, aging.

use crate::support::*;
use bella_core::job::Priority;
use bella_core::{JobId, ProjectId};
use bella_scheduler::{HeldAttempt, QueuedJob, SchedulerConfig, schedule};
use std::collections::BTreeMap;

fn with_priority(mut queued: QueuedJob, priority: Priority) -> QueuedJob {
    let mut spec = (*queued.spec).clone();
    spec.priority = priority;
    queued.spec = std::sync::Arc::new(spec);
    queued
}

fn in_session(mut queued: QueuedJob, name: &str) -> QueuedJob {
    queued.session = Some(session(name));
    queued
}

fn offered_projects(plan: &bella_scheduler::Schedule, queue: &[QueuedJob]) -> Vec<ProjectId> {
    plan.offers
        .iter()
        .map(|offer| {
            queue
                .iter()
                .find(|queued| queued.id == offer.job_id)
                .expect("offers name queued jobs")
                .project
                .clone()
        })
        .collect()
}

/// One project queues 100 jobs, another 2. The small project's jobs are
/// among the first scheduled, and the big one does not take every slot.
#[test]
fn a09_a_small_project_is_served_among_the_first() {
    let big = project("big");
    let small = project("small");
    let mut queue: Vec<QueuedJob> = (0..100)
        .map(|i| job(&format!("big-{i}"), &big, 60_000 - i, spec(1, 4)))
        .collect();
    let first = job("small-1", &small, 1_000, spec(1, 4));
    let second = job("small-2", &small, 900, spec(1, 4));
    queue.push(first.clone());
    queue.push(second.clone());

    // Room for four jobs at a time.
    let mut state = fleet(
        vec![machine("box", 16, 16)],
        vec![big.clone(), small.clone()],
        queue,
    );
    state.config = patient();
    let plan = schedule(&state);

    assert_eq!(
        offered_projects(&plan, &state.queue),
        vec![
            big.id.clone(),
            small.id.clone(),
            big.id.clone(),
            small.id.clone()
        ],
        "equal weights alternate once each holds something"
    );
    assert!(offer_for(&plan, &first).is_some());
    assert!(offer_for(&plan, &second).is_some());
    assert_eq!(plan.reasons.len(), 98);
}

/// The same across ticks: the small project arrives while the big one
/// holds every slot, and gets the first slots that free up.
#[test]
fn a09_a_late_small_project_gets_the_next_free_slots() {
    let big = project("big");
    let small = project("small");
    let queue: Vec<QueuedJob> = (0..100)
        .map(|i| job(&format!("big-{i}"), &big, 60_000 - i, spec(1, 4)))
        .collect();
    // The first four big jobs finish one per tick, from tick 2 on.
    let durations: BTreeMap<JobId, u64> = queue
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, queued)| (queued.id.clone(), 2 + i as u64))
        .collect();
    let length = |queued: &QueuedJob| durations.get(&queued.id).copied().unwrap_or(6);

    let mut base = fleet(
        vec![machine("box", 16, 16)],
        vec![big.clone(), small.clone()],
        queue,
    );
    base.config = patient();
    let mut sim = Sim::new(base, 1_000);

    let plan = sim.step(length);
    assert_eq!(
        plan.offers.len(),
        4,
        "the big project fills the idle machine"
    );

    let first = job("small-1", &small, 0, spec(1, 4));
    let second = job("small-2", &small, 0, spec(1, 4));
    sim.submit(first.clone());
    sim.submit(second.clone());
    for _ in 0..6 {
        sim.step(length);
    }

    assert_eq!(sim.start_tick(&first).min(sim.start_tick(&second)), Some(2));
    assert_eq!(sim.start_tick(&first).max(sim.start_tick(&second)), Some(3));
    // The big project keeps running too: it is limited, not stopped.
    let big_started = sim
        .started
        .iter()
        .filter(|(_, j, _)| j.project == big.id)
        .count();
    assert!(big_started >= 6, "big project started {big_started} jobs");
}

#[test]
fn project_weight_sets_the_share_under_contention() {
    let mut heavy = project("heavy");
    heavy.policy.weight = 300;
    let light = project("light");
    let mut queue = Vec::new();
    for i in 0..200 {
        queue.push(job(&format!("heavy-{i}"), &heavy, 50_000 - i, spec(1, 1)));
        queue.push(job(&format!("light-{i}"), &light, 40_000 - i, spec(1, 1)));
    }
    let mut base = fleet(
        vec![machine("box", 8, 8)],
        vec![heavy.clone(), light.clone()],
        queue,
    );
    base.config = patient();
    let mut sim = Sim::new(base, 1_000);
    for _ in 0..30 {
        sim.step(|_| 3);
    }

    let total = sim.started.len();
    let heavy_started = sim
        .started
        .iter()
        .filter(|(_, j, _)| j.project == heavy.id)
        .count();
    assert_eq!(total, 80, "eight slots refilled every three ticks");
    let percent = heavy_started * 100 / total;
    assert!(
        (65..=85).contains(&percent),
        "weight 300 against 100 should hold about three quarters, got {percent}%"
    );
}

/// Opening more sessions does not buy a project a larger share.
#[test]
fn sessions_add_no_weight() {
    let many = project("many-sessions");
    let one = project("one-session");
    let mut queue = Vec::new();
    for s in 0..5u64 {
        for i in 0..10u64 {
            let queued = job(
                &format!("many-{s}-{i}"),
                &many,
                90_000 - s * 100 - i,
                spec(1, 1),
            );
            queue.push(in_session(queued, &format!("session-{s}")));
        }
    }
    for i in 0..10u64 {
        let queued = job(&format!("one-{i}"), &one, 50_000 - i, spec(1, 1));
        queue.push(in_session(queued, "only"));
    }
    let mut state = fleet(
        vec![machine("box", 8, 8)],
        vec![many.clone(), one.clone()],
        queue,
    );
    state.config = patient();
    let plan = schedule(&state);

    let offered = offered_projects(&plan, &state.queue);
    assert_eq!(offered.len(), 8);
    assert_eq!(offered.iter().filter(|id| **id == many.id).count(), 4);
    assert_eq!(offered.iter().filter(|id| **id == one.id).count(), 4);
}

#[test]
fn sessions_inside_a_project_take_turns() {
    let p = project("app");
    let mut queue = Vec::new();
    for i in 0..20u64 {
        queue.push(in_session(
            job(&format!("busy-{i}"), &p, 90_000 - i, spec(1, 1)),
            "busy",
        ));
    }
    let quiet_a = in_session(job("quiet-a", &p, 2_000, spec(1, 1)), "quiet");
    let quiet_b = in_session(job("quiet-b", &p, 1_000, spec(1, 1)), "quiet");
    queue.push(quiet_a.clone());
    queue.push(quiet_b.clone());

    let mut state = fleet(vec![machine("box", 4, 4)], vec![p], queue);
    state.config = patient();
    let plan = schedule(&state);

    let sessions: Vec<_> = plan
        .offers
        .iter()
        .map(|offer| {
            let queued = state
                .queue
                .iter()
                .find(|q| q.id == offer.job_id)
                .expect("queued");
            queued.session.clone()
        })
        .collect();
    assert_eq!(
        sessions,
        vec![
            Some(session("busy")),
            Some(session("quiet")),
            Some(session("busy")),
            Some(session("quiet")),
        ]
    );
    // Inside a session the oldest job goes first.
    assert_eq!(plan.offers[1].job_id, quiet_a.id);
    assert_eq!(plan.offers[3].job_id, quiet_b.id);
}

#[test]
fn a_session_that_already_holds_attempts_waits_for_one_that_holds_none() {
    let mut p = project("app");
    let mut box_ = machine("box", 4, 4);
    // The busy session already runs three attempts; one slot is free.
    box_.reserved = vector(3, 3);
    box_.held_attempts = 3;
    p.reserved = vector(3, 3);
    p.held_attempts = 3;
    let older = in_session(job("busy-next", &p, 60_000, spec(1, 1)), "busy");
    let newer = in_session(job("quiet-first", &p, 1_000, spec(1, 1)), "quiet");

    let mut state = fleet(
        vec![box_],
        vec![p.clone()],
        vec![older.clone(), newer.clone()],
    );
    state.config = patient();
    state.held = (0..3)
        .map(|_| HeldAttempt {
            project: p.id.clone(),
            session: Some(session("busy")),
            priority: Priority::Normal,
            reserved: vector(1, 1),
        })
        .collect();

    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 1);
    assert_eq!(plan.offers[0].job_id, newer.id);

    // Without the held attempts as input, age decides.
    state.held.clear();
    let plan = schedule(&state);
    assert_eq!(plan.offers[0].job_id, older.id);
}

#[test]
fn the_higher_band_goes_first_when_neither_holds_anything() {
    let p = project("app");
    let normal = job("normal", &p, 60_000, spec(1, 1));
    let interactive = with_priority(
        job("interactive", &p, 1_000, spec(1, 1)),
        Priority::Interactive,
    );
    let mut state = fleet(
        vec![machine("box", 1, 1)],
        vec![p],
        vec![normal.clone(), interactive.clone()],
    );
    state.config = patient();
    let plan = schedule(&state);
    assert!(offer_for(&plan, &interactive).is_some());
    assert!(offer_for(&plan, &normal).is_none());
}

/// An endless stream of interactive jobs, arriving faster than they can
/// run, does not keep a background job from starting.
#[test]
fn an_endless_interactive_stream_does_not_starve_background_work() {
    let p = project("app");
    let mut base = fleet(vec![machine("box", 2, 8)], vec![p.clone()], Vec::new());
    base.config = patient();
    let mut sim = Sim::new(base, 1_000);

    let mut serial = 0;
    let mut submit_interactive = |sim: &mut Sim, count: usize| {
        for _ in 0..count {
            serial += 1;
            let queued = job(&format!("interactive-{serial}"), &p, 0, spec(1, 1));
            sim.submit(with_priority(queued, Priority::Interactive));
        }
    };

    // Both slots are taken by interactive work before the background job
    // exists, and two more interactive jobs arrive every tick.
    submit_interactive(&mut sim, 4);
    sim.step(|_| 2);
    let background = with_priority(job("background", &p, 0, spec(1, 1)), Priority::Background);
    sim.submit(background.clone());
    for _ in 0..8 {
        submit_interactive(&mut sim, 2);
        sim.step(|_| 2);
    }

    assert_eq!(
        sim.start_tick(&background),
        Some(2),
        "the background job takes one of the first slots to free up"
    );
    let waiting_interactive = sim.base.queue.len();
    assert!(
        waiting_interactive > 8,
        "the interactive backlog kept growing ({waiting_interactive} waiting)"
    );
}

#[test]
fn bands_share_capacity_by_weight_under_contention() {
    let p = project("app");
    let mut queue = Vec::new();
    for i in 0..200 {
        let interactive = job(&format!("interactive-{i}"), &p, 50_000 - i, spec(1, 1));
        queue.push(with_priority(interactive, Priority::Interactive));
        let background = job(&format!("background-{i}"), &p, 50_000 - i, spec(1, 1));
        queue.push(with_priority(background, Priority::Background));
    }
    let mut base = fleet(vec![machine("box", 8, 8)], vec![p], queue);
    base.config = patient();
    let mut sim = Sim::new(base, 1_000);
    for _ in 0..40 {
        sim.step(|_| 4);
    }

    let total = sim.started.len();
    let interactive = sim
        .started
        .iter()
        .filter(|(_, j, _)| j.spec.priority == Priority::Interactive)
        .count();
    assert_eq!(total, 80, "eight slots refilled every four ticks");
    let percent = interactive * 100 / total;
    // Weights 8 and 2: four fifths interactive, one fifth background.
    assert!(
        (70..=85).contains(&percent),
        "interactive should hold about 80%, got {percent}%"
    );
    assert!(
        total - interactive >= 10,
        "background keeps making progress"
    );
}

fn aging(after_ms: u64) -> SchedulerConfig {
    SchedulerConfig {
        aging_after_ms: after_ms,
        ..patient()
    }
}

/// Offer exactly one slot to `queue` and return the job that got it.
fn winner(
    config: SchedulerConfig,
    projects: Vec<bella_scheduler::ProjectState>,
    queue: Vec<QueuedJob>,
) -> JobId {
    let mut state = fleet(vec![machine("box", 1, 1)], projects, queue);
    state.config = config;
    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 1);
    plan.offers[0].job_id.clone()
}

#[test]
fn waiting_raises_a_job_by_one_band() {
    let p = project("app");
    let old_background = with_priority(job("old", &p, 120_000, spec(1, 1)), Priority::Background);
    let fresh_normal = job("fresh", &p, 1_000, spec(1, 1));
    let queue = vec![old_background.clone(), fresh_normal.clone()];

    // Aged into the normal band, where the older job goes first.
    assert_eq!(
        winner(aging(60_000), vec![p.clone()], queue.clone()),
        old_background.id
    );
    // Without aging the normal job outranks it.
    assert_eq!(winner(patient(), vec![p], queue), fresh_normal.id);
}

#[test]
fn aging_never_raises_more_than_one_band() {
    let p = project("app");
    let ancient = with_priority(
        job("ancient", &p, 36_000_000, spec(1, 1)),
        Priority::Background,
    );
    let fresh = with_priority(job("fresh", &p, 1_000, spec(1, 1)), Priority::Interactive);
    let queue = vec![ancient, fresh.clone()];
    assert_eq!(winner(aging(60_000), vec![p], queue), fresh.id);
}

#[test]
fn aging_never_reaches_urgent() {
    let mut p = project("app");
    p.policy.allow_urgent = true;
    let ancient = with_priority(
        job("ancient", &p, 36_000_000, spec(1, 1)),
        Priority::Interactive,
    );
    let urgent = with_priority(job("urgent", &p, 1_000, spec(1, 1)), Priority::Urgent);
    let queue = vec![ancient, urgent.clone()];
    let config = SchedulerConfig {
        aging_ceiling: Priority::Urgent,
        ..aging(60_000)
    };
    assert_eq!(winner(config, vec![p], queue), urgent.id);
}

#[test]
fn the_aging_ceiling_is_configurable_below_interactive() {
    let p = project("app");
    let old_normal = job("old", &p, 120_000, spec(1, 1));
    let fresh = with_priority(job("fresh", &p, 1_000, spec(1, 1)), Priority::Interactive);
    let queue = vec![old_normal.clone(), fresh.clone()];

    // Default ceiling: the old normal job reaches the interactive band.
    assert_eq!(
        winner(aging(60_000), vec![p.clone()], queue.clone()),
        old_normal.id
    );
    let capped = SchedulerConfig {
        aging_ceiling: Priority::Normal,
        ..aging(60_000)
    };
    assert_eq!(winner(capped, vec![p], queue), fresh.id);
}

#[test]
fn urgent_needs_the_projects_permission() {
    let mut p = project("app");
    let older_interactive = with_priority(
        job("interactive", &p, 5_000, spec(1, 1)),
        Priority::Interactive,
    );
    let urgent = with_priority(job("urgent", &p, 1_000, spec(1, 1)), Priority::Urgent);
    let queue = vec![older_interactive.clone(), urgent.clone()];

    // Not allowed: served as interactive, so the older job goes first.
    assert_eq!(
        winner(patient(), vec![p.clone()], queue.clone()),
        older_interactive.id
    );
    p.policy.allow_urgent = true;
    assert_eq!(winner(patient(), vec![p], queue), urgent.id);
}
