//! Rules 5 and 6: which machine, and never the same capacity twice.

use crate::support::*;
use bella_core::job::QueueReasonCode as Code;
use bella_core::machine::{MachinePolicy, MachineProfile};
use bella_core::platform::{CapabilityRequirement, Os, PlacementChoice, PlatformSelector};
use bella_scheduler::{MachineState, schedule};

fn mac(name: &str, cpus: u64, memory_gib: u64) -> MachineState {
    let mut m = machine(name, cpus, memory_gib);
    m.platform = MAC;
    m.os_version = "15.1".to_string();
    m.capabilities = vec![capability("xcode", "16.2")];
    m
}

#[test]
fn a_preference_is_honoured_when_possible_and_dropped_when_not() {
    let p = project("app");
    let linux = machine("linux-box", 32, 128);
    let apple = mac("mac-box", 8, 32);
    let mut prefers_mac = spec(4, 8);
    prefers_mac.placement = PlacementChoice::Prefer {
        platform: PlatformSelector::os(Os::Macos),
    };
    let queued = job("build", &p, 0, prefers_mac);

    // The Linux machine has far more room; the preference still wins.
    let state = fleet(
        vec![linux.clone(), apple.clone()],
        vec![p.clone()],
        vec![queued.clone()],
    );
    assert_eq!(offered_machine(&schedule(&state), &queued), apple.id);

    // With the Mac full, the job falls back instead of waiting.
    let mut full = apple;
    full.reserved = vector(8, 32);
    let state = fleet(vec![linux.clone(), full], vec![p], vec![queued.clone()]);
    assert_eq!(offered_machine(&schedule(&state), &queued), linux.id);
}

#[test]
fn portable_work_stays_off_protected_workstations_when_it_can() {
    let p = project("app");
    let mut workstation = machine("workstation", 32, 128);
    workstation.policy = MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation);
    let builder = machine("builder", 8, 16);
    let queued = job("build", &p, 0, spec(4, 8));

    let state = fleet(
        vec![workstation.clone(), builder.clone()],
        vec![p.clone()],
        vec![queued.clone()],
    );
    assert_eq!(offered_machine(&schedule(&state), &queued), builder.id);

    // When the builder has no room the workstation is used.
    let mut busy = builder;
    busy.reserved = vector(6, 12);
    let state = fleet(
        vec![workstation.clone(), busy],
        vec![p],
        vec![queued.clone()],
    );
    assert_eq!(offered_machine(&schedule(&state), &queued), workstation.id);
}

/// Portable work is not put on the only macOS machine while an Xcode job
/// that can run nowhere else is waiting.
#[test]
fn a_scarce_machine_is_kept_for_the_work_only_it_can_do() {
    let p = project("app");
    let apple = mac("mac-box", 32, 128);
    let linux = machine("linux-box", 8, 16);
    let portable = job("portable", &p, 60_000, spec(4, 8));
    let mut needs_xcode = spec(30, 100);
    needs_xcode.compatibility.requires = vec![CapabilityRequirement::named("xcode")];
    let archive = job("archive", &p, 1_000, needs_xcode);

    // Alone, the portable job takes the roomier Mac.
    let alone = fleet(
        vec![apple.clone(), linux.clone()],
        vec![p.clone()],
        vec![portable.clone()],
    );
    assert_eq!(offered_machine(&schedule(&alone), &portable), apple.id);

    // With the Xcode job waiting behind it, it goes to Linux, and the Mac
    // still has the whole 30 CPUs the archive needs.
    let both = fleet(
        vec![apple.clone(), linux.clone()],
        vec![p],
        vec![portable.clone(), archive.clone()],
    );
    let plan = schedule(&both);
    assert_eq!(offered_machine(&plan, &portable), linux.id);
    assert_eq!(offered_machine(&plan, &archive), apple.id);
}

#[test]
fn the_machine_left_with_most_headroom_wins_and_ties_go_to_the_lowest_id() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(2, 4));

    let mut loaded = machine("loaded", 16, 64);
    loaded.reserved = vector(8, 32);
    let idle = machine("idle", 16, 64);
    let state = fleet(
        vec![loaded, idle.clone()],
        vec![p.clone()],
        vec![queued.clone()],
    );
    assert_eq!(offered_machine(&schedule(&state), &queued), idle.id);

    let one = machine("twin-one", 16, 64);
    let two = machine("twin-two", 16, 64);
    let lowest = one.id.clone().min(two.id.clone());
    for machines in [vec![one.clone(), two.clone()], vec![two, one]] {
        let state = fleet(machines, vec![p.clone()], vec![queued.clone()]);
        assert_eq!(offered_machine(&schedule(&state), &queued), lowest);
    }
}

/// Two jobs that each need more than half of the only machine: the second
/// is checked against what the first one's offer already took.
#[test]
fn a01_two_offers_in_one_call_never_share_the_same_capacity() {
    let p = project("app");
    let first = job("first", &p, 2_000, spec(2, 10));
    let second = job("second", &p, 1_000, spec(2, 10));
    let box_ = machine("box", 16, 16);
    let state = fleet(
        vec![box_.clone()],
        vec![p.clone()],
        vec![first.clone(), second.clone()],
    );

    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 1);
    assert_eq!(offered_machine(&plan, &first), box_.id);
    let reason = reason_for(&plan, &second);
    assert_eq!(reason.code, Code::WaitingForCapacity);
    assert_eq!(
        reason.message, "box: needs 10 GiB memory; 6 GiB free (10 GiB of 16 GiB reserved)",
        "the first offer counts as reserved"
    );

    // With a second machine each job gets its own.
    let other = machine("other", 16, 16);
    let state = fleet(
        vec![box_, other],
        vec![p],
        vec![first.clone(), second.clone()],
    );
    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 2);
    assert_ne!(
        offered_machine(&plan, &first),
        offered_machine(&plan, &second)
    );
}

