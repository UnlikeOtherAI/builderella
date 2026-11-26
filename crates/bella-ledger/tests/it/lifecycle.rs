use crate::common::{
    boot, exited, fixture, lost, proof, released, spec, unknown_attempt, usage, vector,
};
use bella_core::ErrorCode;
use bella_core::ids::Digest;
use bella_core::job::{CleanupState, JobPhase, Outcome, RetryClass};
use bella_core::machine::AvailabilityFlag;
use bella_core::protocol::{
    ArtifactKind, AttemptEvent, AttemptRef, ReleaseProof, ScopeIdentity, ScopeKind, StepState,
};
use bella_ledger::{CancelOutcome, EventDisposition, JobDisposition, LedgerError, ReleaseOutcome};

fn scope() -> ScopeIdentity {
    ScopeIdentity {
        kind: ScopeKind::LinuxCgroup,
        name: "bella-attempt".into(),
        detail: "/sys/fs/cgroup/bella/attempt".into(),
    }
}

fn step(name: &str, state: StepState, exit_code: Option<i32>) -> AttemptEvent {
    AttemptEvent::StepChanged {
        step: name.into(),
        state,
        exit_code,
    }
}

fn cleanup(state: CleanupState) -> AttemptEvent {
    AttemptEvent::Cleanup {
        state,
        detail: String::new(),
    }
}

fn failed() -> Outcome {
    Outcome::Failed {
        step: Some("test".into()),
        exit_code: Some(1),
    }
}

#[test]
fn an_attempt_runs_from_offer_to_verified_release() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    let phase = || f.ledger.job(&job).unwrap().phase;

    f.events(&attempt, 1, vec![AttemptEvent::Preparing]);
    assert_eq!(phase(), JobPhase::Preparing);
    f.advance_secs(2);
    let started_at = f.now();
    f.events(
        &attempt,
        2,
        vec![
            AttemptEvent::Started { scope: scope() },
            step("compile", StepState::Running, None),
        ],
    );
    assert_eq!(phase(), JobPhase::Executing);
    f.advance_secs(30);
    f.events(
        &attempt,
        4,
        vec![
            step("compile", StepState::Succeeded, Some(0)),
            step("test", StepState::Running, None),
            step("test", StepState::Succeeded, Some(0)),
            AttemptEvent::Collecting,
        ],
    );
    assert_eq!(phase(), JobPhase::Collecting);
    f.events(
        &attempt,
        8,
        vec![AttemptEvent::ArtifactPublished {
            path: "target/app".into(),
            kind: ArtifactKind::Artifact,
            digest: Digest::of_bytes(b"binary"),
            size: 1234,
        }],
    );

    // The result is known while the resources are still held.
    let exited_at = f.now();
    f.events(&attempt, 9, vec![exited(Outcome::Succeeded)]);
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Cleaning);
    assert_eq!(view.outcome, Some(Outcome::Succeeded));
    assert!(!view.is_settled());
    assert_eq!(f.held(&machine), vector(1000, 1));

    f.events(
        &attempt,
        10,
        vec![
            cleanup(CleanupState::ProcessesStopped),
            cleanup(CleanupState::CleanupComplete),
        ],
    );
    assert_eq!(
        f.ledger.job(&job).unwrap().cleanup,
        CleanupState::CleanupComplete
    );
    f.advance_secs(1);
    f.events(&attempt, 12, vec![released()]);

    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert!(view.is_settled());
    assert_eq!(view.outcome, Some(Outcome::Succeeded));
    assert_eq!(view.cleanup, CleanupState::Released);
    assert!(f.held(&machine).is_zero());
    let run = view.current_attempt().unwrap();
    assert_eq!(run.phase, JobPhase::Finished);
    assert_eq!(run.cleanup, CleanupState::Released);
    assert_eq!(run.outcome, Some(Outcome::Succeeded));
    assert_eq!(run.usage, Some(usage()));
    assert_eq!(run.started_at, Some(started_at));
    assert_eq!(run.exited_at, Some(exited_at));
    assert_eq!(run.released_at, Some(f.now()));
    assert!(!run.uncertain);
    let steps: Vec<_> = run
        .steps
        .iter()
        .map(|s| (s.name.as_str(), s.state, s.exit_code))
        .collect();
    assert_eq!(
        steps,
        [
            ("compile", StepState::Succeeded, Some(0)),
            ("test", StepState::Succeeded, Some(0))
        ]
    );
    assert_eq!(run.steps[0].started_at, Some(started_at));
    assert_eq!(run.steps[0].finished_at, Some(exited_at));
    assert_eq!(f.ledger.job_artifacts(&job).unwrap().len(), 1);
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert_eq!(record.last_event_seq, 12);
    assert_eq!(record.release_reason.as_deref(), Some("proof"));
    assert!(record.start_delivered_at.is_some());

    assert_eq!(
        f.job_event_kinds(&job),
        [
            "job.queued",
            "attempt.offered",
            "job.offered",
            "attempt.accepted",
            "attempt.start_committed",
            "attempt.preparing",
            "job.preparing",
            "attempt.started",
            "job.executing",
            "attempt.step_changed",
            "attempt.step_changed",
            "attempt.step_changed",
            "attempt.step_changed",
            "attempt.collecting",
            "job.collecting",
            "attempt.artifact_published",
            "attempt.exited",
            "job.cleaning",
            "attempt.cleanup",
            "attempt.cleanup",
            "attempt.released",
            "job.finished"
        ]
    );
    // Events can be replayed from any sequence number.
    let all = f.ledger.job_events_after(&job, 0, 10_000).unwrap();
    assert_eq!(view.last_event, all.last().unwrap().seq);
    let tail = f
        .ledger
        .job_events_after(&job, all[all.len() - 3].seq, 10)
        .unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[1].kind, "job.finished");
    let first_two = f.ledger.events_after(0, 2).unwrap();
    assert_eq!(first_two.len(), 2);
    assert!(first_two[0].seq < first_two[1].seq);
}

