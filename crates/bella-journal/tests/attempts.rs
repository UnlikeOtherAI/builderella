//! Offers, start permits and local state changes.

mod common;

use bella_core::job::Outcome;
use bella_core::protocol::{
    AttemptEvent, CommandResult, ExecutionAuthority, LocalAttemptState, RejectCode, StepState,
    UsageSummary,
};
use bella_core::resources::ResourceVector;
use bella_journal::{
    HoldOutcome, JournalError, RevokeOutcome, StartDecision, can_transition, local_stop_deadline,
};
use common::*;
use std::time::Duration;

const ALL_STATES: [LocalAttemptState; 10] = [
    LocalAttemptState::OfferHeld,
    LocalAttemptState::StartAuthorized,
    LocalAttemptState::Preparing,
    LocalAttemptState::Running,
    LocalAttemptState::EnvironmentHeld,
    LocalAttemptState::Collecting,
    LocalAttemptState::Cleaning,
    LocalAttemptState::Released,
    LocalAttemptState::Quarantined,
    LocalAttemptState::LostAcrossBoot,
];

#[test]
fn a11_retransmitted_start_is_authorised_exactly_once() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    assert_eq!(journal.hold_offer(&offer).unwrap(), HoldOutcome::Held);

    let permit = authority(T0);
    assert_eq!(
        journal.authorize_start(&offer.attempt, permit).unwrap(),
        StartDecision::Fresh
    );
    // The same Start again, and one carrying a different authority.
    assert_eq!(
        journal.authorize_start(&offer.attempt, permit).unwrap(),
        StartDecision::AlreadyAuthorized
    );
    let other = ExecutionAuthority {
        valid_until: T0.plus_secs(600),
        ..permit
    };
    assert_eq!(
        journal.authorize_start(&offer.attempt, other).unwrap(),
        StartDecision::AlreadyAuthorized
    );
    let record = journal.attempt(&offer.attempt.attempt_id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::StartAuthorized);
    assert_eq!(
        record.authority,
        Some(permit),
        "a retransmission does not replace the permit"
    );
    assert_eq!(record.authorized_at, Some(T0));

    // However far the attempt has come, a late Start never reads as fresh.
    let id = &offer.attempt.attempt_id;
    journal.set_state(id, LocalAttemptState::Preparing).unwrap();
    journal.set_scope(id, &scope("bella-a11")).unwrap();
    journal.set_state(id, LocalAttemptState::Running).unwrap();
    assert_eq!(
        journal.authorize_start(&offer.attempt, permit).unwrap(),
        StartDecision::AlreadyAuthorized
    );
    journal.set_state(id, LocalAttemptState::Cleaning).unwrap();
    journal.release(&offer.attempt, proof()).unwrap();
    assert_eq!(
        journal.authorize_start(&offer.attempt, permit).unwrap(),
        StartDecision::AlreadyAuthorized
    );
    assert_eq!(state(&journal, &offer.attempt), LocalAttemptState::Released);
}

#[test]
fn a11_start_permit_survives_a_restart_and_is_not_fresh_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let offer = offer(1);
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        journal.hold_offer(&offer).unwrap();
        assert_eq!(
            journal
                .authorize_start(&offer.attempt, authority(T0))
                .unwrap(),
            StartDecision::Fresh
        );
    }
    // The worker died before acknowledging; the controller sends Start again.
    let journal = open(&path, &clock);
    journal.recover(&boot(1)).unwrap();
    assert_eq!(
        journal
            .authorize_start(&offer.attempt, authority(T0))
            .unwrap(),
        StartDecision::AlreadyAuthorized
    );
}

