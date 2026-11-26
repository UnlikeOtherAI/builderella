//! Acceptance scenarios assigned to this crate: A04, A05, A06, A07, A08, A65.

use crate::common::{GIB, Host, MIB, T0, evicted, id, offered, request, running};
use bella_core::job::{Priority, QueueReasonCode};
use bella_core::machine::{MachineProfile, PressureLevel};
use bella_protect::{Action, History, Input, actions, admit, budget};
use std::collections::BTreeMap;

const EXAMPLE_MEMORY: u64 = 32 * GIB - 32 * GIB / 100 * 20;

/// Exhaustive on purpose: a new kind of action, such as a suspension, has
/// to be considered here before this file compiles again.
fn kind(action: &Action) -> &'static str {
    match action {
        Action::StopAdmission { .. } => "stop",
        Action::SetCpuLimit { .. } => "cpu",
        Action::Evict { .. } => "evict",
        Action::ResumeAdmission => "resume",
    }
}

/// A04: an accepted offer counts in full before it uses anything.
#[test]
fn a04_offered_and_unstarted_grants_count_in_full() {
    // Ceiling 8 CPUs and 25.6 GiB; 30 GiB available; the grant is idle.
    let mut host = Host::shared_workstation();
    host.grants = vec![offered(1, 6000, 20 * GIB)];

    // Policy capacity: 20 + 6 GiB does not fit 25.6, 6 + 2.001 CPUs not 8.
    let memory = host.admit(&request(1000, 6 * GIB)).expect_err("memory");
    assert_eq!(memory.code, QueueReasonCode::WaitingForCapacity);
    let cpu = host.admit(&request(2001, GIB)).expect_err("cpu");
    assert_eq!(cpu.code, QueueReasonCode::WaitingForCapacity);
    assert_eq!(host.admit(&request(2000, 5 * GIB)), Ok(()));
    assert_eq!(host.budget().free.cpu_milli, 2000);
    assert_eq!(host.budget().free.memory_bytes, EXAMPLE_MEMORY - 20 * GIB);

    // Immediate safety: 26 − 2 − 0.5 − 20 promised = 3.5 GiB.
    host.set_available(26 * GIB);
    let unsafe_now = host.admit(&request(1000, 5 * GIB)).expect_err("safety");
    assert_eq!(unsafe_now.code, QueueReasonCode::ForegroundProtection);
    assert_eq!(
        unsafe_now.message,
        "needs 5.0 GiB; 3.5 GiB can be granted safely now (available 26.0, floor 2.0, guard 0.5, promised 20.0)"
    );
    assert_eq!(host.admit(&request(1000, 3 * GIB + GIB / 2)), Ok(()));

    // A use figure reported before the start does not shrink the promise.
    host.grants[0].memory_used_bytes = Some(18 * GIB);
    assert!(host.admit(&request(1000, 5 * GIB)).is_err());

    // Once it runs and really uses 18 GiB, available memory has dropped by
    // as much, and the answer is the same 3.5 GiB.
    host.grants[0].started = true;
    host.set_available(8 * GIB);
    assert_eq!(host.admit(&request(1000, 3 * GIB + GIB / 2)), Ok(()));
    assert!(host.admit(&request(1000, 3 * GIB + GIB / 2 + 1)).is_err());

    // It also takes the workstation's single slot from the moment it is held.
    let mut single = Host::workstation();
    single.grants = vec![offered(1, 1000, GIB)];
    let slot = single.admit(&request(1000, GIB)).expect_err("slot");
    assert_eq!(slot.code, QueueReasonCode::MachineConcurrencyLimit);
}