/// A11: duplicate events and a duplicate release change nothing.
#[test]
fn a11_duplicate_events_and_release_are_no_ops() {
    let f = fixture();
    let machine = f.machine("builder");
    let other = f.submit("app");
    f.offer(&other, &machine);
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    let script = vec![
        AttemptEvent::Preparing,
        AttemptEvent::Started { scope: scope() },
        step("compile", StepState::Running, None),
        step("compile", StepState::Failed, Some(2)),
        exited(failed()),
        cleanup(CleanupState::CleanupComplete),
    ];
    f.events(&attempt, 1, script.clone());
    let before = (
        f.ledger.job(&job).unwrap(),
        f.ledger.last_event_seq().unwrap(),
    );

    // The worker resends everything after a reconnect.
    for (index, event) in script.iter().enumerate() {
        let disposition = f.event(&attempt, index as u64 + 1, event.clone()).unwrap();
        assert_eq!(disposition, EventDisposition::Duplicate);
    }
    // A duplicate sequence number is ignored even with different content.
    assert_eq!(
        f.event(&attempt, 5, exited(Outcome::Succeeded)).unwrap(),
        EventDisposition::Duplicate
    );
    assert_eq!(
        (
            f.ledger.job(&job).unwrap(),
            f.ledger.last_event_seq().unwrap()
        ),
        before
    );
    assert_eq!(f.held(&machine), vector(2000, 2));

    // The release applies once.
    assert_eq!(
        f.event(&attempt, 7, released()).unwrap(),
        EventDisposition::Applied
    );
    assert_eq!(f.held(&machine), vector(1000, 1));
    let after = (
        f.ledger.job(&job).unwrap(),
        f.ledger.last_event_seq().unwrap(),
    );
    assert_eq!(after.0.outcome, Some(failed()));

    assert_eq!(
        f.event(&attempt, 7, released()).unwrap(),
        EventDisposition::Duplicate
    );
    assert_eq!(
        f.event(&attempt, 8, released()).unwrap(),
        EventDisposition::AttemptReleased
    );
    assert_eq!(
        f.ledger.record_release(&attempt, &proof()).unwrap(),
        ReleaseOutcome::AlreadyReleased
    );
    assert_eq!(
        f.ledger.record_release(&attempt, &proof()).unwrap(),
        ReleaseOutcome::AlreadyReleased
    );
    // Capacity went down by exactly one reservation and nothing else moved.
    assert_eq!(f.held(&machine), vector(1000, 1));
    assert_eq!(
        (
            f.ledger.job(&job).unwrap(),
            f.ledger.last_event_seq().unwrap()
        ),
        after
    );
    assert_eq!(f.ledger.machine_attempts(&machine).unwrap().len(), 1);
}

