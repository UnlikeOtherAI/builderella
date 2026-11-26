//! Command handling: offers, duplicates, fencing and reconciliation.

use crate::support::*;
use bella_core::defaults::GIB;
use bella_core::job::{Outcome, QueueReasonCode};
use bella_core::protocol::{
    AttemptEvent, AttemptRef, CommandResult, ControllerCommand, LocalAttemptState, OfferDecision,
    ReconcileAction, ReconcileVerdict, RejectCode, StepState,
};
use bella_core::resources::ResourceVector;
use std::time::Duration;

#[test]
fn a11_a_retransmitted_start_never_launches_a_second_copy() {
    let mut h = Harness::new();
    h.start_connected();
    let runs = h.outside("runs");
    let offer = h.offer(spec(vec![
        helper("once", &format!("append|{runs}|run")),
        helper("hold", "sleep|1").needs(&["once"]),
    ]));
    assert_eq!(h.ctl.offer(&offer), OfferDecision::Accepted);
    let authority = h.authority();
    let start = ControllerCommand::Start {
        attempt: offer.attempt.clone(),
        authority,
    };
    let first = bella_core::CommandId::generate();
    h.ctl.command_as(&first, h.ctl.epoch, start.clone());
    // The same command again, and the same start under a new command id.
    h.ctl.command_as(&first, h.ctl.epoch, start.clone());
    let second = h.ctl.command(start);
    assert_eq!(h.ctl.wait_ack(&second), CommandResult::Duplicate);
    let acks: Vec<CommandResult> = h.ctl.wait("both answers to the first id", |ctl| {
        let found: Vec<CommandResult> = ctl
            .acks
            .iter()
            .filter(|(id, _)| *id == first)
            .map(|(_, result)| result.clone())
            .collect();
        (found.len() == 2).then_some(found)
    });
    assert_eq!(acks, [CommandResult::Applied, CommandResult::Duplicate]);

    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_released(&id);
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "run\n");
    let started = h
        .ctl
        .events_of(&id)
        .iter()
        .filter(|event| matches!(event, AttemptEvent::Preparing))
        .count();
    assert_eq!(started, 1);
    // A start after the release is still only a duplicate.
    assert_eq!(
        h.ctl.start(&offer.attempt, h.authority()),
        CommandResult::Duplicate
    );
}

#[test]
fn commands_from_an_older_epoch_or_generation_are_rejected() {
    let mut h = Harness::new();
    h.start_connected();
    let offer = h.launch(spec(vec![helper("wait", "sleep|60")]));
    let id = offer.attempt.attempt_id.clone();
    h.ctl.wait_step(&id, "wait", StepState::Running);

    let stale = bella_core::CommandId::generate();
    h.ctl.command_as(
        &stale,
        h.ctl.epoch - 1,
        ControllerCommand::Cancel {
            attempt: offer.attempt.clone(),
            reason: "from an old controller".to_string(),
            grace_secs: 0,
        },
    );
    match h.ctl.wait_ack(&stale) {
        CommandResult::Rejected { code, .. } => assert_eq!(code, RejectCode::StaleEpoch),
        other => panic!("expected a stale-epoch rejection, got {other:?}"),
    }

    let old_generation = AttemptRef {
        attempt_id: id.clone(),
        generation: offer.attempt.generation - 1,
    };
    match h.ctl.cancel(&old_generation, 0) {
        CommandResult::Rejected { code, .. } => assert_eq!(code, RejectCode::StaleGeneration),
        other => panic!("expected a stale-generation rejection, got {other:?}"),
    }
    // Neither stopped anything.
    h.ctl.pump(Duration::from_millis(300));
    assert_eq!(h.ctl.outcome(&id), None);

    // A revoke cannot withdraw an attempt that has started.
    let revoke = h.ctl.command(ControllerCommand::Revoke {
        attempt: offer.attempt.clone(),
        reason: "too late".to_string(),
    });
    match h.ctl.wait_ack(&revoke) {
        CommandResult::Rejected { code, .. } => assert_eq!(code, RejectCode::InvalidState),
        other => panic!("a started attempt cannot be revoked, got {other:?}"),
    }

    // A newer epoch is accepted and remembered.
    h.ctl.epoch += 1;
    assert_eq!(h.ctl.cancel(&offer.attempt, 0), CommandResult::Applied);
    h.ctl.wait_released(&id);
    assert_eq!(h.journal().known_epoch().unwrap(), h.ctl.epoch);
}