/// A05: use beyond a reservation is charged at once, stops admission and is
/// remedied by evicting the offender once the host is under pressure.
#[test]
fn a05_observed_excess_is_charged_stops_admission_and_is_evicted_under_pressure() {
    let mut host = Host::shared_workstation();
    let mut bystander = running(2, 1000, 2 * GIB, GIB);
    bystander.priority = Priority::Maintenance;
    host.grants = vec![running(1, 2000, 4 * GIB, 6 * GIB), bystander];

    // Charged at the 6 GiB it uses, not the 4 GiB it reserved.
    let budget = host.budget();
    assert_eq!(budget.free.memory_bytes, EXAMPLE_MEMORY - 8 * GIB);
    assert!(!budget.admitting);
    assert!(
        budget
            .reasons
            .iter()
            .any(|r| r
                .contains("uses 6.0 GiB of memory against a 4.0 GiB reservation (2.0 GiB over)")),
        "{:?}",
        budget.reasons
    );
    let rejection = host.admit(&request(1000, GIB)).expect_err("excess");
    assert_eq!(rejection.code, QueueReasonCode::MemoryPressure);

    // The host is fine: the excess is reported, nothing is evicted.
    let mut history = History::new();
    assert_eq!(host.tick(&mut history), vec![]);

    // Under pressure the offender goes at once, without a grace period,
    // and not the lower-priority attempt that kept its promise.
    host.set_pressure(PressureLevel::Warning);
    let todo = host.tick(&mut history);
    assert_eq!(todo.len(), 1, "{todo:?}");
    assert!(matches!(
        &todo[0],
        Action::Evict { attempt_id, reason }
            if *attempt_id == id(1)
                && reason.starts_with("memory pressure is warning, available 30.0 GiB; evicting attempt ")
                && reason.ends_with("which uses 6.0 GiB of memory against a 4.0 GiB reservation (2.0 GiB over)")
    ));

    // Available memory below the floor has the same effect.
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 2000, 4 * GIB, 6 * GIB)];
    host.set_available(GIB);
    assert_eq!(evicted(&host.tick(&mut History::new())), vec![id(1)]);

    // Within the tolerance: still charged at real use, admission stays open.
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 2000, 4 * GIB, 4 * GIB + 32 * MIB)];
    let budget = host.budget();
    assert!(budget.admitting);
    assert_eq!(
        budget.free.memory_bytes,
        EXAMPLE_MEMORY - 4 * GIB - 32 * MIB
    );
    assert!(budget.reasons[0].contains("charged at its real use"));
    host.set_pressure(PressureLevel::Warning);
    assert_eq!(evicted(&host.tick(&mut History::new())), vec![]);
}

/// A06: suspending an attempt frees neither memory nor tokens, and is never
/// the answer to memory pressure.
#[test]
fn a06_a_suspended_grant_still_holds_its_memory_and_tokens() {
    let mut host = Host::shared_workstation();
    host.policy.tokens = BTreeMap::from([("ios-simulator".to_string(), 1)]);
    let mut held = running(1, 2000, 20 * GIB, 12 * GIB);
    held.reserved = held.reserved.with_token("ios-simulator", 1);
    host.grants = vec![held];
    host.set_available(18 * GIB);

    let requests = [
        // 20 + 6 GiB exceeds the 25.6 GiB ceiling.
        request(1000, 6 * GIB),
        // 18 − 2 − 0.5 − 8 promised = 7.5 GiB is safe.
        request(1000, 5 * GIB),
        request(1000, 8 * GIB),
        // The only simulator is taken.
        request(1000, GIB).with_token("ios-simulator", 1),
    ];
    let while_running: Vec<_> = requests.iter().map(|q| host.admit(q)).collect();
    let budget_running = host.budget();
    assert!(while_running[0].is_err());
    assert_eq!(while_running[1], Ok(()));
    assert!(while_running[2].is_err());
    assert!(while_running[3].is_err());
    assert_eq!(budget_running.free.token("ios-simulator"), 0);

    host.grants[0].suspended = true;
    let while_suspended: Vec<_> = requests.iter().map(|q| host.admit(q)).collect();
    assert_eq!(while_suspended, while_running);
    assert_eq!(host.budget(), budget_running);

    // Memory relief for a host with a suspended attempt is its eviction.
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    let mut kinds = Vec::new();
    for _ in 1..=4 {
        kinds.extend(host.tick(&mut history).iter().map(kind));
    }
    assert_eq!(kinds, vec!["stop", "cpu", "evict"]);
}

/// A07: the pressure response has hysteresis. A noisy signal neither stops
/// and resumes admission in turn, nor evicts.
#[test]
fn a07_pressure_response_has_hysteresis_and_does_not_flap() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    let mut history = History::new();
    let mut log: Vec<(u64, &'static str)> = Vec::new();
    let mut run = |host: &mut Host, history: &mut History, level: PressureLevel| {
        host.set_pressure(level);
        let todo = host.tick(history);
        log.extend(todo.iter().map(|a| (host.elapsed(), kind(a))));
    };
    use PressureLevel::{Critical, Normal, Warning};

    // 1–60 s: warning every other second. Never sustained: nothing.
    for second in 0..60 {
        let level = if second % 2 == 0 { Warning } else { Normal };
        run(&mut host, &mut history, level);
    }
    // 61–66 s: steady warning. Five seconds of it end at 66 s.
    for _ in 0..6 {
        run(&mut host, &mut history, Warning);
    }
    // 67–126 s: noisy again, with critical spikes. Stays stopped; no
    // critical run lasts three seconds, so nothing is evicted.
    for second in 0..60 {
        let level = match second % 4 {
            0 => Normal,
            1 => Critical,
            2 => Warning,
            _ => Normal,
        };
        run(&mut host, &mut history, level);
    }
    // Normal from the last noisy sample at 126 s on; thirty seconds of it
    // end at 156 s.
    for _ in 0..60 {
        run(&mut host, &mut history, Normal);
    }

    assert_eq!(
        log,
        vec![(66, "stop"), (66, "cpu"), (156, "cpu"), (156, "resume")]
    );
}