#[test]
fn a_sequence_gap_reports_what_is_expected() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing]);

    let error = f.event(&attempt, 3, AttemptEvent::Collecting).unwrap_err();
    assert!(matches!(
        error,
        LedgerError::SequenceGap {
            expected: 2,
            got: 3
        }
    ));
    assert_eq!(error.code(), ErrorCode::InvalidState);
    assert_eq!(error.to_api_error().details["expected"], 2);
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Preparing);
    assert_eq!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .last_event_seq,
        1
    );

    assert_eq!(
        f.event(&unknown_attempt(), 1, AttemptEvent::Preparing)
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );
}

#[test]
fn nothing_runs_without_a_start_permit() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);

    for event in [
        AttemptEvent::Preparing,
        AttemptEvent::Started { scope: scope() },
        exited(Outcome::Succeeded),
    ] {
        let error = f.event(&attempt, 1, event).unwrap_err();
        assert!(matches!(error, LedgerError::InvalidState(_)), "{error}");
    }
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Offered);
    assert!(view.outcome.is_none());
    assert_eq!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .last_event_seq,
        0
    );

    // A worker that dropped its hold may say so: the offer is forgotten.
    assert_eq!(
        f.event(&attempt, 1, released()).unwrap(),
        EventDisposition::Applied
    );
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
    assert_eq!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .release_reason
            .as_deref(),
        Some("forgotten")
    );
}

/// A15 and A60: events and results from an old generation or an old
/// controller epoch are rejected and cannot change the canonical outcome.
#[test]
fn a15_a60_stale_generations_and_epochs_change_nothing() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let old = f.start(&job, &machine);
    f.events(&old, 1, vec![AttemptEvent::Preparing]);

    // The machine reboots; the first attempt is lost and the job retried.
    f.ledger.machine_disconnected(&machine, 1).unwrap();
    f.ledger.machine_connected(&machine, 2).unwrap();
    f.ledger.reconcile(&machine, &boot(2), &[]).unwrap();
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
    let current = f.start(&job, &machine);
    assert_eq!(current.generation, 2);
    f.events(&current, 1, vec![AttemptEvent::Preparing]);
    let before = (
        f.ledger.job(&job).unwrap(),
        f.ledger.last_event_seq().unwrap(),
    );

    // The old attempt turns out to have "succeeded". Nobody listens.
    for event in [
        AttemptEvent::Started { scope: scope() },
        exited(Outcome::Succeeded),
        AttemptEvent::ArtifactPublished {
            path: "stale".into(),
            kind: ArtifactKind::Artifact,
            digest: Digest::of_bytes(b"stale"),
            size: 1,
        },
        released(),
    ] {
        let error = f.event(&old, 2, event).unwrap_err();
        assert!(
            matches!(
                error,
                LedgerError::StaleGeneration {
                    got: 1,
                    current: 2,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(error.code(), ErrorCode::StaleGeneration);
    }
    // Neither does a message that claims the current attempt id with an
    // old generation, or the old id with the current generation.
    for forged in [
        AttemptRef {
            generation: 1,
            ..current.clone()
        },
        AttemptRef {
            generation: 2,
            ..old.clone()
        },
    ] {
        assert!(matches!(
            f.event(&forged, 2, exited(Outcome::Succeeded)),
            Err(LedgerError::StaleGeneration { .. })
        ));
        assert!(matches!(
            f.ledger.record_artifact(
                &forged,
                "stale",
                ArtifactKind::Artifact,
                &Digest::of_bytes(b"stale"),
                1
            ),
            Err(LedgerError::StaleGeneration { .. })
        ));
    }
    assert!(matches!(
        f.ledger.record_release(
            &AttemptRef {
                generation: 1,
                ..current.clone()
            },
            &proof()
        ),
        Err(LedgerError::StaleGeneration { .. })
    ));
    assert_eq!(
        f.ledger.record_release(&old, &proof()).unwrap(),
        ReleaseOutcome::AlreadyReleased
    );
    assert_eq!(
        (
            f.ledger.job(&job).unwrap(),
            f.ledger.last_event_seq().unwrap()
        ),
        before
    );
    assert_eq!(f.held(&machine), vector(1000, 1));
    assert!(f.ledger.job_artifacts(&job).unwrap().is_empty());

    // Commands from another controller epoch are refused as well.
    let epoch = f.ledger.epoch();
    for stale in [epoch - 1, epoch + 1] {
        assert!(matches!(
            f.ledger.commit_start(&current, stale),
            Err(LedgerError::StaleEpoch { .. })
        ));
        assert!(matches!(
            f.ledger.renew_authority(&current, stale),
            Err(LedgerError::StaleEpoch { .. })
        ));
    }

    // Only the current generation sets the result.
    f.events(&current, 2, vec![exited(failed()), released()]);
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.outcome, Some(failed()));
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.attempts.len(), 2);
    assert!(matches!(
        view.attempts[0].outcome,
        Some(Outcome::InfraInterrupted { .. })
    ));
}

