//! Rule 4: backfill past a large job, bounded by the starvation threshold.

use crate::support::*;
use bella_core::UnixMillis;
use bella_core::job::{Priority, QueueReasonCode as Code};
use bella_core::resources::ResourceVector;
use bella_scheduler::{QueuedJob, SchedulerConfig, schedule};

const TICK_MS: u64 = 1_000;

fn starving_after(threshold_ms: u64) -> SchedulerConfig {
    SchedulerConfig {
        aging_after_ms: u64::MAX,
        starvation_threshold_ms: threshold_ms,
        ..SchedulerConfig::default()
    }
}

#[test]
fn a_small_job_passes_an_older_large_one_that_does_not_fit() {
    let p = project("app");
    let mut box_ = machine("box", 16, 16);
    box_.reserved = vector(1, 8);
    box_.held_attempts = 1;
    let large = job("large", &p, 60_000, spec(1, 12));
    let small = job("small", &p, 1_000, spec(1, 4));
    let state = fleet(
        vec![box_.clone()],
        vec![p],
        vec![large.clone(), small.clone()],
    );

    let plan = schedule(&state);
    assert_eq!(offered_machine(&plan, &small), box_.id);
    let reason = reason_for(&plan, &large);
    assert_eq!(reason.code, Code::WaitingForCapacity);
    assert!(
        !reason.message.contains("takes no other work"),
        "below the threshold nothing is held back: {}",
        reason.message
    );
}

/// What the stream simulation observed.
struct Outcome {
    /// Tick at which the large job started, if it did.
    large_started: Option<u64>,
    /// Ticks at which at least one small job started.
    small_start_ticks: Vec<u64>,
    /// Ticks at which a waiting small job was told `ReservedForOlderJob`.
    held_back_ticks: Vec<u64>,
    /// The large job's reason on the first tick it was held for.
    large_reason_when_held: Option<String>,
}

/// One 16 GiB machine and a steady stream of 4 GiB jobs, one arriving and
/// one finishing every tick, so at most 4 GiB is ever free. A 12 GiB job
/// arrives at tick 4, once the machine is full.
fn stream(threshold_ms: u64, same_project: bool, ticks: u64) -> Outcome {
    let stream_project = project("stream");
    let large_project = if same_project {
        stream_project.clone()
    } else {
        project("large")
    };
    let mut base = fleet(
        vec![machine("box", 16, 16)],
        vec![stream_project.clone(), project("large")],
        Vec::new(),
    );
    base.config = starving_after(threshold_ms);
    let mut sim = Sim::new(base, TICK_MS);
    let large = job("large", &large_project, 0, spec(1, 12));

    let mut outcome = Outcome {
        large_started: None,
        small_start_ticks: Vec::new(),
        held_back_ticks: Vec::new(),
        large_reason_when_held: None,
    };
    for tick in 0..ticks {
        sim.submit(job(
            &format!("small-{tick}"),
            &stream_project,
            0,
            spec(1, 4),
        ));
        if tick == 4 {
            sim.submit(large.clone());
        }
        let plan = sim.step(|_| 4);

        if plan.offers.iter().any(|offer| offer.job_id != large.id) {
            outcome.small_start_ticks.push(tick);
        }
        let held_back = plan
            .reasons
            .iter()
            .any(|(id, reason)| *id != large.id && reason.code == Code::ReservedForOlderJob);
        if held_back {
            outcome.held_back_ticks.push(tick);
            if outcome.large_reason_when_held.is_none() {
                outcome.large_reason_when_held = plan
                    .reasons
                    .get(&large.id)
                    .map(|reason| reason.message.clone());
            }
        }
    }
    outcome.large_started = sim.start_tick(&large);
    outcome
}