#[test]
fn a13_revoked_offer_can_never_start() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    assert_eq!(journal.held_resources().unwrap(), reservation());

    assert_eq!(
        journal.revoke(&offer.attempt).unwrap(),
        RevokeOutcome::Revoked
    );
    let record = journal.attempt(&offer.attempt.attempt_id).unwrap().unwrap();
    assert_eq!(record.state, LocalAttemptState::Released);
    assert_eq!(record.released_at, Some(T0));
    assert_eq!(record.authority, None);
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
    assert!(journal.active_attempts().unwrap().is_empty());

    // The Start that crossed the Revoke on the wire.
    let refused = journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap_err();
    assert!(matches!(refused, JournalError::OfferNotHeld { .. }));
    assert_eq!(refused.reject_code(), Some(RejectCode::OfferNotHeld));
    assert_eq!(state(&journal, &offer.attempt), LocalAttemptState::Released);

    // A repeated revoke is answered, and a repeated offer does not revive it.
    assert_eq!(
        journal.revoke(&offer.attempt).unwrap(),
        RevokeOutcome::AlreadyReleased
    );
    assert!(matches!(
        journal.hold_offer(&offer),
        Err(JournalError::OfferNotHeld { .. })
    ));
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
}

#[test]
fn a13_revoke_is_refused_once_a_start_permit_exists() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    let attempt = start(&journal, &offer, authority(T0));
    let refused = journal.revoke(&attempt).unwrap_err();
    assert!(matches!(
        refused,
        JournalError::InvalidState {
            from: LocalAttemptState::StartAuthorized,
            to: LocalAttemptState::Released,
        }
    ));
    assert_eq!(refused.reject_code(), Some(RejectCode::InvalidState));
    assert_eq!(
        state(&journal, &attempt),
        LocalAttemptState::StartAuthorized
    );
    assert_eq!(journal.held_resources().unwrap(), reservation());
}

#[test]
fn a_repeated_offer_is_a_duplicate_and_old_generations_are_stale() {
    let clock = clock();
    let journal = memory(&clock);
    let second = offer_with(7, 2, reservation(), T0.plus_secs(15));
    assert_eq!(journal.hold_offer(&second).unwrap(), HoldOutcome::Held);
    assert_eq!(journal.hold_offer(&second).unwrap(), HoldOutcome::Duplicate);
    assert_eq!(
        journal.held_resources().unwrap(),
        reservation(),
        "a duplicate offer reserves nothing extra"
    );

    // An older attempt of the same job arrives late.
    let first = offer_with(7, 1, reservation(), T0.plus_secs(15));
    let stale = journal.hold_offer(&first).unwrap_err();
    assert!(matches!(
        stale,
        JournalError::StaleGeneration {
            held: 2,
            offered: 1
        }
    ));
    assert_eq!(stale.reject_code(), Some(RejectCode::StaleGeneration));
    assert!(
        journal
            .attempt(&first.attempt.attempt_id)
            .unwrap()
            .is_none()
    );

    // A different attempt cannot claim a generation that is taken.
    let mut imposter = offer_with(7, 2, reservation(), T0.plus_secs(15));
    imposter.attempt.attempt_id = attempt_ref(99, 2).attempt_id;
    assert!(matches!(
        journal.hold_offer(&imposter),
        Err(JournalError::GenerationConflict { .. })
    ));

    // The same attempt id named with other generations.
    let mut older = second.attempt.clone();
    older.generation = 1;
    assert!(matches!(
        journal.authorize_start(&older, authority(T0)),
        Err(JournalError::StaleGeneration {
            held: 2,
            offered: 1
        })
    ));
    assert!(matches!(
        journal.revoke(&older),
        Err(JournalError::StaleGeneration { .. })
    ));
    let mut newer = second.attempt.clone();
    newer.generation = 3;
    assert!(matches!(
        journal.authorize_start(&newer, authority(T0)),
        Err(JournalError::GenerationConflict { .. })
    ));
    assert_eq!(
        state(&journal, &second.attempt),
        LocalAttemptState::OfferHeld
    );

    // A newer generation of the job is accepted next to the older attempt.
    let third = offer_with(7, 3, reservation(), T0.plus_secs(15));
    assert_eq!(journal.hold_offer(&third).unwrap(), HoldOutcome::Held);
}

