//! Restart, reboot, sleep and expiry.

mod common;

use bella_core::machine::{MachinePolicy, MachineProfile};
use bella_core::protocol::{AttemptEvent, ExecutionAuthority, LocalAttemptState, StepState};
use bella_core::resources::ResourceVector;
use bella_journal::{
    Enrollment, ExpiryReason, JournalError, RecoveryAction, RevokeOutcome, StartDecision,
};
use common::*;
use std::time::Duration;

#[test]
fn a16_attempts_from_another_boot_are_lost_and_their_scope_is_never_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    let attempt = offer.attempt.clone();
    let id = &attempt.attempt_id;
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
    }

    // The machine reboots. The same PIDs and group ids may now belong to
    // anything at all.
    clock.advance(Duration::from_secs(40));
    let journal = open(&path, &clock);
    let recovered = journal.recover(&boot(2)).unwrap();
    assert_eq!(recovered.len(), 1);
    let found = &recovered[0];
    assert_eq!(found.action, RecoveryAction::CleanUpAfterReboot);
    assert_eq!(found.found_state, LocalAttemptState::Running);
    assert_eq!(found.record.state, LocalAttemptState::LostAcrossBoot);
    assert_eq!(found.record.boot_id, boot(1));
    assert_eq!(
        found.record.scope, None,
        "nothing is left to adopt or signal"
    );
    assert_eq!(found.record.stale_scope, Some(scope("bella-attempt")));
    assert_eq!(
        found.record.workspace.as_deref(),
        Some("work/attempt"),
        "the workspace still has to be removed"
    );

    // Lost is not released: the capacity stays held until cleanup is done.
    assert_eq!(journal.held_resources().unwrap(), reservation());
    let report = &journal.reports().unwrap()[0];
    assert_eq!(report.state, LocalAttemptState::LostAcrossBoot);
    assert_eq!(report.reserved, reservation());

    // The lost attempt cannot be resumed or handed its old scope back.
    assert!(matches!(
        journal.set_scope(id, &scope("bella-attempt")),
        Err(JournalError::NotAllowed {
            state: LocalAttemptState::LostAcrossBoot,
            ..
        })
    ));
    for to in [
        LocalAttemptState::Running,
        LocalAttemptState::Collecting,
        LocalAttemptState::Released,
    ] {
        assert!(matches!(
            journal.set_state(id, to),
            Err(JournalError::InvalidState { .. })
        ));
    }
    assert!(matches!(
        journal.set_step_state(id, "test", StepState::Running, None),
        Err(JournalError::NotAllowed { .. })
    ));
    assert_eq!(
        journal.authorize_start(&attempt, authority(T0)).unwrap(),
        StartDecision::AlreadyAuthorized
    );

    // Workspace removed, then released with a proof resting on the reboot.
    journal.set_state(id, LocalAttemptState::Cleaning).unwrap();
    let mut reboot_proof = proof();
    reboot_proof.method = "machine rebooted".to_owned();
    journal.release(&attempt, reboot_proof).unwrap();
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
}

#[test]
fn a16_a_scope_from_this_boot_is_readopted_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
    }
    let journal = open(&path, &clock);
    let recovered = journal.recover(&boot(1)).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].action, RecoveryAction::Readopt);
    assert_eq!(recovered[0].record.state, LocalAttemptState::Running);
    assert_eq!(recovered[0].record.scope, Some(scope("bella-attempt")));
    assert_eq!(recovered[0].record.stale_scope, None);
    // Asking again gives the same answer.
    assert_eq!(journal.recover(&boot(1)).unwrap(), recovered);
}