/// Offers planned in one call count against the machine's attempt limit,
/// the project's attempt limit and exclusive tokens.
#[test]
fn a02_offers_in_one_call_respect_attempt_limits_and_tokens() {
    let p = project("app");

    // One attempt at a time, however much room there is.
    let mut workstation = machine("workstation", 32, 128);
    workstation.policy = MachinePolicy::defaults_for(MachineProfile::ProtectedWorkstation);
    let first = job("first", &p, 2_000, spec(1, 1));
    let second = job("second", &p, 1_000, spec(1, 1));
    let state = fleet(
        vec![workstation],
        vec![p.clone()],
        vec![first.clone(), second.clone()],
    );
    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 1);
    assert_eq!(plan.offers[0].job_id, first.id);
    assert_eq!(plan.offers[0].max_concurrent, Some(1));
    let reason = reason_for(&plan, &second);
    assert_eq!(reason.code, Code::MachineConcurrencyLimit);
    assert_eq!(
        reason.message,
        "workstation: runs 1 of at most 1 concurrent attempts"
    );

    // A project limited to three attempts, one already running.
    let mut limited = project("limited");
    limited.policy.max_concurrent_attempts = Some(3);
    limited.held_attempts = 1;
    let queue: Vec<_> = (0..5)
        .map(|i| job(&format!("limited-{i}"), &limited, 10_000 - i, spec(1, 1)))
        .collect();
    let state = fleet(vec![machine("box", 32, 128)], vec![limited], queue.clone());
    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 2);
    for waiting in &queue[2..] {
        let reason = reason_for(&plan, waiting);
        assert_eq!(reason.code, Code::ProjectConcurrencyLimit);
        assert!(
            reason.message.contains("runs 3 of at most 3"),
            "{}",
            reason.message
        );
    }

    // One simulator: only one of two simulator jobs is offered.
    let mut one_simulator = machine("mac-box", 16, 64);
    one_simulator
        .managed_ceiling
        .tokens
        .insert("ios-simulator".to_string(), 1);
    one_simulator.effective_budget = Some(one_simulator.managed_ceiling.clone());
    let simulator_job = |name: &str, age: u64| {
        let mut s = spec(1, 1);
        s.envelope
            .peak
            .tokens
            .insert("ios-simulator".to_string(), 1);
        job(name, &p, age, s)
    };
    let (ui_a, ui_b) = (simulator_job("ui-a", 2_000), simulator_job("ui-b", 1_000));
    let state = fleet(
        vec![one_simulator],
        vec![p],
        vec![ui_a.clone(), ui_b.clone()],
    );
    let plan = schedule(&state);
    assert_eq!(plan.offers.len(), 1);
    assert_eq!(plan.offers[0].job_id, ui_a.id);
    let reason = reason_for(&plan, &ui_b);
    assert_eq!(reason.code, Code::WaitingForCapacity);
    assert!(
        reason
            .message
            .contains("needs 1 `ios-simulator` token; 0 free"),
        "{}",
        reason.message
    );
}

/// A job whose peak is the app (8 GiB) plus its tests (4 GiB) is placed
/// only where the whole 12 GiB fits, never where just the app would.
#[test]
fn a19_the_combined_peak_must_fit_as_a_whole() {
    let p = project("app");
    let app_and_tests = job("app-and-tests", &p, 0, spec(4, 12));

    let mut ten_free = machine("ten-free", 16, 32);
    ten_free.reserved = vector(2, 22);
    let mut fourteen_free = machine("fourteen-free", 16, 16);
    fourteen_free.reserved = vector(2, 2);

    // Ten free gigabytes would hold the app alone, and that machine is
    // otherwise the roomier one, but the peak does not fit.
    let alone = fleet(
        vec![ten_free.clone()],
        vec![p.clone()],
        vec![app_and_tests.clone()],
    );
    let plan = schedule(&alone);
    assert!(plan.offers.is_empty());
    let reason = reason_for(&plan, &app_and_tests);
    assert_eq!(reason.code, Code::WaitingForCapacity);
    assert_eq!(
        reason.message,
        "ten-free: needs 12 GiB memory; 10 GiB free (22 GiB of 32 GiB reserved)"
    );

    let both = fleet(
        vec![ten_free, fourteen_free.clone()],
        vec![p],
        vec![app_and_tests.clone()],
    );
    let plan = schedule(&both);
    assert_eq!(offered_machine(&plan, &app_and_tests), fourteen_free.id);
    assert_eq!(plan.offers[0].reservation, vector(4, 12));
}

#[test]
fn an_offer_carries_what_was_reserved_and_what_it_was_checked_against() {
    let p = project("app");
    let queued = job("build", &p, 0, spec(4, 8));
    let mut m = machine("box", 16, 64);
    m.policy.max_concurrent_attempts = Some(3);
    // The worker currently grants less than its ceiling.
    let mut budget = m.managed_ceiling.clone();
    budget.memory_bytes = 24 * GIB;
    budget.cpu_milli = 12_000;
    m.effective_budget = Some(budget.clone());

    let plan = schedule(&fleet(vec![m.clone()], vec![p], vec![queued.clone()]));
    let offer = offer_for(&plan, &queued).expect("the job fits");
    assert_eq!(offer.machine_id, m.id);
    assert_eq!(offer.reservation, queued.spec.envelope.peak);
    assert_eq!(offer.capacity, budget);
    assert_eq!(offer.max_concurrent, Some(3));
    assert!(plan.reasons.is_empty());
}