#[test]
fn an_unknown_attempt_is_reported_as_such() {
    let clock = clock();
    let journal = memory(&clock);
    let nobody = attempt_ref(404, 1);
    let missing = journal.authorize_start(&nobody, authority(T0)).unwrap_err();
    assert!(matches!(missing, JournalError::NotFound(_)));
    assert_eq!(missing.reject_code(), Some(RejectCode::UnknownAttempt));
    assert!(matches!(
        missing.command_result(),
        Some(CommandResult::Rejected {
            code: RejectCode::UnknownAttempt,
            ..
        })
    ));
    assert!(matches!(
        journal.set_state(&nobody.attempt_id, LocalAttemptState::Cleaning),
        Err(JournalError::NotFound(_))
    ));
    assert!(journal.attempt(&nobody.attempt_id).unwrap().is_none());
}

#[test]
fn a_start_after_the_offer_expired_is_refused() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();

    clock.advance(Duration::from_millis(14_999));
    let early = offer_with(2, 1, reservation(), T0.plus_secs(15));
    journal.hold_offer(&early).unwrap();
    assert_eq!(
        journal
            .authorize_start(&early.attempt, authority(T0))
            .unwrap(),
        StartDecision::Fresh,
        "one millisecond before expiry the offer still stands"
    );

    clock.advance(Duration::from_millis(1));
    let refused = journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap_err();
    assert!(matches!(refused, JournalError::OfferNotHeld { .. }));
    assert_eq!(
        state(&journal, &offer.attempt),
        LocalAttemptState::OfferHeld
    );

    // An attempt authorised in time is still not launched twice afterwards.
    clock.advance(Duration::from_secs(30));
    assert_eq!(
        journal
            .authorize_start(&early.attempt, authority(T0))
            .unwrap(),
        StartDecision::AlreadyAuthorized
    );
}

