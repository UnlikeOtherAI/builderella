//! The reconciliation rules of `docs/architecture.md` §6.5, one by one.

use crate::common::{Fixture, boot, exited, fixture, spec, unknown_attempt, vector};
use bella_core::ids::{JobId, MachineId};
use bella_core::job::{JobPhase, Outcome, RetryClass};
use bella_core::protocol::{
    AttemptEvent, AttemptRef, AttemptReport, LocalAttemptState, ReconcileAction,
};
use bella_ledger::LedgerError;

fn report(attempt: &AttemptRef, job: &JobId, state: LocalAttemptState) -> AttemptReport {
    AttemptReport {
        attempt: attempt.clone(),
        job_id: job.clone(),
        state,
        last_event_seq: 0,
        reserved: vector(1000, 1),
        usage: None,
    }
}

/// The worker reconnects on the same boot and reports `reports`.
fn reconnect(f: &Fixture, machine: &MachineId, reports: &[AttemptReport]) -> Vec<ReconcileAction> {
    reconnect_on(f, machine, 1, reports)
}

fn reconnect_on(
    f: &Fixture,
    machine: &MachineId,
    boot_number: u32,
    reports: &[AttemptReport],
) -> Vec<ReconcileAction> {
    let generation = f
        .ledger
        .machine_status(machine)
        .unwrap()
        .connection_generation;
    f.ledger.machine_disconnected(machine, generation).unwrap();
    f.ledger.machine_connected(machine, generation + 1).unwrap();
    let verdicts = f
        .ledger
        .reconcile(machine, &boot(boot_number), reports)
        .unwrap();
    assert_eq!(verdicts.len(), reports.len());
    for (verdict, report) in verdicts.iter().zip(reports) {
        assert_eq!(verdict.attempt, report.attempt);
    }
    verdicts.into_iter().map(|verdict| verdict.action).collect()
}

#[test]
fn a_current_started_attempt_continues() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing]);
    f.ledger.mark_machine_uncertain(&machine).unwrap();

    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::Running)],
    );
    assert_eq!(actions, [ReconcileAction::Continue]);
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert!(record.held);
    assert!(!record.uncertain, "contact is restored");
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Preparing);
    assert!(f.ledger.machine_status(&machine).unwrap().reconciled);
}

#[test]
fn a_start_that_never_arrived_can_be_sent_again() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    // The worker still only holds the offer.
    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::OfferHeld)],
    );
    assert_eq!(actions, [ReconcileAction::Continue]);
    let permit = f.ledger.commit_start(&attempt, f.ledger.epoch()).unwrap();
    assert!(permit.already_committed);
    assert_eq!(f.held(&machine), vector(1000, 1));
}

#[test]
fn an_offer_without_a_start_permit_is_revoked_and_forgotten() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);

    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::OfferHeld)],
    );
    assert_eq!(actions, [ReconcileAction::Forget]);
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert_eq!(record.release_reason.as_deref(), Some("forgotten"));
    assert!(matches!(
        f.ledger.commit_start(&attempt, f.ledger.epoch()),
        Err(LedgerError::InvalidState(_))
    ));
    // Reported again later, it is still to be forgotten.
    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::OfferHeld)],
    );
    assert_eq!(actions, [ReconcileAction::Forget]);
}

#[test]
fn an_unknown_attempt_is_aborted() {
    let f = fixture();
    let machine = f.machine("builder");
    let other = f.machine("other");
    let job = f.submit("app");
    let elsewhere = f.start(&job, &other);

    let actions = reconnect(
        &f,
        &machine,
        &[
            report(
                &unknown_attempt(),
                &JobId::generate(),
                LocalAttemptState::Running,
            ),
            // An attempt that belongs to a different machine.
            report(&elsewhere, &job, LocalAttemptState::Running),
        ],
    );
    assert_eq!(actions, [ReconcileAction::Abort, ReconcileAction::Abort]);
    // The other machine's attempt is untouched.
    assert_eq!(f.held(&other), vector(1000, 1));
    assert!(f.ledger.attempt(&elsewhere.attempt_id).unwrap().held);
}

#[test]
fn a_stale_generation_is_aborted() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let old = f.offer(&job, &machine);
    f.ledger.revoke_offer(&old, "rescheduled").unwrap();
    let current = f.start(&job, &machine);

    let actions = reconnect(
        &f,
        &machine,
        &[
            report(&old, &job, LocalAttemptState::Running),
            report(&current, &job, LocalAttemptState::Running),
            // The right attempt id with the wrong generation.
            report(
                &AttemptRef {
                    generation: 1,
                    ..current.clone()
                },
                &job,
                LocalAttemptState::Running,
            ),
        ],
    );
    assert_eq!(
        actions,
        [
            ReconcileAction::Abort,
            ReconcileAction::Continue,
            ReconcileAction::Abort
        ]
    );
    assert_eq!(f.held(&machine), vector(1000, 1));
}

#[test]
fn a_cancel_requested_attempt_is_aborted_and_stays_held() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing]);
    f.ledger.request_cancel(&job).unwrap();

    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::Running)],
    );
    assert_eq!(actions, [ReconcileAction::Abort]);
    // Aborting is a command to the worker; the capacity returns on proof.
    assert_eq!(f.held(&machine), vector(1000, 1));
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Preparing);
}

#[test]
fn a_released_attempt_that_is_reported_again_is_aborted() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.finish(&attempt, Outcome::Succeeded);
    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::Cleaning)],
    );
    assert_eq!(actions, [ReconcileAction::Abort]);
    assert_eq!(
        f.ledger.job(&job).unwrap().outcome,
        Some(Outcome::Succeeded)
    );
}