#[test]
fn a_retryable_result_requeues_until_attempts_run_out() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app"); // pure, two attempts

    let first = f.start(&job, &machine);
    f.events(&first, 1, vec![AttemptEvent::Preparing, exited(lost())]);
    // A result that will be retried is not the job's result.
    let view = f.ledger.job(&job).unwrap();
    assert!(view.outcome.is_none());
    assert_eq!(view.attempts[0].outcome, Some(lost()));
    assert_eq!(
        f.ledger.record_release(&first, &proof()).unwrap(),
        ReleaseOutcome::Released(JobDisposition::Requeued)
    );
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Queued);
    assert!(view.outcome.is_none());
    assert_eq!(view.cleanup, CleanupState::NotStarted);
    assert!(f.held(&machine).is_zero());
    let queued = f.ledger.queued_jobs(10).unwrap();
    assert_eq!(queued[0].started_attempts, 1);

    // The second attempt is the last one allowed.
    let second = f.start(&job, &machine);
    assert_eq!(second.generation, 2);
    f.events(&second, 1, vec![exited(lost())]);
    assert_eq!(f.ledger.job(&job).unwrap().outcome, Some(lost()));
    assert_eq!(
        f.ledger.record_release(&second, &proof()).unwrap(),
        ReleaseOutcome::Released(JobDisposition::Finished)
    );
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(lost()));
    assert!(
        f.job_event_kinds(&job)
            .ends_with(&["attempt.released".into(), "job.finished".into()])
    );
}

#[test]
fn only_infrastructure_failures_of_repeatable_work_are_retried() {
    let f = fixture();
    let machine = f.machine("builder");

    // An ordinary failure is a result, not a reason to retry.
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.finish(&attempt, failed());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Finished);

    // Work with external side effects is never repeated automatically.
    let mut external = spec();
    external.retry_class = RetryClass::ExternalSideEffects;
    external.max_attempts = 5;
    let job = f.submit_spec("app", external);
    let attempt = f.start(&job, &machine);
    f.events(&attempt, 1, vec![exited(lost())]);
    assert_eq!(f.ledger.job(&job).unwrap().outcome, Some(lost()));
    f.events(&attempt, 2, vec![released()]);
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Finished);

    // A release that never reported a result counts as an interruption.
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    assert_eq!(
        f.ledger.record_release(&attempt, &proof()).unwrap(),
        ReleaseOutcome::Released(JobDisposition::Requeued)
    );
    assert!(matches!(
        f.ledger.attempt(&attempt.attempt_id).unwrap().outcome,
        Some(Outcome::InfraInterrupted { .. })
    ));
}

#[test]
fn an_unverified_release_quarantines_the_capacity() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(
        &attempt,
        1,
        vec![AttemptEvent::Preparing, exited(Outcome::Succeeded)],
    );

    let unverified = ReleaseProof {
        scope_empty: false,
        workspace_removed: true,
        method: "timed out waiting for the process group".into(),
    };
    assert_eq!(
        f.ledger.record_release(&attempt, &unverified).unwrap(),
        ReleaseOutcome::Quarantined
    );
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Quarantined);
    assert_eq!(view.cleanup, CleanupState::Quarantined);
    assert_eq!(view.outcome, Some(Outcome::Succeeded), "the result stands");
    assert!(!view.is_settled());
    assert_eq!(f.held(&machine), vector(1000, 1));
    assert!(
        f.ledger
            .machine_view(&machine)
            .unwrap()
            .flags
            .contains(&AvailabilityFlag::Quarantined)
    );
    // A workspace that was not removed is no better.
    let half = ReleaseProof {
        scope_empty: true,
        workspace_removed: false,
        method: "cgroup.procs empty".into(),
    };
    assert_eq!(
        f.ledger.record_release(&attempt, &half).unwrap(),
        ReleaseOutcome::Quarantined
    );
    assert_eq!(f.held(&machine), vector(1000, 1));

    // Cleanup resumes, and a real proof releases it.
    f.events(&attempt, 3, vec![cleanup(CleanupState::ProcessesStopped)]);
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Cleaning);
    assert_eq!(view.cleanup, CleanupState::ProcessesStopped);
    assert!(f.ledger.machine_view(&machine).unwrap().flags.is_empty());
    f.events(&attempt, 4, vec![released()]);
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Finished);
    assert!(f.held(&machine).is_zero());
}