/// A large job behind an endless stream of small ones starts within a
/// bounded number of ticks once it has waited past the threshold.
#[test]
fn a10_a_large_job_starts_despite_a_stream_of_small_ones() {
    for same_project in [false, true] {
        // The large job arrives at tick 4 and has waited 5 s at tick 9.
        let outcome = stream(5 * TICK_MS, same_project, 30);

        // Until then small jobs keep passing it, one per tick.
        for tick in 4..9 {
            assert!(
                outcome.small_start_ticks.contains(&tick),
                "same_project={same_project}: a small job backfills at tick {tick}"
            );
        }
        // From tick 9 the machine takes nothing else, and three running
        // jobs drain by tick 11, when the whole 12 GiB is free.
        assert_eq!(
            outcome.held_back_ticks,
            vec![9, 10],
            "same_project={same_project}"
        );
        assert_eq!(
            outcome.large_started,
            Some(11),
            "same_project={same_project}"
        );
        let message = outcome
            .large_reason_when_held
            .expect("the large job was waiting");
        assert!(
            message.contains("needs 12 GiB memory; 4 GiB free")
                && message.contains("box takes no other work until it fits"),
            "{message}"
        );
        // Afterwards the stream resumes.
        assert!(
            outcome.small_start_ticks.iter().any(|tick| *tick > 11),
            "same_project={same_project}: small jobs run again once the large one started"
        );
    }
}

/// The control for the test above: with the rule switched off, the same
/// stream keeps the large job waiting for the whole run.
#[test]
fn a10_without_the_threshold_the_stream_starves_the_large_job() {
    let outcome = stream(u64::MAX, false, 60);
    assert_eq!(outcome.large_started, None);
    assert!(outcome.held_back_ticks.is_empty());
    assert!(outcome.small_start_ticks.len() >= 55);
}

#[test]
fn the_machine_where_the_job_fits_soonest_is_the_one_kept_free() {
    let p = project("app");
    let mut alpha = machine("alpha", 16, 16);
    alpha.reserved = vector(3, 12);
    alpha.held_attempts = 3;
    let mut beta = machine("beta", 16, 16);
    beta.reserved = vector(1, 6);
    beta.held_attempts = 1;

    // Needs 12 GiB: 8 GiB short on alpha, 2 GiB short on beta.
    let large = job("large", &p, 900_000, spec(1, 12));
    let small = job("small", &p, 1_000, spec(1, 4));
    let medium = job("medium", &p, 500, spec(1, 6));
    let mut state = fleet(
        vec![alpha.clone(), beta.clone()],
        vec![p],
        vec![large.clone(), small.clone(), medium.clone()],
    );
    state.config = starving_after(600_000);
    let plan = schedule(&state);

    let waiting = reason_for(&plan, &large);
    assert_eq!(waiting.code, Code::WaitingForCapacity);
    assert!(
        waiting
            .message
            .contains("this job has waited 15 min, so beta takes no other work"),
        "{}",
        waiting.message
    );

    // The small job fits both machines and beta has more room, but beta is
    // kept free, so it goes to alpha.
    assert_eq!(offered_machine(&plan, &small), alpha.id);

    // The medium job fits only beta, and is told why it may not go there.
    let held_back = reason_for(&plan, &medium);
    assert_eq!(held_back.code, Code::ReservedForOlderJob);
    let on_beta = blocker_on(held_back, "beta");
    assert_eq!(on_beta.code, Code::ReservedForOlderJob);
    assert!(!on_beta.permanent);
    assert!(
        on_beta.message.contains(large.id.as_str()),
        "{}",
        on_beta.message
    );
    assert!(
        on_beta.message.contains("has waited 15 min"),
        "{}",
        on_beta.message
    );
    assert_eq!(
        blocker_on(held_back, "alpha").code,
        Code::WaitingForCapacity
    );

    // Without the threshold both fit somewhere and are offered.
    state.config = patient();
    let plan = schedule(&state);
    assert_eq!(offered_machine(&plan, &small), beta.id);
    assert_eq!(offered_machine(&plan, &medium), beta.id);
}