#[test]
fn an_unreported_attempt_is_released_when_the_machine_rebooted() {
    let f = fixture();
    let machine = f.machine("builder");
    let retried = f.submit("app");
    let mut external = spec();
    external.retry_class = RetryClass::ExternalSideEffects;
    let not_retried = f.submit_spec("app", external);
    let finished = f.submit("app");

    let first = f.start(&retried, &machine);
    f.events(&first, 1, vec![AttemptEvent::Preparing]);
    let second = f.start(&not_retried, &machine);
    f.events(&second, 1, vec![AttemptEvent::Preparing]);
    // This one had already reported its result and was cleaning up.
    let third = f.start(&finished, &machine);
    f.events(
        &third,
        1,
        vec![AttemptEvent::Preparing, exited(Outcome::Succeeded)],
    );
    assert_eq!(f.held(&machine), vector(3000, 3));

    assert!(reconnect_on(&f, &machine, 2, &[]).is_empty());
    assert!(
        f.held(&machine).is_zero(),
        "its processes cannot have survived"
    );
    for attempt in [&first, &second, &third] {
        let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
        assert_eq!(record.release_reason.as_deref(), Some("reboot"));
    }
    // Repeatable work is queued for a fresh attempt.
    let view = f.ledger.job(&retried).unwrap();
    assert_eq!(view.phase, JobPhase::Queued);
    assert!(view.outcome.is_none());
    assert!(matches!(
        view.attempts[0].outcome,
        Some(Outcome::InfraInterrupted { .. })
    ));
    // Work with external side effects is finished as interrupted.
    let view = f.ledger.job(&not_retried).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert!(matches!(
        view.outcome,
        Some(Outcome::InfraInterrupted { .. })
    ));
    // A result reported before the reboot stands.
    let view = f.ledger.job(&finished).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(Outcome::Succeeded));
    assert_eq!(
        f.ledger.machine_status(&machine).unwrap().boot_id,
        Some(boot(2))
    );
}

#[test]
fn an_unreported_attempt_is_released_when_its_start_was_never_delivered() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    // The start was committed, but no acknowledgement or event ever came.
    let attempt = f.start(&job, &machine);

    assert!(reconnect(&f, &machine, &[]).is_empty());
    assert!(f.held(&machine).is_zero());
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert_eq!(record.release_reason.as_deref(), Some("never_delivered"));
    assert!(matches!(
        record.outcome,
        Some(Outcome::InfraInterrupted { .. })
    ));
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
}

#[test]
fn an_unreported_delivered_attempt_stays_held_and_uncertain() {
    let f = fixture();
    let machine = f.machine("builder");
    let acknowledged = f.submit("app");
    let reported_progress = f.submit("app");
    let first = f.start(&acknowledged, &machine);
    assert!(f.ledger.record_start_delivered(&first).unwrap());
    let second = f.start(&reported_progress, &machine);
    f.events(&second, 1, vec![AttemptEvent::Preparing]);

    // Same boot, and the worker does not account for either attempt.
    assert!(reconnect(&f, &machine, &[]).is_empty());
    assert_eq!(f.held(&machine), vector(2000, 2));
    for attempt in [&first, &second] {
        let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
        assert!(record.held && record.uncertain);
        assert!(record.outcome.is_none());
    }
    assert_eq!(
        f.ledger.job(&acknowledged).unwrap().phase,
        JobPhase::Offered
    );
    assert_eq!(
        f.ledger.job(&reported_progress).unwrap().phase,
        JobPhase::Preparing
    );

    // Reported the next time, the uncertainty clears.
    let actions = reconnect(
        &f,
        &machine,
        &[report(
            &second,
            &reported_progress,
            LocalAttemptState::Running,
        )],
    );
    assert_eq!(actions, [ReconcileAction::Continue]);
    assert!(!f.ledger.attempt(&second.attempt_id).unwrap().uncertain);
    assert!(f.ledger.attempt(&first.attempt_id).unwrap().uncertain);
}

#[test]
fn an_unreported_offer_is_released() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    assert!(reconnect(&f, &machine, &[]).is_empty());
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
    assert!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .outcome
            .is_none()
    );
}

#[test]
fn reconciling_an_unknown_machine_fails() {
    let f = fixture();
    assert!(matches!(
        f.ledger.reconcile(&MachineId::generate(), &boot(1), &[]),
        Err(LedgerError::NotFound(_))
    ));
}

#[test]
fn a_quarantined_offer_is_released_only_by_proof_or_reboot() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    // The worker could not verify that its hold on the offer left nothing.
    f.events(
        &attempt,
        1,
        vec![AttemptEvent::Quarantined {
            reason: "workspace could not be removed".into(),
        }],
    );
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Quarantined);

    // None of the ways an ordinary offer goes away applies.
    f.advance_secs(3600);
    assert!(f.ledger.expire_offers(f.now()).unwrap().is_empty());
    assert!(matches!(
        f.ledger.revoke_offer(&attempt, "tidy up"),
        Err(LedgerError::InvalidState(_))
    ));
    assert!(matches!(
        f.ledger.request_cancel(&job).unwrap(),
        bella_ledger::CancelOutcome::Requested { .. }
    ));
    let actions = reconnect(
        &f,
        &machine,
        &[report(&attempt, &job, LocalAttemptState::Quarantined)],
    );
    assert_eq!(actions, [ReconcileAction::Abort]);
    assert!(reconnect(&f, &machine, &[]).is_empty());
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert!(record.held && record.uncertain);
    assert_eq!(f.held(&machine), vector(1000, 1));

    // A reboot proves nothing of it is left.
    assert!(reconnect_on(&f, &machine, 2, &[]).is_empty());
    assert!(f.held(&machine).is_zero());
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(Outcome::Cancelled));
}