#[test]
fn a16_attempts_cannot_change_before_recovery_has_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
    }
    // A new process, possibly a new boot: until recover() has compared boot
    // ids, the journaled scope must not be acted on.
    let journal = open(&path, &clock);
    let attempt = &offer.attempt;
    let id = &attempt.attempt_id;
    let gated: [Result<(), JournalError>; 9] = [
        journal.hold_offer(&common::offer(2)).map(|_| ()),
        journal.authorize_start(attempt, authority(T0)).map(|_| ()),
        journal.revoke(attempt).map(|_| ()),
        journal.set_scope(id, &scope("bella-attempt")),
        journal.set_state(id, LocalAttemptState::Collecting),
        journal.set_step_state(id, "test", StepState::Running, None),
        journal.request_cancel(attempt, "stop").map(|_| ()),
        journal
            .append_event(attempt, AttemptEvent::Collecting)
            .map(|_| ()),
        journal.expire(clock_now(&clock)).map(|_| ()),
    ];
    for (index, result) in gated.into_iter().enumerate() {
        assert!(
            matches!(result, Err(JournalError::NotRecovered)),
            "operation {index} ran before recovery"
        );
    }
    // Reading is always possible, and nothing changed.
    assert_eq!(state(&journal, attempt), LocalAttemptState::Running);
    assert_eq!(journal.held_resources().unwrap(), reservation());
    assert_eq!(journal.reports().unwrap().len(), 1);

    journal.recover(&boot(1)).unwrap();
    journal
        .set_state(id, LocalAttemptState::Collecting)
        .unwrap();
}

fn clock_now(clock: &bella_core::ManualClock) -> bella_core::UnixMillis {
    use bella_core::Clock;
    clock.now()
}

#[test]
fn a16_cleanup_started_after_a_reboot_is_readopted_within_that_boot_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    let id = offer.attempt.attempt_id.clone();
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
    }
    {
        // Boot 2: the lost attempt starts cleaning up in a new scope.
        let journal = open(&path, &clock);
        journal.recover(&boot(2)).unwrap();
        journal.set_state(&id, LocalAttemptState::Cleaning).unwrap();
        journal.set_scope(&id, &scope("bella-cleanup")).unwrap();
        journal
            .set_step_state(&id, "teardown", StepState::Running, None)
            .unwrap();
    }
    {
        // The worker restarts within boot 2: that scope is real and its
        // processes may be alive.
        let journal = open(&path, &clock);
        let recovered = journal.recover(&boot(2)).unwrap();
        assert_eq!(recovered[0].action, RecoveryAction::Readopt);
        assert_eq!(recovered[0].record.state, LocalAttemptState::Cleaning);
        assert_eq!(recovered[0].record.scope, Some(scope("bella-cleanup")));
    }
    // Boot 3: the cleanup scope is gone with the reboot as well.
    let journal = open(&path, &clock);
    let recovered = journal.recover(&boot(3)).unwrap();
    assert_eq!(recovered[0].action, RecoveryAction::CleanUpAfterReboot);
    assert_eq!(recovered[0].found_state, LocalAttemptState::Cleaning);
    assert_eq!(recovered[0].record.state, LocalAttemptState::LostAcrossBoot);
    assert_eq!(recovered[0].record.scope, None);
    assert_eq!(
        recovered[0].record.stale_scope,
        Some(scope("bella-cleanup"))
    );
    // Still lost when the worker restarts once more in boot 3.
    drop(journal);
    let journal = open(&path, &clock);
    let again = journal.recover(&boot(3)).unwrap();
    assert_eq!(again[0].action, RecoveryAction::CleanUpAfterReboot);
    assert_eq!(again[0].found_state, LocalAttemptState::LostAcrossBoot);
}

#[test]
fn a16_a_quarantined_attempt_is_lost_too_once_the_machine_rebooted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    let id = offer.attempt.attempt_id.clone();
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        run(&journal, &offer, authority(T0));
        journal.set_state(&id, LocalAttemptState::Cleaning).unwrap();
        journal
            .set_state(&id, LocalAttemptState::Quarantined)
            .unwrap();
    }
    let journal = open(&path, &clock);
    let recovered = journal.recover(&boot(2)).unwrap();
    assert_eq!(recovered[0].found_state, LocalAttemptState::Quarantined);
    assert_eq!(recovered[0].action, RecoveryAction::CleanUpAfterReboot);
    assert_eq!(journal.held_resources().unwrap(), reservation());
}