#[test]
fn a_quarantined_attempt_is_resolved_by_an_administrator() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(
        &attempt,
        1,
        vec![
            AttemptEvent::Preparing,
            exited(failed()),
            AttemptEvent::Quarantined {
                reason: "processes survived SIGKILL".into(),
            },
        ],
    );
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Quarantined);
    assert_eq!(f.held(&machine), vector(1000, 1));
    assert!(
        f.job_event_kinds(&job)
            .ends_with(&["attempt.quarantined".into(), "job.quarantined".into()])
    );

    let error = f
        .ledger
        .operator_release(&f.dev, &attempt.attempt_id, "looked fine to me")
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Forbidden);
    assert_eq!(f.held(&machine), vector(1000, 1));

    assert_eq!(
        f.ledger
            .operator_release(&f.admin, &attempt.attempt_id, "machine reimaged")
            .unwrap(),
        ReleaseOutcome::Released(JobDisposition::Finished)
    );
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(failed()));
    assert!(f.held(&machine).is_zero());
    assert_eq!(
        f.ledger
            .operator_release(&f.admin, &attempt.attempt_id, "again")
            .unwrap(),
        ReleaseOutcome::AlreadyReleased
    );
}

#[test]
fn cancelling_a_queued_job_finishes_it_at_once() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    assert_eq!(
        f.ledger.request_cancel(&job).unwrap(),
        CancelOutcome::Cancelled { revoked: None }
    );
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(Outcome::Cancelled));
    assert!(view.cancel_requested);
    assert_eq!(
        f.job_event_kinds(&job),
        ["job.queued", "job.cancel_requested", "job.finished"]
    );

    // No offer can follow, and cancelling again changes nothing.
    assert!(matches!(
        f.try_offer(&job, &machine, &vector(1000, 1)),
        Err(LedgerError::InvalidState(_))
    ));
    let events = f.ledger.last_event_seq().unwrap();
    assert_eq!(
        f.ledger.request_cancel(&job).unwrap(),
        CancelOutcome::AlreadyFinished
    );
    assert_eq!(f.ledger.last_event_seq().unwrap(), events);
    assert_eq!(
        f.ledger
            .request_cancel(&bella_core::ids::JobId::generate())
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );
}

#[test]
fn cancelling_an_unstarted_offer_revokes_it() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    match f.ledger.request_cancel(&job).unwrap() {
        CancelOutcome::Cancelled {
            revoked: Some(target),
        } => {
            assert_eq!(target.attempt, attempt);
            assert_eq!(target.machine_id, machine);
        }
        other => panic!("unexpected outcome {other:?}"),
    }
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(Outcome::Cancelled));
    assert!(f.held(&machine).is_zero());
    assert_eq!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .release_reason
            .as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        f.job_event_kinds(&job),
        [
            "job.queued",
            "attempt.offered",
            "job.offered",
            "job.cancel_requested",
            "attempt.cancelled",
            "job.finished"
        ]
    );
}