#[test]
fn an_offer_beyond_the_local_budget_is_rejected_with_the_protect_reason() {
    let mut h = Harness::new();
    h.start_connected();
    let mut spec = spec(vec![helper("big", "lines|1|0")]);
    spec.envelope =
        bella_core::resources::ResourceEnvelope::fixed(ResourceVector::new(2000, 200 * GIB, 0));
    let offer = h.offer(spec);

    // What bella-protect itself says about this request on this machine.
    let containment = bella_contain::native().unwrap();
    let inventory = test_inventory(containment.as_ref());
    let mut telemetry = calm_telemetry();
    telemetry.sampled_at = h.now();
    let input = bella_protect::Input {
        policy: &h.ctl.policy,
        hardware: &inventory.hardware,
        telemetry: Some(&telemetry),
        now: h.now(),
        grants: &[],
        control: Default::default(),
        config: &h.tuning.protect,
    };
    let expected = bella_protect::admit(&input, &offer.reservation).expect_err("too large");

    match h.ctl.offer(&offer) {
        OfferDecision::Rejected { code, message } => {
            assert_eq!(code, expected.code);
            assert_eq!(message, expected.message);
            assert!(!message.is_empty());
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    // Nothing is held for it.
    let journal = h.journal();
    assert!(
        journal
            .attempt(&offer.attempt.attempt_id)
            .unwrap()
            .is_none()
    );
    // A start for it finds no hold.
    match h.ctl.start(&offer.attempt, h.authority()) {
        CommandResult::Rejected { code, .. } => assert_eq!(code, RejectCode::UnknownAttempt),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn an_offer_against_another_policy_revision_or_expired_is_rejected() {
    let mut h = Harness::new();
    h.start_connected();
    let mut offer = h.offer(spec(vec![helper("x", "lines|1|0")]));
    offer.policy_revision += 1;
    match h.ctl.offer(&offer) {
        OfferDecision::Rejected { message, .. } => {
            assert!(message.contains("revision"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    let mut offer = h.offer(spec(vec![helper("x", "lines|1|0")]));
    offer.expires_at = h.now().minus(Duration::from_secs(1));
    match h.ctl.offer(&offer) {
        OfferDecision::Rejected { message, .. } => {
            assert!(message.contains("expired"), "{message}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_repeated_offer_command_gets_the_same_answer() {
    let mut h = Harness::new();
    h.start_connected();
    let offer = h.offer(spec(vec![helper("x", "lines|1|0")]));
    let id = bella_core::CommandId::generate();
    let command = ControllerCommand::Offer {
        offer: Box::new(offer.clone()),
    };
    h.ctl.command_as(&id, h.ctl.epoch, command.clone());
    assert_eq!(h.ctl.wait_reply(&id), OfferDecision::Accepted);
    h.ctl.command_as(&id, h.ctl.epoch, command);
    let answers = h.ctl.wait("two replies", |ctl| {
        let found: Vec<OfferDecision> = ctl
            .replies
            .iter()
            .filter(|(reply, _, _)| *reply == id)
            .map(|(_, _, decision)| decision.clone())
            .collect();
        (found.len() == 2).then_some(found)
    });
    assert_eq!(answers, [OfferDecision::Accepted, OfferDecision::Accepted]);

    // Too large, then repeated: the rejection is repeated too.
    let mut big = spec(vec![helper("x", "lines|1|0")]);
    big.envelope =
        bella_core::resources::ResourceEnvelope::fixed(ResourceVector::new(1000, 500 * GIB, 0));
    let big = h.offer(big);
    let big_id = bella_core::CommandId::generate();
    let command = ControllerCommand::Offer {
        offer: Box::new(big.clone()),
    };
    h.ctl.command_as(&big_id, h.ctl.epoch, command.clone());
    let first = h.ctl.wait_reply(&big_id);
    h.ctl.command_as(&big_id, h.ctl.epoch, command);
    let answers = h.ctl.wait("two replies", |ctl| {
        let found: Vec<OfferDecision> = ctl
            .replies
            .iter()
            .filter(|(reply, _, _)| *reply == big_id)
            .map(|(_, _, decision)| decision.clone())
            .collect();
        (found.len() == 2).then_some(found)
    });
    assert!(matches!(first, OfferDecision::Rejected { .. }));
    assert_eq!(answers[0], answers[1]);

    // Revoking the held offer frees it; a start then finds nothing held.
    let revoke = h.ctl.command(ControllerCommand::Revoke {
        attempt: offer.attempt.clone(),
        reason: "not needed".to_string(),
    });
    assert_eq!(h.ctl.wait_ack(&revoke), CommandResult::Applied);
    match h.ctl.start(&offer.attempt, h.authority()) {
        CommandResult::Rejected { code, .. } => assert_eq!(code, RejectCode::OfferNotHeld),
        other => panic!("{other:?}"),
    }
}

#[test]
fn reconcile_verdicts_are_applied_before_anything_else() {
    let mut h = Harness::new();
    h.start_connected();
    let running = h.launch(spec(vec![helper("wait", "sleep|60")]));
    let held = h.offer(spec(vec![helper("x", "lines|1|0")]));
    assert_eq!(h.ctl.offer(&held), OfferDecision::Accepted);
    let kept = h.launch(spec(vec![helper("wait", "sleep|60")]));
    h.ctl
        .wait_step(&running.attempt.attempt_id, "wait", StepState::Running);
    h.ctl
        .wait_step(&kept.attempt.attempt_id, "wait", StepState::Running);

    // The controller restarts: a new connection with a newer epoch.
    h.ctl.drop_connection();
    h.ctl.epoch += 1;
    let hello = h.ctl.accept_hello();
    let reported: Vec<_> = hello
        .attempts
        .iter()
        .map(|r| r.attempt.attempt_id.clone())
        .collect();
    for attempt in [&running, &held, &kept] {
        assert!(reported.contains(&attempt.attempt.attempt_id));
    }
    let held_report = hello
        .attempts
        .iter()
        .find(|report| report.attempt == held.attempt)
        .unwrap();
    assert_eq!(held_report.state, LocalAttemptState::OfferHeld);
    h.ctl.welcome();

    // Before the verdicts nothing new is accepted.
    let early = h.offer(spec(vec![helper("x", "lines|1|0")]));
    match h.ctl.offer(&early) {
        OfferDecision::Rejected { code, .. } => {
            assert_eq!(code, QueueReasonCode::ControllerReconciling)
        }
        other => panic!("{other:?}"),
    }

    h.ctl.reconcile(vec![
        ReconcileVerdict {
            attempt: running.attempt.clone(),
            action: ReconcileAction::Abort,
        },
        ReconcileVerdict {
            attempt: held.attempt.clone(),
            action: ReconcileAction::Forget,
        },
        ReconcileVerdict {
            attempt: kept.attempt.clone(),
            action: ReconcileAction::Continue,
        },
    ]);
    h.ctl.wait_released(&running.attempt.attempt_id);
    assert_eq!(
        h.ctl.outcome(&running.attempt.attempt_id),
        Some(Outcome::Cancelled)
    );
    let forgotten = h
        .journal()
        .attempt(&held.attempt.attempt_id)
        .unwrap()
        .unwrap();
    assert_eq!(forgotten.state, LocalAttemptState::Released);
    assert!(forgotten.authority.is_none(), "it never started");
    // The continued attempt is still running.
    h.ctl.pump(Duration::from_millis(300));
    assert_eq!(h.ctl.outcome(&kept.attempt.attempt_id), None);
    assert_eq!(h.ctl.cancel(&kept.attempt, 0), CommandResult::Applied);
    h.ctl.wait_released(&kept.attempt.attempt_id);
}

#[test]
fn policy_updates_and_drain_reach_the_heartbeat() {
    let mut h = Harness::new();
    h.start_connected();
    let mut policy = h.ctl.policy.clone();
    policy.revision += 1;
    policy.accept_builds = false;
    let update = h.ctl.command(ControllerCommand::UpdatePolicy {
        policy: policy.clone(),
    });
    assert_eq!(h.ctl.wait_ack(&update), CommandResult::Applied);
    h.ctl.wait_heartbeat("the new policy revision", |beat| {
        beat.policy_revision == policy.revision && !beat.admitting
    });
    let drain = h
        .ctl
        .command(ControllerCommand::SetDrain { draining: true });
    assert_eq!(h.ctl.wait_ack(&drain), CommandResult::Applied);
    h.ctl.wait_heartbeat("draining", |beat| {
        beat.local_control.draining && !beat.admitting
    });
    assert_eq!(h.ctl.wait_ack(&drain), CommandResult::Applied);
    // The policy is journaled for the next start.
    h.stop().unwrap();
    let hello = h.start_connected();
    assert_eq!(hello.policy_revision, policy.revision);
}