#[test]
fn recovery_sorts_offers_and_unspawned_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let fresh = offer_with(1, 1, reservation(), T0.plus_secs(60));
    let expiring = offer_with(2, 1, reservation(), T0.plus_secs(15));
    let authorised = offer_with(3, 1, reservation(), T0.plus_secs(15));
    let preparing = offer_with(4, 1, reservation(), T0.plus_secs(15));
    let released = offer_with(5, 1, reservation(), T0.plus_secs(15));
    {
        let journal = open(&path, &clock);
        assert!(journal.recover(&boot(1)).unwrap().is_empty());
        journal.hold_offer(&fresh).unwrap();
        journal.hold_offer(&expiring).unwrap();
        start(&journal, &authorised, authority(T0));
        let attempt = start(&journal, &preparing, authority(T0));
        journal
            .set_workspace(&attempt.attempt_id, "work/four")
            .unwrap();
        journal
            .set_state(&attempt.attempt_id, LocalAttemptState::Preparing)
            .unwrap();
        journal.hold_offer(&released).unwrap();
        journal.revoke(&released.attempt).unwrap();
    }

    // The worker restarts 20 s later, in the same boot.
    clock.advance(Duration::from_secs(20));
    let journal = open(&path, &clock);
    let recovered = journal.recover(&boot(1)).unwrap();
    let actions: Vec<_> = recovered
        .iter()
        .map(|found| (found.record.attempt.clone(), found.action))
        .collect();
    assert_eq!(
        actions,
        vec![
            (fresh.attempt.clone(), RecoveryAction::KeepOffer),
            (expiring.attempt.clone(), RecoveryAction::DropOffer),
            (authorised.attempt.clone(), RecoveryAction::ResumeUnspawned),
            (preparing.attempt.clone(), RecoveryAction::ResumeUnspawned),
        ],
        "released attempts are not part of recovery"
    );
    assert_eq!(recovered[1].record.state, LocalAttemptState::Released);
    assert_eq!(recovered[1].found_state, LocalAttemptState::OfferHeld);
    assert_eq!(recovered[3].record.workspace.as_deref(), Some("work/four"));
    // Three still hold; the dropped offer does not.
    assert_eq!(journal.active_attempts().unwrap().len(), 3);

    // The kept offer can still be started, the dropped one cannot.
    assert_eq!(
        journal
            .authorize_start(&fresh.attempt, authority(T0.plus_secs(20)))
            .unwrap(),
        StartDecision::Fresh
    );
    assert!(matches!(
        journal.authorize_start(&expiring.attempt, authority(T0.plus_secs(20))),
        Err(JournalError::OfferNotHeld { .. })
    ));
    assert_eq!(
        journal.revoke(&expiring.attempt).unwrap(),
        RevokeOutcome::AlreadyReleased
    );
}

#[test]
fn an_unstarted_offer_does_not_survive_a_reboot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer_with(1, 1, reservation(), T0.plus_secs(600));
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        journal.hold_offer(&offer).unwrap();
    }
    // Not expired by the clock, but accepted before a reboot.
    let journal = open(&path, &clock);
    let recovered = journal.recover(&boot(2)).unwrap();
    assert_eq!(recovered[0].action, RecoveryAction::DropOffer);
    assert_eq!(recovered[0].record.state, LocalAttemptState::Released);
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
    assert!(journal.recover(&boot(2)).unwrap().is_empty());
}

