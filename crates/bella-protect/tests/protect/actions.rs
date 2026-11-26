//! `actions`: grace periods, CPU limits, the eviction choice and recovery.

use crate::common::{GIB, Host, T0, cpu_limits, evicted, id, offered, resumes, running, stops};
use bella_core::job::{Priority, RetryClass};
use bella_core::machine::PressureLevel;
use bella_core::{AttemptId, UnixMillis};
use bella_protect::{Action, Grant, History, actions, pressure_actions};

#[test]
fn warning_stops_admission_after_its_grace_and_recovery_resumes_it() {
    let mut host = Host::shared_workstation();
    host.grants = vec![
        running(1, 4000, 4 * GIB, 2 * GIB),
        running(2, 2000, 2 * GIB, GIB),
    ];
    let mut history = History::new();
    host.set_pressure(PressureLevel::Warning);

    // Samples at 1 s .. 5 s cover four seconds of warning: not yet.
    for _ in 1..=5 {
        assert_eq!(host.tick(&mut history), vec![]);
        assert_eq!(history.admission_stopped(), None);
    }
    // The sixth covers five: admission stops and CPU limits are halved.
    let todo = host.tick(&mut history);
    assert_eq!(todo.len(), 3, "{todo:?}");
    assert!(matches!(
        &todo[0],
        Action::StopAdmission { reason }
            if reason == "memory pressure has been at warning or above for 5.0 s (grace 5.0 s), available 30.0 GiB"
    ));
    assert_eq!(cpu_limits(&todo), vec![(id(1), 2000), (id(2), 1000)]);
    assert!(history.admission_stopped().is_some());
    assert_eq!(history.cpu_limit(&id(1)), Some(2000));
    // Warning alone evicts nothing, and nothing is proposed twice.
    assert_eq!(host.tick(&mut history), vec![]);

    // Normal from the sample at 8 s; thirty seconds of it end at 38 s.
    host.set_pressure(PressureLevel::Normal);
    for _ in 8..=37 {
        assert_eq!(host.tick(&mut history), vec![]);
    }
    let todo = host.tick(&mut history);
    assert_eq!(host.elapsed(), 38);
    assert_eq!(
        todo,
        vec![
            Action::SetCpuLimit {
                attempt_id: id(1),
                cpu_milli: 4000
            },
            Action::SetCpuLimit {
                attempt_id: id(2),
                cpu_milli: 2000
            },
            Action::ResumeAdmission,
        ]
    );
    assert_eq!(history.admission_stopped(), None);
    assert_eq!(host.tick(&mut history), vec![]);
}

#[test]
fn critical_pressure_evicts_one_victim_per_cooldown() {
    let mut host = Host::shared_workstation();
    let mut background_old = running(2, 1000, 2 * GIB, GIB);
    background_old.priority = Priority::Background;
    let mut background_new = running(3, 1000, 2 * GIB, GIB);
    background_new.priority = Priority::Background;
    background_new.started_at = Some(UnixMillis(T0.0 + 100));
    host.grants = vec![
        running(1, 1000, 2 * GIB, GIB),
        background_old,
        background_new,
    ];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();

    let mut evictions = Vec::new();
    for _ in 1..=60 {
        let todo = host.tick(&mut history);
        let victims = evicted(&todo);
        assert!(victims.len() <= 1, "one victim at a time: {todo:?}");
        if host.elapsed() == 4 {
            assert_eq!(stops(&todo), 1);
            assert!(matches!(
                todo.last(),
                Some(Action::Evict { reason, .. })
                    if reason.starts_with("memory pressure has been critical for 3.0 s (grace 3.0 s), available 30.0 GiB; evicting attempt ")
                        && reason.ends_with("(priority background, pure), which uses 1.0 GiB of memory within a 2.0 GiB reservation")
            ));
        }
        for victim in victims {
            evictions.push((host.elapsed(), victim));
        }
    }
    // Three seconds of critical, then one eviction per 15 s cooldown:
    // lowest priority and most recently started first. The victims stay in
    // the grant list (cleanup is not instant) and are not chosen again.
    assert_eq!(evictions, vec![(4, id(3)), (19, id(2)), (34, id(1))]);
    assert!(history.is_evicting(&id(3)));
}

/// The first attempt evicted under critical pressure, if any within 10 s.
fn first_victim(grants: Vec<Grant>) -> Option<AttemptId> {
    let mut host = Host::shared_workstation();
    host.grants = grants;
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    (0..10).find_map(|_| evicted(&host.tick(&mut history)).into_iter().next())
}