/// A08: the owner is protected with the controller unreachable. The API has
/// no controller input at all, so there is nothing to be unreachable.
#[test]
fn a08_protection_decisions_take_no_controller_input() {
    let mut host = Host::with(MachineProfile::ProtectedWorkstation, 10, 32);
    host.grants = vec![running(1, 4000, 8 * GIB, 4 * GIB)];

    // This is everything a decision can depend on. The pattern names every
    // field: adding a "connected" or controller-supplied input would stop
    // this test compiling.
    let Input {
        policy: _,
        hardware: _,
        telemetry: _,
        now: _,
        grants: _,
        control: _,
        config: _,
    } = host.input();

    // The owner starts working: admission, CPU and memory are all handled
    // from local observations alone.
    host.set_cpu(7000, 3000);
    host.set_available(GIB + GIB / 2);
    host.set_pressure(PressureLevel::Critical);
    let input = host.input();
    let refusal = admit(&input, &request(1000, GIB)).expect_err("protected");
    assert_eq!(refusal.code, QueueReasonCode::MemoryPressure);
    assert!(!budget(&input).admitting);
    assert_eq!(budget(&input).ceiling.cpu_milli, 3000);

    let mut history = History::new();
    let mut kinds = Vec::new();
    for _ in 1..=4 {
        kinds.extend(host.tick(&mut history).iter().map(kind));
    }
    // 1 s: CPU limited to the 3 CPUs the foreground leaves. 3 s: memory
    // below the floor for two seconds: stop, halve the CPU limit, evict.
    assert_eq!(kinds, vec!["cpu", "stop", "cpu", "evict"]);
    assert_eq!(history.cpu_limit(&id(1)), Some(2000));

    // The same inputs give the same decisions, whenever and wherever asked.
    let again = History::new();
    assert_eq!(
        actions(&host.input(), &again),
        actions(&host.input(), &again)
    );
    assert_eq!(budget(&host.input()), budget(&host.input()));
    assert_eq!(host.now, T0.plus_secs(4));
}

/// A65: memory a grant already uses has already left "available"; only the
/// unused part of its promise is subtracted again.
#[test]
fn a65_memory_in_use_is_not_subtracted_twice_and_promises_are_kept() {
    // 64 GiB workstation: ceiling 51.2 GiB, floor 2 GiB, guard 0.5 GiB.
    let mut host = Host::with(MachineProfile::ProtectedWorkstation, 16, 64);
    host.policy.max_concurrent_attempts = None;
    // One grant reserved 16 GiB and uses 14 of them. Those 14 GiB are
    // already missing from the 20 GiB the OS reports as available.
    host.grants = vec![running(1, 2000, 16 * GIB, 14 * GIB)];
    host.set_available(20 * GIB);

    let floor = 2 * GIB;
    let guard = GIB / 2;
    let (available, reserved, used) = (20 * GIB, 16 * GIB, 14 * GIB);
    let correct = available - floor - guard - (reserved - used);
    assert_eq!(correct, 15 * GIB + GIB / 2);
    assert_eq!(host.budget().free.memory_bytes, correct);

    // Subtracting the whole reservation again would leave 1.5 GiB and
    // wrongly refuse a 12 GiB request.
    let double_subtraction = available - floor - guard - reserved;
    assert_eq!(double_subtraction, GIB + GIB / 2);
    assert!(double_subtraction < 12 * GIB);
    assert_eq!(host.admit(&request(1000, 12 * GIB)), Ok(()));

    // Ignoring the promise would leave 17.5 GiB and wrongly admit a 16 GiB
    // request, although the grant may still grow by 2 GiB.
    let no_promise = available - floor - guard;
    assert_eq!(no_promise, 17 * GIB + GIB / 2);
    assert!(no_promise >= 16 * GIB);
    let rejection = host.admit(&request(1000, 16 * GIB)).expect_err("promise");
    assert_eq!(rejection.code, QueueReasonCode::ForegroundProtection);
    assert_eq!(
        rejection.message,
        "needs 16.0 GiB; 15.5 GiB can be granted safely now (available 20.0, floor 2.0, guard 0.5, promised 2.0)"
    );

    // The boundary is exact.
    assert_eq!(host.admit(&request(1000, correct)), Ok(()));
    assert!(host.admit(&request(1000, correct + 1)).is_err());

    // Low use never shrinks the promise: using less leaves more promised.
    host.grants[0].memory_used_bytes = Some(4 * GIB);
    host.set_available(30 * GIB);
    assert_eq!(
        host.budget().free.memory_bytes,
        30 * GIB - floor - guard - 12 * GIB
    );
}
