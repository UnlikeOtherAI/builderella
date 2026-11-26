//! The local protection loop, limits and execution authority. None of it
//! needs the controller.

use crate::support::*;
use bella_core::AttemptId;
use bella_core::defaults::{GIB, MIB};
use bella_core::job::{Outcome, QueueReasonCode};
use bella_core::machine::{AvailabilityFlag, PressureLevel};
use bella_core::protocol::{
    AttemptEvent, CommandResult, ControllerCommand, ExecutionAuthority, LocalAttemptState,
    OfferDecision, StepState,
};
use bella_core::resources::{Enforcement, ResourceVector};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Tuning with protection graces short enough for a test.
fn quick_protection() -> Options {
    let mut options = Options::default();
    let protect = &mut options.tuning.protect;
    protect.pressure_warning_grace = Duration::from_millis(100);
    protect.pressure_critical_grace = Duration::from_millis(200);
    protect.floor_breach_grace = Duration::from_millis(200);
    protect.eviction_cooldown = Duration::from_millis(200);
    options
}

/// Wait until the journal shows `attempt` released, without a controller.
fn wait_released_offline(h: &Harness, attempt: &AttemptId) -> bella_journal::AttemptRecord {
    let mut found = None;
    wait_until("the attempt to be released", || {
        found = h
            .journal()
            .attempt(attempt)
            .unwrap()
            .filter(|record| record.state == LocalAttemptState::Released);
        found.is_some()
    });
    found.unwrap()
}