#[test]
fn each_starved_job_keeps_a_different_machine_free() {
    let p = project("app");
    let mut alpha = machine("alpha", 16, 16);
    alpha.reserved = vector(1, 8);
    alpha.held_attempts = 1;
    let mut beta = machine("beta", 16, 16);
    beta.reserved = vector(1, 8);
    beta.held_attempts = 1;

    let first = job("first", &p, 900_000, spec(1, 12));
    let second = job("second", &p, 800_000, spec(1, 12));
    let small = job("small", &p, 1_000, spec(1, 4));
    let mut state = fleet(
        vec![alpha, beta],
        vec![p],
        vec![first.clone(), second.clone(), small.clone()],
    );
    state.config = starving_after(600_000);
    let plan = schedule(&state);

    assert!(plan.offers.is_empty());
    let held_back = reason_for(&plan, &small);
    assert_eq!(held_back.code, Code::ReservedForOlderJob);
    let mut holders: Vec<&str> = Vec::new();
    for blocker in &held_back.blockers {
        assert_eq!(blocker.code, Code::ReservedForOlderJob);
        if blocker.message.contains(first.id.as_str()) {
            holders.push("first");
        }
        if blocker.message.contains(second.id.as_str()) {
            holders.push("second");
        }
    }
    holders.sort_unstable();
    assert_eq!(holders, vec!["first", "second"]);
}

#[test]
fn a_job_past_the_threshold_that_fits_goes_before_fresh_work() {
    let mut p = project("app");
    p.policy.allow_urgent = true;
    let background = {
        let mut s = spec(1, 1);
        s.priority = Priority::Background;
        job("patient", &p, 900_000, s)
    };
    let urgent = {
        let mut s = spec(1, 1);
        s.priority = Priority::Urgent;
        job("urgent", &p, 1_000, s)
    };
    let mut state = fleet(
        vec![machine("box", 1, 1)],
        vec![p],
        vec![background.clone(), urgent.clone()],
    );

    state.config = starving_after(600_000);
    let plan = schedule(&state);
    assert!(offer_for(&plan, &background).is_some());
    assert_eq!(reason_for(&plan, &urgent).code, Code::WaitingForCapacity);

    state.config = patient();
    let plan = schedule(&state);
    assert!(offer_for(&plan, &urgent).is_some());
}

#[test]
fn a_machine_whose_budget_cannot_hold_the_job_is_not_kept_free() {
    let p = project("app");
    // The ceiling would hold 12 GiB, but the worker grants 8 GiB right now,
    // so draining this machine would not let the large job start.
    let mut squeezed = machine("workstation", 16, 16);
    squeezed.effective_budget = Some(ResourceVector::new(16_000, 8 * GIB, 500 * GIB));
    let large = job("large", &p, 900_000, spec(1, 12));
    let small = job("small", &p, 1_000, spec(1, 4));
    let mut state = fleet(
        vec![squeezed.clone()],
        vec![p],
        vec![large.clone(), small.clone()],
    );
    state.config = starving_after(600_000);

    let plan = schedule(&state);
    let waiting = reason_for(&plan, &large);
    assert_eq!(waiting.code, Code::WaitingForCapacity);
    assert!(
        !waiting.message.contains("takes no other work"),
        "{}",
        waiting.message
    );
    assert_eq!(offered_machine(&plan, &small), squeezed.id);

    // Once the budget is back the machine is kept free for it.
    squeezed.effective_budget = Some(squeezed.managed_ceiling.clone());
    squeezed.reserved = vector(1, 8);
    squeezed.held_attempts = 1;
    state.machines = vec![squeezed];
    let plan = schedule(&state);
    assert!(plan.offers.is_empty());
    assert_eq!(reason_for(&plan, &small).code, Code::ReservedForOlderJob);
}

#[test]
fn a_machine_is_only_kept_free_for_a_job_that_is_waiting_for_room_on_it() {
    let p = project("app");
    let box_ = machine("box", 16, 16);
    // The old job is not short of room: this machine declined it.
    let mut declined: QueuedJob = job("declined", &p, 900_000, spec(1, 12));
    declined.recently_rejected_by = vec![(box_.id.clone(), UnixMillis(NOW + 60_000))];
    let small = job("small", &p, 1_000, spec(1, 4));
    let mut state = fleet(
        vec![box_.clone()],
        vec![p],
        vec![declined.clone(), small.clone()],
    );
    state.config = starving_after(600_000);

    let plan = schedule(&state);
    assert_eq!(reason_for(&plan, &declined).code, Code::OfferRejected);
    assert_eq!(offered_machine(&plan, &small), box_.id);
}