/// A started 2 GiB grant, 1 GiB over its reservation when `over`.
fn candidate(n: u32, priority: Priority, retry_class: RetryClass, over: bool) -> Grant {
    Grant {
        priority,
        retry_class,
        ..running(n, 1000, 2 * GIB, if over { 3 * GIB } else { GIB })
    }
}

#[test]
fn victim_order() {
    use Priority::{Background, Maintenance, Normal, Urgent};
    use RetryClass::{ExternalSideEffects, IsolatedSideEffects, Pure};

    let later = |mut grant: Grant| {
        grant.started_at = Some(UnixMillis(T0.0 + 1));
        grant
    };
    let unknown_start = |mut grant: Grant| {
        grant.started_at = None;
        grant
    };
    let not_started = |n| Grant {
        priority: Maintenance,
        ..offered(n, 1000, 2 * GIB)
    };

    let rows: Vec<(&str, Vec<Grant>, Option<u32>)> = vec![
        (
            "over its reservation beats everything else",
            vec![
                candidate(1, Maintenance, Pure, false),
                candidate(2, Urgent, IsolatedSideEffects, true),
            ],
            Some(2),
        ),
        (
            "then lowest priority",
            vec![
                candidate(1, Normal, Pure, false),
                candidate(2, Maintenance, IsolatedSideEffects, false),
                candidate(3, Background, Pure, false),
            ],
            Some(2),
        ),
        (
            "then pure before isolated side effects",
            vec![
                later(candidate(1, Normal, IsolatedSideEffects, false)),
                candidate(2, Normal, Pure, false),
            ],
            Some(2),
        ),
        (
            "then most recently started",
            vec![
                candidate(1, Normal, Pure, false),
                later(candidate(2, Normal, Pure, false)),
                candidate(3, Normal, Pure, false),
            ],
            Some(2),
        ),
        (
            "an unknown start time counts as the oldest",
            vec![
                unknown_start(candidate(1, Normal, Pure, false)),
                candidate(2, Normal, Pure, false),
            ],
            Some(2),
        ),
        (
            "external side effects are spared whatever their priority",
            vec![
                candidate(1, Maintenance, ExternalSideEffects, false),
                candidate(2, Urgent, Pure, false),
            ],
            Some(2),
        ),
        (
            "with only external side effects left nothing is evicted",
            vec![
                candidate(1, Maintenance, ExternalSideEffects, false),
                candidate(2, Normal, ExternalSideEffects, false),
            ],
            None,
        ),
        (
            "external side effects go when they are the only offender",
            vec![
                candidate(1, Maintenance, Pure, false),
                candidate(2, Urgent, ExternalSideEffects, true),
            ],
            Some(2),
        ),
        (
            "with another offender the external one is spared",
            vec![
                candidate(1, Maintenance, ExternalSideEffects, true),
                candidate(2, Urgent, Pure, true),
                candidate(3, Maintenance, Pure, false),
            ],
            Some(2),
        ),
        (
            "two external offenders are both spared; relief comes from the rest",
            vec![
                candidate(1, Maintenance, ExternalSideEffects, true),
                candidate(2, Maintenance, ExternalSideEffects, true),
                candidate(3, Urgent, Pure, false),
            ],
            Some(3),
        ),
        (
            "an attempt that has not started holds no memory to give back",
            vec![not_started(1), candidate(2, Urgent, Pure, false)],
            Some(2),
        ),
        (
            "nothing started, nothing to evict",
            vec![not_started(1)],
            None,
        ),
    ];
    for (name, grants, expected) in rows {
        assert_eq!(first_victim(grants), expected.map(id), "{name}");
    }
}

#[test]
fn floor_breach_evicts_after_its_grace_on_a_host_without_pressure_levels() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 4 * GIB, 2 * GIB)];
    host.set_pressure(PressureLevel::Unknown);
    host.set_available(GIB);
    let mut history = History::new();

    assert_eq!(host.tick(&mut history), vec![]);
    assert_eq!(host.tick(&mut history), vec![]);
    let todo = host.tick(&mut history);
    assert_eq!(stops(&todo), 1);
    assert_eq!(evicted(&todo), vec![id(1)]);
    assert!(matches!(
        todo.last(),
        Some(Action::Evict { reason, .. })
            if reason.starts_with("available memory 1.0 GiB has been below the 2.0 GiB floor for 2.0 s (grace 2.0 s); evicting attempt ")
    ));

    // Back above the floor from 4 s; admission resumes thirty seconds on.
    host.set_available(10 * GIB);
    let mut resumed_at = Vec::new();
    for _ in 4..=60 {
        let todo = host.tick(&mut history);
        assert!(evicted(&todo).is_empty());
        if resumes(&todo) > 0 {
            resumed_at.push(host.elapsed());
        }
    }
    assert_eq!(resumed_at, vec![34]);
}