#[test]
fn a17_authority_runs_out_at_valid_until_plus_the_disconnected_allowance() {
    let clock = clock();
    let journal = memory(&clock);
    // valid until +60 s, 120 s of disconnected running, hard deadline +1 h.
    let attempt = run(&journal, &offer(1), authority(T0));

    assert_eq!(journal.expire(clock_now(&clock)).unwrap().stop, vec![]);
    clock.advance(Duration::from_millis(179_999));
    let report = journal.expire(clock_now(&clock)).unwrap();
    assert!(report.stop.is_empty(), "still inside the allowance");
    journal
        .set_step_state(&attempt.attempt_id, "build", StepState::Running, None)
        .unwrap();

    clock.advance(Duration::from_millis(1));
    let report = journal.expire(clock_now(&clock)).unwrap();
    assert_eq!(report.stop.len(), 1);
    assert_eq!(report.stop[0].attempt, attempt);
    assert_eq!(report.stop[0].reason, ExpiryReason::AuthorityLapsed);
    assert_eq!(report.stop[0].state, LocalAttemptState::Running);
    assert_eq!(report.stop[0].authority, authority(T0));
    let record = journal.attempt(&attempt.attempt_id).unwrap().unwrap();
    assert_eq!(record.cancel.unwrap().requested_at, T0.plus_secs(180));
    // Expiry asks for a stop; it frees nothing by itself.
    assert_eq!(record.state, LocalAttemptState::Running);
    assert_eq!(journal.held_resources().unwrap(), reservation());
}

#[test]
fn a17_a_renewal_in_time_keeps_the_attempt_running() {
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    for _ in 0..30 {
        clock.advance(Duration::from_secs(10));
        let now = clock_now(&clock);
        journal
            .renew_authority(
                &attempt,
                ExecutionAuthority {
                    valid_until: now.plus_secs(60),
                    ..authority(T0)
                },
            )
            .unwrap();
        assert!(journal.expire(now).unwrap().stop.is_empty());
    }
    // Five minutes in, well past the first window, and still authorised.
    journal
        .set_step_state(&attempt.attempt_id, "test", StepState::Running, None)
        .unwrap();
    assert!(
        journal
            .attempt(&attempt.attempt_id)
            .unwrap()
            .unwrap()
            .cancel
            .is_none()
    );
}