#[test]
fn the_documented_transition_table_is_the_one_enforced() {
    use LocalAttemptState::*;
    let allowed = [
        (StartAuthorized, Preparing),
        (StartAuthorized, Cleaning),
        (Preparing, Running),
        (Preparing, Cleaning),
        (Running, EnvironmentHeld),
        (Running, Collecting),
        (Running, Cleaning),
        (EnvironmentHeld, Collecting),
        (EnvironmentHeld, Cleaning),
        (Collecting, Cleaning),
        (Cleaning, Released),
        (Cleaning, Quarantined),
        (Quarantined, Cleaning),
        (LostAcrossBoot, Cleaning),
    ];
    for from in ALL_STATES {
        for to in ALL_STATES {
            assert_eq!(
                can_transition(from, to),
                allowed.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
    // Nothing leaves Released, and nothing is released without cleanup.
    for to in ALL_STATES {
        assert!(!can_transition(Released, to));
    }
    for from in ALL_STATES {
        assert_eq!(can_transition(from, Released), from == Cleaning);
    }
}

#[test]
fn set_state_walks_the_table_and_refuses_shortcuts() {
    use LocalAttemptState::*;
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    let id = &offer.attempt.attempt_id;

    // An offer that is only held cannot be moved by hand.
    for to in ALL_STATES {
        if to == OfferHeld {
            continue;
        }
        assert!(
            matches!(
                journal.set_state(id, to),
                Err(JournalError::InvalidState {
                    from: OfferHeld,
                    ..
                })
            ),
            "OfferHeld -> {to:?}"
        );
    }
    journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap();

    assert!(matches!(
        journal.set_state(id, Running),
        Err(JournalError::InvalidState {
            from: StartAuthorized,
            to: Running
        })
    ));
    assert!(matches!(
        journal.set_state(id, Released),
        Err(JournalError::InvalidState { .. })
    ));
    journal.set_state(id, Preparing).unwrap();
    journal.set_state(id, Preparing).unwrap();
    journal.set_scope(id, &scope("bella-walk")).unwrap();
    journal.set_state(id, Running).unwrap();
    journal.set_state(id, EnvironmentHeld).unwrap();
    assert!(matches!(
        journal.set_state(id, Running),
        Err(JournalError::InvalidState { .. })
    ));
    journal.set_state(id, Collecting).unwrap();
    journal.set_state(id, Cleaning).unwrap();
    journal.set_state(id, Quarantined).unwrap();
    assert_eq!(
        journal.held_resources().unwrap(),
        reservation(),
        "a quarantined attempt keeps its resources"
    );
    assert!(matches!(
        journal.set_state(id, Released),
        Err(JournalError::InvalidState {
            from: Quarantined,
            to: Released
        })
    ));
    journal.set_state(id, Cleaning).unwrap();
    journal.set_state(id, Released).unwrap();
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
    for to in ALL_STATES {
        if to == Released {
            continue;
        }
        assert!(journal.set_state(id, to).is_err(), "Released -> {to:?}");
    }
}

#[test]
fn a_scope_must_be_journaled_before_anything_runs() {
    use LocalAttemptState::*;
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    let id = &offer.attempt.attempt_id;

    // Before the start permit there is nothing to own.
    assert!(matches!(
        journal.set_scope(id, &scope("bella-early")),
        Err(JournalError::NotAllowed {
            state: OfferHeld,
            ..
        })
    ));
    journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap();
    journal.set_state(id, Preparing).unwrap();

    let unowned = journal.set_state(id, Running).unwrap_err();
    assert!(matches!(unowned, JournalError::ScopeNotRecorded { .. }));
    assert_eq!(state(&journal, &offer.attempt), Preparing);

    let first = scope("bella-scope");
    journal.set_scope(id, &first).unwrap();
    journal.set_scope(id, &first).unwrap();
    journal.set_state(id, Running).unwrap();

    // The adapter may add detail; the scope itself cannot be swapped.
    let mut more = first.clone();
    more.detail.push_str(";pgid=4243 started=1700000001");
    journal.set_scope(id, &more).unwrap();
    assert!(matches!(
        journal.set_scope(id, &scope("bella-other")),
        Err(JournalError::ScopeMismatch { .. })
    ));
    assert_eq!(journal.attempt(id).unwrap().unwrap().scope, Some(more));
}

#[test]
fn a_step_cannot_start_without_a_scope_or_outside_running_and_cleanup() {
    use LocalAttemptState::*;
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    let id = &offer.attempt.attempt_id;
    assert!(matches!(
        journal.set_step_state(id, "build", StepState::Pending, None),
        Err(JournalError::NotAllowed {
            state: OfferHeld,
            ..
        })
    ));
    journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap();
    journal
        .set_step_state(id, "build", StepState::Pending, None)
        .unwrap();
    assert!(matches!(
        journal.set_step_state(id, "build", StepState::Running, None),
        Err(JournalError::NotAllowed {
            state: StartAuthorized,
            ..
        })
    ));

    // Straight to cleanup, where a cleanup step still needs a scope first.
    journal.set_state(id, Cleaning).unwrap();
    assert!(matches!(
        journal.set_step_state(id, "teardown", StepState::Running, None),
        Err(JournalError::ScopeNotRecorded { .. })
    ));
    journal.set_scope(id, &scope("bella-cleanup")).unwrap();
    journal
        .set_step_state(id, "teardown", StepState::Running, None)
        .unwrap();
    journal
        .set_step_state(id, "teardown", StepState::Succeeded, Some(0))
        .unwrap();
    journal
        .set_step_state(id, "build", StepState::Skipped, None)
        .unwrap();

    let steps = journal.attempt(id).unwrap().unwrap().steps;
    assert_eq!(steps["teardown"].state, StepState::Succeeded);
    assert_eq!(steps["teardown"].exit_code, Some(0));
    assert_eq!(steps["build"].state, StepState::Skipped);
    assert_eq!(steps.len(), 2);

    journal.release(&offer.attempt, proof()).unwrap();
    assert!(matches!(
        journal.set_step_state(id, "build", StepState::Failed, Some(1)),
        Err(JournalError::NotAllowed {
            state: Released,
            ..
        })
    ));
}

#[test]
fn a_workspace_is_recorded_once_before_the_first_process() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    let id = &offer.attempt.attempt_id;
    assert!(matches!(
        journal.set_workspace(id, "work/a"),
        Err(JournalError::NotAllowed { .. })
    ));
    journal
        .authorize_start(&offer.attempt, authority(T0))
        .unwrap();
    journal.set_workspace(id, "work/a").unwrap();
    journal.set_workspace(id, "work/a").unwrap();
    assert!(matches!(
        journal.set_workspace(id, "work/b"),
        Err(JournalError::WorkspaceMismatch { .. })
    ));
    assert_eq!(
        journal.attempt(id).unwrap().unwrap().workspace.as_deref(),
        Some("work/a")
    );

    let late = common::offer(2);
    let running = run(&journal, &late, authority(T0));
    journal
        .set_state(&running.attempt_id, LocalAttemptState::Collecting)
        .unwrap();
    // Already recorded: repeating it is fine at any point.
    journal
        .set_workspace(&running.attempt_id, "work/attempt")
        .unwrap();
}

#[test]
fn renewal_only_moves_valid_until_and_only_forwards() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    assert!(matches!(
        journal.renew_authority(&offer.attempt, authority(T0)),
        Err(JournalError::NotAllowed { .. })
    ));
    let permit = authority(T0);
    journal.authorize_start(&offer.attempt, permit).unwrap();

    // A renewal that also tries to move the hard deadline and the
    // disconnected allowance.
    let generous = ExecutionAuthority {
        valid_until: T0.plus_secs(70),
        hard_deadline: T0.plus_secs(999_999),
        disconnected_continuation_secs: 86_400,
    };
    let renewed = journal.renew_authority(&offer.attempt, generous).unwrap();
    assert_eq!(renewed.valid_until, T0.plus_secs(70));
    assert_eq!(renewed.hard_deadline, permit.hard_deadline);
    assert_eq!(
        renewed.disconnected_continuation_secs,
        permit.disconnected_continuation_secs
    );

    // An older renewal delivered late does not shorten the window.
    let late = ExecutionAuthority {
        valid_until: T0.plus_secs(65),
        ..permit
    };
    assert_eq!(
        journal.renew_authority(&offer.attempt, late).unwrap(),
        renewed
    );
    assert_eq!(
        journal
            .attempt(&offer.attempt.attempt_id)
            .unwrap()
            .unwrap()
            .authority,
        Some(renewed)
    );

    // Once the attempt is cleaning up there is nothing left to extend.
    let id = &offer.attempt.attempt_id;
    journal.set_state(id, LocalAttemptState::Cleaning).unwrap();
    let ignored = ExecutionAuthority {
        valid_until: T0.plus_secs(500),
        ..permit
    };
    assert_eq!(
        journal.renew_authority(&offer.attempt, ignored).unwrap(),
        renewed
    );

    let revoked = common::offer(2);
    journal.hold_offer(&revoked).unwrap();
    journal.revoke(&revoked.attempt).unwrap();
    assert!(matches!(
        journal.renew_authority(&revoked.attempt, permit),
        Err(JournalError::OfferNotHeld { .. })
    ));
}

#[test]
fn the_local_stop_deadline_is_the_earlier_limit_and_honours_the_machine_cap() {
    let permit = authority(T0);
    assert_eq!(local_stop_deadline(&permit, None), T0.plus_secs(180));
    assert_eq!(local_stop_deadline(&permit, Some(30)), T0.plus_secs(90));
    assert_eq!(local_stop_deadline(&permit, Some(9_999)), T0.plus_secs(180));
    let short = ExecutionAuthority {
        hard_deadline: T0.plus_secs(100),
        ..permit
    };
    assert_eq!(local_stop_deadline(&short, None), T0.plus_secs(100));
}

#[test]
fn the_first_outcome_stands_and_releases_nothing() {
    let clock = clock();
    let journal = memory(&clock);
    let offer = offer(1);
    journal.hold_offer(&offer).unwrap();
    let id = &offer.attempt.attempt_id;
    let failed = Outcome::Failed {
        step: Some("test".to_owned()),
        exit_code: Some(1),
    };
    let usage = UsageSummary {
        cpu_time_ms: 12_000,
        memory_current_bytes: 0,
        memory_peak_bytes: 3 * GIB,
        processes: 0,
        disk_bytes: GIB,
    };
    assert!(matches!(
        journal.record_outcome(id, &failed, &usage),
        Err(JournalError::NotAllowed { .. })
    ));

    let attempt = run(&journal, &common::offer(2), authority(T0));
    let id = &attempt.attempt_id;
    assert!(journal.record_outcome(id, &failed, &usage).unwrap());
    assert!(
        !journal
            .record_outcome(id, &Outcome::Cancelled, &UsageSummary::default())
            .unwrap()
    );
    let record = journal.attempt(id).unwrap().unwrap();
    assert_eq!(record.outcome, Some(failed));
    assert_eq!(record.usage, Some(usage.clone()));
    assert_eq!(record.state, LocalAttemptState::Running);
    assert!(record.state.holds_resources());

    let report = journal
        .reports()
        .unwrap()
        .into_iter()
        .find(|report| report.attempt == attempt)
        .unwrap();
    assert_eq!(report.usage, Some(usage));
    assert_eq!(report.reserved, reservation());
}

#[test]
fn a_stop_request_only_lets_the_attempt_move_towards_cleanup() {
    use LocalAttemptState::*;
    let clock = clock();
    let journal = memory(&clock);
    let waiting = start(&journal, &offer(1), authority(T0));
    let running = run(&journal, &offer(2), authority(T0));

    assert!(journal.request_cancel(&waiting, "user cancelled").unwrap());
    assert!(!journal.request_cancel(&waiting, "again").unwrap());
    assert!(journal.request_cancel(&running, "user cancelled").unwrap());
    let record = journal.attempt(&waiting.attempt_id).unwrap().unwrap();
    let cancel = record.cancel.unwrap();
    assert_eq!(cancel.reason, "user cancelled");
    assert_eq!(cancel.requested_at, T0);
    // A request is not a result: both still hold what they held.
    assert_eq!(
        journal.held_resources().unwrap(),
        reservation().saturating_add(&reservation())
    );

    assert!(matches!(
        journal.set_state(&waiting.attempt_id, Preparing),
        Err(JournalError::CancelRequested { .. })
    ));
    assert!(matches!(
        journal.set_state(&running.attempt_id, EnvironmentHeld),
        Err(JournalError::CancelRequested { .. })
    ));
    assert!(matches!(
        journal.set_step_state(&running.attempt_id, "test", StepState::Running, None),
        Err(JournalError::CancelRequested { .. })
    ));

    journal.set_state(&waiting.attempt_id, Cleaning).unwrap();
    journal.set_state(&running.attempt_id, Cleaning).unwrap();
    // Cleanup steps always may run.
    journal
        .set_step_state(&running.attempt_id, "teardown", StepState::Running, None)
        .unwrap();

    let mut stale = running.clone();
    stale.generation = 0;
    assert!(matches!(
        journal.request_cancel(&stale, "old"),
        Err(JournalError::StaleGeneration { .. })
    ));
}

#[test]
fn release_needs_cleanup_and_a_complete_proof() {
    use LocalAttemptState::*;
    let clock = clock();
    let journal = memory(&clock);
    let attempt = run(&journal, &offer(1), authority(T0));
    let id = &attempt.attempt_id;

    assert!(matches!(
        journal.release(&attempt, proof()),
        Err(JournalError::InvalidState {
            from: Running,
            to: Released
        })
    ));
    journal.set_state(id, Cleaning).unwrap();

    let mut leaky = proof();
    leaky.scope_empty = false;
    assert!(matches!(
        journal.release(&attempt, leaky),
        Err(JournalError::IncompleteReleaseProof { .. })
    ));
    let mut littered = proof();
    littered.workspace_removed = false;
    assert!(matches!(
        journal.release(&attempt, littered),
        Err(JournalError::IncompleteReleaseProof { .. })
    ));
    assert_eq!(state(&journal, &attempt), Cleaning);
    assert_eq!(journal.held_resources().unwrap(), reservation());
    assert!(journal.unacked_events(id).unwrap().is_empty());

    let released = journal.release(&attempt, proof()).unwrap();
    assert_eq!(released.seq, 1);
    assert_eq!(released.event, AttemptEvent::Released { proof: proof() });
    assert_eq!(state(&journal, &attempt), Released);
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
    assert_eq!(journal.unacked_events(id).unwrap(), vec![released]);
}

#[test]
fn held_resources_counts_offers_in_full_and_sums_tokens() {
    let clock = clock();
    let journal = memory(&clock);
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);

    let sim = ResourceVector::new(1000, GIB, 0).with_token("ios-simulator", 1);
    let gpu = ResourceVector::new(4000, 8 * GIB, 5 * GIB)
        .with_token("ios-simulator", 2)
        .with_token("gpu", 1);
    let held_only = offer_with(1, 1, sim.clone(), T0.plus_secs(15));
    journal.hold_offer(&held_only).unwrap();
    let running = run(
        &journal,
        &offer_with(2, 1, gpu.clone(), T0.plus_secs(15)),
        authority(T0),
    );

    let expected = ResourceVector::new(5000, 9 * GIB, 5 * GIB)
        .with_token("ios-simulator", 3)
        .with_token("gpu", 1);
    assert_eq!(journal.held_resources().unwrap(), expected);
    assert_eq!(journal.active_attempts().unwrap().len(), 2);

    journal.revoke(&held_only.attempt).unwrap();
    assert_eq!(journal.held_resources().unwrap(), gpu);
    journal
        .set_state(&running.attempt_id, LocalAttemptState::Cleaning)
        .unwrap();
    assert_eq!(journal.held_resources().unwrap(), gpu);
    journal.release(&running, proof()).unwrap();
    assert_eq!(journal.held_resources().unwrap(), ResourceVector::ZERO);
    assert_eq!(journal.attempts().unwrap().len(), 2);
    assert!(journal.active_attempts().unwrap().is_empty());
}

#[test]
fn faults_of_the_journal_are_not_protocol_rejections() {
    assert_eq!(JournalError::NotRecovered.reject_code(), None);
    assert_eq!(JournalError::Busy.reject_code(), None);
    assert!(JournalError::NotEnrolled.command_result().is_none());
    assert!(RejectCode::try_from(&JournalError::NotRecovered).is_err());

    let stale = JournalError::StaleEpoch {
        known: 5,
        offered: 4,
    };
    assert_eq!(
        RejectCode::try_from(&stale).unwrap(),
        RejectCode::StaleEpoch
    );
    let policy = JournalError::StalePolicyRevision {
        known: 3,
        offered: 2,
    };
    assert_eq!(
        policy.reject_code(),
        Some(RejectCode::PolicyRevisionMismatch)
    );
    match stale.command_result() {
        Some(CommandResult::Rejected { code, message }) => {
            assert_eq!(code, RejectCode::StaleEpoch);
            assert!(message.contains('5') && message.contains('4'), "{message}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}