#[test]
fn foreground_cpu_beyond_its_entitlement_is_taken_proportionally() {
    // T = 10, F = 2, static ceiling 8; grants reserve 4 + 2 CPUs.
    let mut host = Host::shared_workstation();
    host.grants = vec![
        running(1, 4000, 2 * GIB, GIB),
        running(2, 2000, 2 * GIB, GIB),
    ];
    let mut history = History::new();

    // Within the entitlement nothing changes.
    host.set_cpu(1500, 3000);
    assert_eq!(host.tick(&mut history), vec![]);

    // Foreground 5: ceiling 5, shared 4:2.
    host.set_cpu(5000, 3000);
    let todo = host.tick(&mut history);
    assert_eq!(cpu_limits(&todo), vec![(id(1), 3333), (id(2), 1666)]);
    assert_eq!(todo.len(), 2, "no admission stop, no eviction: {todo:?}");

    // A change smaller than the step is not worth a new limit.
    host.set_cpu(5100, 3000);
    assert_eq!(host.tick(&mut history), vec![]);

    // Foreground 9.8: 200 mCPU left, but nobody goes below the minimum.
    host.set_cpu(9800, 200);
    let todo = host.tick(&mut history);
    assert_eq!(cpu_limits(&todo), vec![(id(1), 500), (id(2), 500)]);
    assert_eq!(host.elapsed(), 4);

    // Quiet again from 5 s. The limits follow the peak of the last ten
    // seconds, so they return at 14 s, in one step.
    host.set_cpu(1000, 1000);
    for _ in 5..=13 {
        assert_eq!(host.tick(&mut history), vec![]);
    }
    let todo = host.tick(&mut history);
    assert_eq!(cpu_limits(&todo), vec![(id(1), 4000), (id(2), 2000)]);
    assert_eq!(host.tick(&mut history), vec![]);
}

#[test]
fn a_noisy_foreground_does_not_make_cpu_limits_flap() {
    let mut host = Host::shared_workstation();
    host.grants = vec![
        running(1, 4000, 2 * GIB, GIB),
        running(2, 2000, 2 * GIB, GIB),
    ];
    let mut history = History::new();
    let mut changes = Vec::new();
    for second in 0..40 {
        host.set_cpu(if second % 2 == 0 { 5000 } else { 1000 }, 3000);
        changes.extend(cpu_limits(&host.tick(&mut history)));
    }
    assert_eq!(changes, vec![(id(1), 3333), (id(2), 1666)]);
}

#[test]
fn a_boost_suspends_the_foreground_reduction() {
    let mut host = Host::shared_workstation();
    host.grants = vec![
        running(1, 4000, 2 * GIB, GIB),
        running(2, 2000, 2 * GIB, GIB),
    ];
    let mut history = History::new();
    host.set_cpu(5000, 3000);
    host.control.boost_until = Some(T0.plus_secs(3600));
    assert_eq!(host.tick(&mut history), vec![]);
    host.control.boost_until = None;
    assert_eq!(
        cpu_limits(&host.tick(&mut history)),
        vec![(id(1), 3333), (id(2), 1666)]
    );
}

#[test]
fn cpu_limits_respect_small_reservations_and_skip_unstarted_grants() {
    let mut host = Host::shared_workstation();
    host.grants = vec![
        running(1, 300, 2 * GIB, GIB),
        running(2, 5000, 2 * GIB, GIB),
        offered(3, 2000, 2 * GIB),
    ];
    let mut history = History::new();
    // 100 mCPU left. The 300 mCPU grant cannot go below its reservation,
    // which is under the minimum already; the unstarted one has no scope.
    host.set_cpu(9900, 100);
    assert_eq!(cpu_limits(&host.tick(&mut history)), vec![(id(2), 500)]);

    // Foreground 6: ceiling 4, shared 300:5000 between the started grants.
    let mut host = Host::shared_workstation();
    host.grants = vec![
        running(1, 300, 2 * GIB, GIB),
        running(2, 5000, 2 * GIB, GIB),
        offered(3, 2000, 2 * GIB),
    ];
    let mut history = History::new();
    host.set_cpu(6000, 100);
    assert_eq!(cpu_limits(&host.tick(&mut history)), vec![(id(2), 3773)]);
}