#[test]
fn a05_a_memory_limit_hit_is_resource_exhausted_not_a_failure() {
    let mut h = Harness::new();
    let tamper = Tamper::default();
    tamper.memory_hits.store(1, Ordering::SeqCst);
    h.containment = Some(Tampered::native(&tamper));
    h.start_connected();
    let offer = h.launch(spec(vec![helper("oom", "lines|1|137")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    match h.ctl.outcome(&id) {
        Some(Outcome::ResourceExhausted { detail }) => {
            assert!(detail.contains("memory limit"), "{detail}");
            assert!(detail.contains("peak"), "{detail}");
            assert!(
                detail.contains(&format!("limit of {GIB} bytes")),
                "{detail}"
            );
        }
        other => panic!("expected ResourceExhausted, got {other:?}"),
    }
    // The step itself still shows how it ended.
    assert_eq!(
        h.ctl.step_states(&id, "oom"),
        [StepState::Running, StepState::Failed]
    );
}

#[test]
fn a05_an_eviction_for_exceeding_the_reservation_is_resource_exhausted() {
    let mut h = Harness::with(quick_protection());
    let tamper = Tamper::default();
    h.containment = Some(Tampered::native(&tamper));
    h.start_connected();
    let offer = h.launch(spec(vec![helper("grow", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "grow", StepState::Running);
    // The scope now reports four times its reservation, and the host is
    // under pressure.
    tamper.memory_bytes.store(4 * GIB, Ordering::SeqCst);
    h.telemetry.lock().unwrap().memory_pressure = PressureLevel::Warning;
    h.ctl.wait_released(&id);
    match h.ctl.outcome(&id) {
        Some(Outcome::ResourceExhausted { detail }) => {
            assert!(
                detail.contains("exceeding its memory reservation"),
                "{detail}"
            );
            assert!(
                detail.contains(&format!("used {} bytes", 4 * GIB)),
                "{detail}"
            );
            assert!(
                detail.contains(&format!("reservation of {GIB} bytes")),
                "{detail}"
            );
        }
        other => panic!(
            "expected ResourceExhausted, got {other:?}\n{}",
            h.ctl.dump()
        ),
    }
}

/// With an adapter that enforces memory (cgroup v2, job objects) a real
/// process over its limit ends as `ResourceExhausted`. The process-group
/// adapter enforces no memory limit, so there is no limit to hit; the
/// classification itself is covered by the test above on every adapter.
#[test]
fn a05_a_real_process_over_a_hard_memory_limit_is_resource_exhausted() {
    let containment = bella_contain::native().unwrap();
    if containment.enforcement().memory != Enforcement::Hard {
        assert_eq!(containment.mechanism(), "process-group");
        eprintln!("not run: the process-group adapter enforces no memory limit");
        return;
    }
    eprintln!("memory limit enforced by {}", containment.mechanism());
    let mut h = Harness::new();
    h.containment = Some(containment);
    h.start_connected();
    let mut spec = spec(vec![helper("hog", "hog|512|30")]);
    spec.envelope =
        bella_core::resources::ResourceEnvelope::fixed(ResourceVector::new(1000, 96 * MIB, 0));
    let offer = h.launch(spec);
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    match h.ctl.outcome(&id) {
        Some(Outcome::ResourceExhausted { detail }) => {
            assert!(detail.contains("memory limit"), "{detail}");
        }
        other => panic!(
            "expected ResourceExhausted, got {other:?}\n{}",
            h.ctl.dump()
        ),
    }
}

#[test]
fn a07_pressure_stops_admission_and_evicts_with_no_controller_connected() {
    let mut h = Harness::with(quick_protection());
    h.start_connected();
    let offer = h.launch(spec(vec![helper("work", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "work", StepState::Running);

    // The controller goes away, then the host comes under pressure.
    h.connector.allow.store(false, Ordering::SeqCst);
    h.ctl.drop_connection();
    {
        let mut telemetry = h.telemetry.lock().unwrap();
        telemetry.memory_pressure = PressureLevel::Critical;
        telemetry.memory_available_bytes = GIB;
    }
    let record = wait_released_offline(&h, &id);
    match record.outcome {
        Some(Outcome::InfraInterrupted { detail }) => {
            assert!(
                detail.contains("evicted to protect the machine"),
                "{detail}"
            );
            assert!(detail.contains("critical"), "{detail}");
        }
        other => panic!("expected an eviction, got {other:?}"),
    }

    // Back online: admission is still stopped while the pressure lasts.
    h.connector.allow.store(true, Ordering::SeqCst);
    h.ctl.connect();
    let beat = h
        .ctl
        .wait_heartbeat("a heartbeat that refuses work", |beat| !beat.admitting);
    assert!(
        beat.flags.contains(&AvailabilityFlag::PressureLimited),
        "{:?}",
        beat.flags
    );
    let next = h.offer(spec(vec![helper("x", "lines|1|0")]));
    match h.ctl.offer(&next) {
        OfferDecision::Rejected { code, message } => {
            assert_eq!(code, QueueReasonCode::MemoryPressure);
            assert!(message.contains("admission is stopped"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // The eviction's events were delivered after the reconnect.
    h.ctl.wait_released(&id);
}

#[test]
fn a08_the_protection_loop_and_local_controls_work_without_a_controller() {
    let mut h = Harness::with(Options {
        allow_connections: false,
        ..Options::default()
    });
    h.start();
    wait_until("the protection loop to sample", || {
        h.samples.load(Ordering::SeqCst) >= 5
    });
    assert!(
        h.connector.attempts.load(Ordering::SeqCst) >= 1,
        "it tried to connect"
    );

    // The machine owner pauses the worker while it is offline.
    let control = bella_worker::local_control(&h.data_dir()).unwrap();
    control.set_paused(true).unwrap();
    h.connector.allow.store(true, Ordering::SeqCst);
    let hello = h.ctl.connect();
    assert!(hello.local_control.paused);
    h.ctl.wait_heartbeat("a paused heartbeat", |beat| {
        beat.local_control.paused && !beat.admitting
    });
    let offer = h.offer(spec(vec![helper("x", "lines|1|0")]));
    match h.ctl.offer(&offer) {
        OfferDecision::Rejected { code, .. } => assert_eq!(code, QueueReasonCode::MachinePaused),
        other => panic!("{other:?}"),
    }
    // And resumes it; the running daemon picks that up from the journal.
    control.set_paused(false).unwrap();
    h.ctl.wait_heartbeat("an admitting heartbeat", |beat| {
        !beat.local_control.paused && beat.admitting
    });
    let offer = h.offer(spec(vec![helper("x", "lines|1|0")]));
    assert_eq!(h.ctl.offer(&offer), OfferDecision::Accepted);
}

fn manual_harness() -> Harness {
    Harness::with(Options {
        manual_clock: true,
        ..Options::default()
    })
}

#[test]
fn a17_after_a_clock_jump_a_lapsed_authority_stops_the_attempt() {
    let mut h = manual_harness();
    h.start_connected();
    let offer = h.launch(spec(vec![helper("work", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "work", StepState::Running);

    // The machine sleeps for two minutes; the authority was good for one.
    h.manual.as_ref().unwrap().advance(Duration::from_secs(120));
    // The first thing that arrives after the wake is a renewal. It must
    // not revive the attempt: the expiry check comes first.
    let renew = h.ctl.command(ControllerCommand::RenewAuthority {
        attempt: offer.attempt.clone(),
        authority: ExecutionAuthority {
            valid_until: h.now().plus_secs(60),
            hard_deadline: h.now().plus_secs(3600),
            disconnected_continuation_secs: 0,
        },
    });
    h.ctl.wait_ack(&renew);
    h.ctl.wait_released(&id);
    match h.ctl.outcome(&id) {
        Some(Outcome::InfraInterrupted { detail }) => {
            assert!(detail.contains("authority lapsed"), "{detail}");
        }
        other => panic!("expected InfraInterrupted, got {other:?}"),
    }
    assert_eq!(
        h.ctl.step_states(&id, "work"),
        [StepState::Running, StepState::Stopped]
    );
}

#[test]
fn a17_a_hard_deadline_stops_the_attempt_even_while_connected() {
    let mut h = manual_harness();
    h.start_connected();
    let offer = h.offer(spec(vec![helper("work", "sleep|60")]));
    assert_eq!(h.ctl.offer(&offer), OfferDecision::Accepted);
    let authority = ExecutionAuthority {
        valid_until: h.now().plus_secs(7200),
        hard_deadline: h.now().plus_secs(100),
        disconnected_continuation_secs: 0,
    };
    assert_eq!(
        h.ctl.start(&offer.attempt, authority),
        CommandResult::Applied
    );
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "work", StepState::Running);
    h.manual.as_ref().unwrap().advance(Duration::from_secs(200));
    h.ctl.wait_released(&id);
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::TimedOut { step: None }));
    // A renewal past the hard deadline is refused.
    let renew = h.ctl.command(ControllerCommand::RenewAuthority {
        attempt: offer.attempt.clone(),
        authority: ExecutionAuthority {
            valid_until: h.now().plus_secs(60),
            hard_deadline: h.now().plus_secs(3600),
            disconnected_continuation_secs: 0,
        },
    });
    assert!(matches!(
        h.ctl.wait_ack(&renew),
        CommandResult::Applied | CommandResult::Rejected { .. }
    ));
}

#[test]
fn a17_disconnected_continuation_is_capped_by_the_spec() {
    let mut h = manual_harness();
    h.start_connected();
    let mut spec = spec(vec![helper("work", "sleep|60")]);
    spec.disconnected_continuation_secs = 30;
    let offer = h.offer(spec);
    assert_eq!(h.ctl.offer(&offer), OfferDecision::Accepted);
    let authority = ExecutionAuthority {
        valid_until: h.now().plus_secs(60),
        hard_deadline: h.now().plus_secs(3600),
        disconnected_continuation_secs: 600,
    };
    assert_eq!(
        h.ctl.start(&offer.attempt, authority),
        CommandResult::Applied
    );
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "work", StepState::Running);
    h.connector.allow.store(false, Ordering::SeqCst);
    h.ctl.drop_connection();
    // Past `valid_until`, inside the spec's 30 s: still running.
    h.manual.as_ref().unwrap().advance(Duration::from_secs(80));
    std::thread::sleep(Duration::from_millis(400));
    let record = h.journal().attempt(&id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::Running);
    // Past it: stopped on its own.
    h.manual.as_ref().unwrap().advance(Duration::from_secs(20));
    let record = wait_released_offline(&h, &id);
    assert!(
        matches!(record.outcome, Some(Outcome::InfraInterrupted { .. })),
        "{:?}",
        record.outcome
    );
}

#[test]
fn a25_unverifiable_cleanup_is_quarantined_and_retried_never_released() {
    let mut options = Options::default();
    options.tuning.kill_wait = Duration::from_millis(500);
    let mut h = Harness::with(options);
    let tamper = Tamper::default();
    tamper.membership_unknown.store(true, Ordering::SeqCst);
    h.containment = Some(Tampered::native(&tamper));
    h.start_connected();
    let offer = h.launch(spec(vec![helper("work", "lines|1|0")]));
    let id = offer.attempt.attempt_id.clone();
    let reason = h.ctl.wait_event(&id, "a quarantine", |event| match event {
        AttemptEvent::Quarantined { reason } => Some(reason.clone()),
        _ => None,
    });
    assert!(reason.contains("cannot tell whether scope"), "{reason}");
    h.ctl.wait_heartbeat("the quarantine flag", |beat| {
        beat.flags.contains(&AvailabilityFlag::Quarantined)
            && beat.attempts.iter().any(|report| {
                report.attempt.attempt_id == id && report.state == LocalAttemptState::Quarantined
            })
    });
    // Retries happen, and none of them releases the attempt.
    h.ctl.pump(Duration::from_secs(2));
    assert!(
        !h.ctl
            .events_of(&id)
            .iter()
            .any(|event| matches!(event, AttemptEvent::Released { .. }))
    );
    let held = h.journal().held_resources().unwrap();
    assert_eq!(held.memory_bytes, GIB, "the reservation stays held");
    let quarantines = h
        .ctl
        .events_of(&id)
        .iter()
        .filter(|event| matches!(event, AttemptEvent::Quarantined { .. }))
        .count();
    assert_eq!(quarantines, 1, "an unchanged reason is reported once");

    // Once emptiness can be verified, a retry releases it.
    tamper.membership_unknown.store(false, Ordering::SeqCst);
    let proof = h.ctl.wait_released(&id);
    assert!(proof.scope_empty && proof.workspace_removed);
    assert_eq!(h.ctl.outcome(&id), Some(Outcome::Succeeded));
    h.ctl.wait_heartbeat("no quarantine", |beat| {
        !beat.flags.contains(&AvailabilityFlag::Quarantined)
    });
    h.assert_no_workspaces();
}