#[test]
fn a17_after_a_sleep_nothing_new_runs_under_the_stale_authority() {
    let clock = clock();
    let journal = memory(&clock);
    let running = run(&journal, &offer(1), authority(T0));
    let authorised = start(&journal, &offer(2), authority(T0));
    let held = offer_with(3, 1, reservation(), T0.plus_secs(15));
    journal.hold_offer(&held).unwrap();
    let long_offer = offer_with(4, 1, reservation(), T0.plus_secs(24 * 3600));
    journal.hold_offer(&long_offer).unwrap();
    let all_four = (0..4).fold(ResourceVector::ZERO, |sum, _| {
        sum.saturating_add(&reservation())
    });
    assert_eq!(journal.held_resources().unwrap(), all_four);

    // The lid closes. Eight hours later the machine wakes: no renewal was
    // received, and the wall clock is what counts.
    clock.advance(Duration::from_secs(8 * 3600));
    let now = clock_now(&clock);

    // Even before the worker gets round to expire(), the journal refuses
    // every step forward under the dead authority.
    assert!(matches!(
        journal.set_step_state(&running.attempt_id, "test", StepState::Running, None),
        Err(JournalError::AuthorityExpired { .. })
    ));
    assert!(matches!(
        journal.set_state(&running.attempt_id, LocalAttemptState::EnvironmentHeld),
        Err(JournalError::AuthorityExpired { .. })
    ));
    assert!(matches!(
        journal.set_state(&authorised.attempt_id, LocalAttemptState::Preparing),
        Err(JournalError::AuthorityExpired { .. })
    ));
    // A Start that was queued before the sleep, for an offer still on hold.
    assert!(matches!(
        journal.authorize_start(&long_offer.attempt, authority(T0)),
        Err(JournalError::AuthorityExpired { .. })
    ));
    assert_eq!(
        state(&journal, &long_offer.attempt),
        LocalAttemptState::OfferHeld
    );
    assert!(matches!(
        journal.authorize_start(&held.attempt, authority(now)),
        Err(JournalError::OfferNotHeld { .. })
    ));
    // Past the hard deadline no renewal helps either.
    assert!(matches!(
        journal.renew_authority(
            &running,
            ExecutionAuthority {
                valid_until: now.plus_secs(60),
                ..authority(T0)
            }
        ),
        Err(JournalError::AuthorityExpired { .. })
    ));

    let report = journal.expire(now).unwrap();
    let stopped: Vec<_> = report
        .stop
        .iter()
        .map(|expired| (expired.attempt.clone(), expired.reason))
        .collect();
    assert_eq!(
        stopped,
        vec![
            (running.clone(), ExpiryReason::HardDeadline),
            (authorised.clone(), ExpiryReason::HardDeadline),
        ]
    );
    assert_eq!(report.released_offers, vec![held.attempt.clone()]);
    assert_eq!(state(&journal, &held.attempt), LocalAttemptState::Released);

    // Stopping is requested, not assumed: the two started attempts hold
    // their resources until the worker has cleaned up, and so does the
    // offer that has not expired.
    let three = reservation()
        .saturating_add(&reservation())
        .saturating_add(&reservation());
    assert_eq!(journal.held_resources().unwrap(), three);
    for attempt in [&running, &authorised] {
        let record = journal.attempt(&attempt.attempt_id).unwrap().unwrap();
        let cancel = record.cancel.expect("a stop request is journaled");
        assert_eq!(cancel.requested_at, now);
        assert!(cancel.reason.contains("hard deadline"), "{}", cancel.reason);
    }

    // The way out is cleanup, where cleanup steps still may run.
    journal
        .set_state(&running.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    journal
        .set_step_state(&running.attempt_id, "teardown", StepState::Running, None)
        .unwrap();
    journal
        .set_state(&authorised.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    // Once they are cleaning up, expire() has nothing more to ask.
    assert_eq!(journal.expire(now).unwrap().stop, vec![]);
    journal.release(&running, proof()).unwrap();
    journal.release(&authorised, proof()).unwrap();
    assert_eq!(journal.held_resources().unwrap(), reservation());
}

#[test]
fn a17_a_stop_decided_on_wake_stands_even_if_the_clock_is_set_back() {
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    clock.advance(Duration::from_secs(600));
    assert_eq!(journal.expire(clock_now(&clock)).unwrap().stop.len(), 1);

    // Time is corrected backwards and a renewal arrives after all.
    clock.set(T0.plus_secs(30));
    journal
        .renew_authority(
            &attempt,
            ExecutionAuthority {
                valid_until: T0.plus_secs(3000),
                ..authority(T0)
            },
        )
        .unwrap();
    assert!(matches!(
        journal.set_step_state(&attempt.attempt_id, "test", StepState::Running, None),
        Err(JournalError::CancelRequested { .. })
    ));
    assert!(matches!(
        journal.set_state(&attempt.attempt_id, LocalAttemptState::EnvironmentHeld),
        Err(JournalError::CancelRequested { .. })
    ));
}

#[test]
fn a17_the_machine_policy_caps_disconnected_running() {
    let clock = clock();
    let journal = memory(&clock);
    let mut policy = MachinePolicy::defaults_for(MachineProfile::PortableBuilder);
    policy.max_disconnected_secs = 30;
    journal
        .set_enrollment(&Enrollment {
            policy,
            ..enrollment()
        })
        .unwrap();
    // The authority asks for 120 s; the owner allows 30 s.
    let attempt = run(&journal, &offer(1), authority(T0));
    clock.advance(Duration::from_millis(89_999));
    assert!(journal.expire(clock_now(&clock)).unwrap().stop.is_empty());
    clock.advance(Duration::from_millis(1));
    let report = journal.expire(clock_now(&clock)).unwrap();
    assert_eq!(report.stop[0].attempt, attempt);
    assert_eq!(report.stop[0].reason, ExpiryReason::AuthorityLapsed);
}

#[test]
fn expire_releases_only_offers_that_never_started() {
    let clock = clock();
    let journal = memory(&clock);
    let short = offer_with(1, 1, reservation(), T0.plus_secs(15));
    let long = offer_with(2, 1, reservation(), T0.plus_secs(40));
    journal.hold_offer(&short).unwrap();
    journal.hold_offer(&long).unwrap();
    let started = start(
        &journal,
        &offer_with(3, 1, reservation(), T0.plus_secs(15)),
        authority(T0),
    );

    clock.advance(Duration::from_millis(14_999));
    assert_eq!(
        journal.expire(clock_now(&clock)).unwrap(),
        Default::default()
    );
    clock.advance(Duration::from_millis(1));
    let report = journal.expire(clock_now(&clock)).unwrap();
    assert_eq!(report.released_offers, vec![short.attempt.clone()]);
    assert!(report.stop.is_empty());
    let record = journal.attempt(&short.attempt.attempt_id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::Released);
    assert_eq!(record.released_at, Some(T0.plus_secs(15)));
    assert_eq!(state(&journal, &long.attempt), LocalAttemptState::OfferHeld);
    // The started attempt's offer expiry no longer matters.
    assert_eq!(
        state(&journal, &started),
        LocalAttemptState::StartAuthorized
    );
    assert_eq!(
        journal.held_resources().unwrap(),
        reservation().saturating_add(&reservation())
    );
    // Nothing is released twice.
    assert_eq!(
        journal.expire(clock_now(&clock)).unwrap(),
        Default::default()
    );
}

#[test]
fn garbage_collection_removes_only_finished_acknowledged_attempts() {
    let clock = clock();
    let journal = memory(&clock);
    let keep = Duration::from_secs(3600);

    // Released and fully acknowledged.
    let done = run(&journal, &offer(1), authority(T0));
    journal
        .set_state(&done.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    journal.release(&done, proof()).unwrap();
    journal.ack_events(&done.attempt_id, 1).unwrap();
    // Released, but the controller has not confirmed the Released event.
    let unconfirmed = run(&journal, &offer(2), authority(T0));
    journal
        .set_state(&unconfirmed.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    journal.release(&unconfirmed, proof()).unwrap();
    // Released and events confirmed, but log bytes are still owed.
    let logs_owed = run(&journal, &offer(3), authority(T0));
    journal
        .record_log_produced(
            &logs_owed.attempt_id,
            "build",
            bella_core::protocol::LogStream::Stdout,
            100,
        )
        .unwrap();
    journal
        .set_state(&logs_owed.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    journal.release(&logs_owed, proof()).unwrap();
    journal.ack_events(&logs_owed.attempt_id, 1).unwrap();
    // Revoked before it started: no events at all.
    let revoked = offer(4);
    journal.hold_offer(&revoked).unwrap();
    journal.revoke(&revoked.attempt).unwrap();
    // Still running.
    let active = run(&journal, &offer(5), authority(T0));

    // Too young to collect.
    clock.advance(Duration::from_secs(3599));
    assert!(
        journal
            .garbage_collect(clock_now(&clock), keep)
            .unwrap()
            .is_empty()
    );
    clock.advance(Duration::from_secs(1));
    let removed = journal.garbage_collect(clock_now(&clock), keep).unwrap();
    assert_eq!(
        removed,
        vec![done.attempt_id.clone(), revoked.attempt.attempt_id.clone()]
    );
    assert!(journal.attempt(&done.attempt_id).unwrap().is_none());
    let left: Vec<_> = journal
        .attempts()
        .unwrap()
        .into_iter()
        .map(|record| record.attempt)
        .collect();
    assert_eq!(
        left,
        vec![unconfirmed.clone(), logs_owed.clone(), active.clone()]
    );
    assert_eq!(journal.held_resources().unwrap(), reservation());

    // The controller disowned one of them: its acknowledgement will never
    // come, so the worker forgets it explicitly. A live attempt cannot be
    // forgotten.
    journal.forget_attempt(&unconfirmed.attempt_id).unwrap();
    assert!(journal.attempt(&unconfirmed.attempt_id).unwrap().is_none());
    assert!(matches!(
        journal.forget_attempt(&active.attempt_id),
        Err(JournalError::NotAllowed {
            state: LocalAttemptState::Running,
            ..
        })
    ));
    assert_eq!(journal.held_resources().unwrap(), reservation());
}