#[test]
fn nothing_is_decided_without_fresh_evidence() {
    // No telemetry at all.
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    for _ in 1..=3 {
        host.tick(&mut history);
    }
    let saved = host.telemetry.take();
    for _ in 4..=22 {
        assert_eq!(host.tick(&mut history), vec![]);
    }
    // A hole in the record restarts the grace period: critical again from
    // 23 s, sustained for three seconds at 26 s.
    host.telemetry = saved;
    let mut evictions = Vec::new();
    for _ in 23..=30 {
        if !evicted(&host.tick(&mut history)).is_empty() {
            evictions.push(host.elapsed());
        }
    }
    assert_eq!(evictions, vec![26]);

    // One critical sample seen again and again is still one sample.
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    for second in 0..=5 {
        host.now = T0.plus_secs(second);
        history.observe(&host.input());
        assert_eq!(actions(&host.input(), &history), vec![]);
    }
}

#[test]
fn a_decision_needs_the_current_sample_in_the_record() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    for _ in 1..=3 {
        host.tick(&mut history);
    }
    // A newer sample that was never observed: nothing sustained can be
    // claimed for it.
    host.now = host.now.plus_secs(1);
    host.sample().sampled_at = host.now;
    assert_eq!(actions(&host.input(), &history), vec![]);
    history.observe(&host.input());
    assert_eq!(evicted(&actions(&host.input(), &history)), vec![id(1)]);
}

#[test]
fn actions_are_proposed_until_they_are_recorded() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    let mut proposals = Vec::new();
    for second in 1..=6 {
        host.now = T0.plus_secs(second);
        host.sample().sampled_at = host.now;
        history.observe(&host.input());
        proposals.push(actions(&host.input(), &history));
    }
    for early in &proposals[..3] {
        assert_eq!(early, &vec![]);
    }
    for late in &proposals[3..] {
        assert_eq!(stops(late), 1);
        assert_eq!(evicted(late), vec![id(1)]);
        assert_eq!(cpu_limits(late), vec![(id(1), 500)]);
    }
    // The architecture's name for the same function.
    assert_eq!(pressure_actions(&host.input(), &history), proposals[5]);

    history.record(&proposals[5], host.now);
    assert_eq!(actions(&host.input(), &history), vec![]);
}

#[test]
fn a_clock_step_backwards_restarts_the_record() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    host.tick(&mut history);
    host.tick(&mut history);

    host.now = UnixMillis(T0.0 - 3_600_000);
    let mut evictions = Vec::new();
    for turn in 1..=6 {
        if !evicted(&host.tick(&mut history)).is_empty() {
            evictions.push(turn);
        }
    }
    // Four samples after the step cover three seconds.
    assert_eq!(evictions, vec![4]);
}

#[test]
fn state_for_released_attempts_is_dropped() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    for _ in 1..=4 {
        host.tick(&mut history);
    }
    assert!(history.is_evicting(&id(1)));
    assert_eq!(history.cpu_limit(&id(1)), Some(500));
    host.grants.clear();
    host.tick(&mut history);
    assert!(!history.is_evicting(&id(1)));
    assert_eq!(history.cpu_limit(&id(1)), None);
}

#[test]
fn sampling_faster_than_the_loop_measures_the_same_durations() {
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    host.set_pressure(PressureLevel::Critical);
    let mut history = History::new();
    let mut evictions = Vec::new();
    // A sample every 50 ms for five seconds.
    for step in 0..=100u64 {
        host.now = UnixMillis(T0.0 + step * 50);
        host.sample().sampled_at = host.now;
        history.observe(&host.input());
        let todo = actions(&host.input(), &history);
        history.record(&todo, host.now);
        if !evicted(&todo).is_empty() {
            evictions.push(step * 50);
        }
    }
    // Critical for exactly the three-second grace, then the cooldown.
    assert_eq!(evictions, vec![3000]);

    // One calm reading among fast samples does not hide a critical one
    // taken within the same tenth of a second.
    let mut host = Host::shared_workstation();
    host.grants = vec![running(1, 1000, 2 * GIB, GIB)];
    let mut history = History::new();
    let mut evictions = Vec::new();
    for step in 0..=100u64 {
        host.now = UnixMillis(T0.0 + step * 50);
        host.sample().sampled_at = host.now;
        host.set_pressure(if step % 2 == 0 {
            PressureLevel::Critical
        } else {
            PressureLevel::Normal
        });
        history.observe(&host.input());
        let todo = actions(&host.input(), &history);
        history.record(&todo, host.now);
        if !evicted(&todo).is_empty() {
            evictions.push(step * 50);
        }
    }
    assert_eq!(evictions, vec![3000]);
}