#[test]
fn cancelling_a_started_job_is_a_request_until_cleanup_is_observed() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.events(
        &attempt,
        1,
        vec![
            AttemptEvent::Preparing,
            AttemptEvent::Started { scope: scope() },
        ],
    );

    let expect_requested = || match f.ledger.request_cancel(&job).unwrap() {
        CancelOutcome::Requested { target } => {
            assert_eq!(target.attempt, attempt);
            assert_eq!(target.machine_id, machine);
        }
        other => panic!("unexpected outcome {other:?}"),
    };
    expect_requested();
    let view = f.ledger.job(&job).unwrap();
    assert!(view.cancel_requested);
    assert_eq!(
        view.phase,
        JobPhase::Executing,
        "still running until observed"
    );
    assert_eq!(view.cleanup, CleanupState::Requested);
    assert!(view.outcome.is_none());
    assert_eq!(f.held(&machine), vector(1000, 1));

    // Asking again converges: same answer, no new event.
    let events = f.ledger.last_event_seq().unwrap();
    expect_requested();
    expect_requested();
    assert_eq!(f.ledger.last_event_seq().unwrap(), events);

    f.events(&attempt, 3, vec![cleanup(CleanupState::WorkerAcknowledged)]);
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Cleaning);
    assert_eq!(view.cleanup, CleanupState::WorkerAcknowledged);
    assert_eq!(f.held(&machine), vector(1000, 1));

    f.events(
        &attempt,
        4,
        vec![
            exited(Outcome::Cancelled),
            cleanup(CleanupState::CleanupComplete),
            released(),
        ],
    );
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(Outcome::Cancelled));
    assert_eq!(view.cleanup, CleanupState::Released);
    assert!(f.held(&machine).is_zero());
    assert_eq!(
        f.ledger.request_cancel(&job).unwrap(),
        CancelOutcome::AlreadyFinished
    );
}

#[test]
fn a_cancelled_attempt_is_not_retried_even_when_it_was_lost() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    f.ledger.request_cancel(&job).unwrap();
    f.events(&attempt, 1, vec![exited(lost()), released()]);
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Finished);
    assert_eq!(view.outcome, Some(lost()));
}

/// A14: a machine that is out of contact keeps its reservations.
#[test]
fn a14_an_uncertain_attempt_keeps_its_reservation() {
    let f = fixture();
    let machine = f.machine("builder");
    let running = f.submit("app");
    let offered = f.submit("app");
    let attempt = f.start(&running, &machine);
    f.events(&attempt, 1, vec![AttemptEvent::Preparing]);
    let pending = f.offer(&offered, &machine);

    // Heartbeats stop: stale, then unreachable, then the connection drops.
    f.advance_secs(15);
    f.ledger
        .set_machine_condition(&machine, AvailabilityFlag::Stale, true)
        .unwrap();
    f.advance_secs(15);
    f.ledger
        .set_machine_condition(&machine, AvailabilityFlag::Unreachable, true)
        .unwrap();
    let flagged = f.ledger.mark_machine_uncertain(&machine).unwrap();
    assert_eq!(flagged.len(), 2);
    f.ledger.machine_disconnected(&machine, 1).unwrap();
    f.advance_secs(3600);

    // Nothing was released and nothing finished.
    assert_eq!(f.held(&machine), vector(2000, 2));
    let view = f.ledger.machine_view(&machine).unwrap();
    assert_eq!(view.attempts.len(), 2);
    assert!(view.attempts.iter().all(|a| a.uncertain));
    let job = f.ledger.job(&running).unwrap();
    assert_eq!(job.phase, JobPhase::Preparing);
    assert!(job.outcome.is_none());
    assert!(job.current_attempt().unwrap().uncertain);
    for attempt in [&attempt, &pending] {
        let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
        assert!(record.held && record.uncertain);
    }
    // Marking twice writes nothing new.
    let events = f.ledger.last_event_seq().unwrap();
    f.ledger.mark_machine_uncertain(&machine).unwrap();
    assert_eq!(f.ledger.last_event_seq().unwrap(), events);

    // The job cannot be placed elsewhere while its attempt is unresolved,
    // and the machine's capacity is still spoken for.
    let spare = f.machine("spare");
    assert!(matches!(
        f.try_offer(&running, &spare, &vector(1000, 1)),
        Err(LedgerError::InvalidState(_))
    ));

    // The worker comes back, still running the attempt: uncertainty clears.
    f.ledger.machine_connected(&machine, 2).unwrap();
    let report = bella_core::protocol::AttemptReport {
        attempt: attempt.clone(),
        job_id: running.clone(),
        state: bella_core::protocol::LocalAttemptState::Running,
        last_event_seq: 1,
        reserved: vector(1000, 1),
        usage: None,
    };
    f.ledger.reconcile(&machine, &boot(1), &[report]).unwrap();
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert!(record.held && !record.uncertain);
    assert_eq!(
        f.held(&machine),
        vector(1000, 1),
        "the unstarted offer was dropped"
    );
}
