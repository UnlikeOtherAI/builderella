use crate::common::{
    GIB, boot, capacity, fixture, heartbeat, proof, spec, unknown_attempt, vector,
};
use bella_core::ErrorCode;
use bella_core::job::{JobPhase, QueueReasonCode};
use bella_core::protocol::{
    AttemptEvent, AttemptRef, AttemptReport, LocalAttemptState, OfferDecision, UsageSummary,
};
use bella_core::resources::ResourceVector;
use bella_ledger::{CancelOutcome, LedgerError, OfferReplyOutcome, ReleaseOutcome, RevokeOutcome};
use std::collections::BTreeSet;
use std::sync::Barrier;

fn rejected() -> OfferDecision {
    OfferDecision::Rejected {
        code: QueueReasonCode::MemoryPressure,
        message: "available memory is below the floor".into(),
    }
}

#[test]
fn an_offer_reserves_and_moves_the_job_to_offered() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let reservation = vector(2000, 4);
    let expires = f.expiry();
    let offer = f
        .ledger
        .create_offer(&job, &machine, &reservation, &capacity(), Some(4), expires)
        .unwrap();

    assert_eq!(offer.attempt.generation, 1);
    assert_eq!(offer.job_id, job);
    assert_eq!(offer.machine_id, machine);
    assert_eq!(offer.reservation, reservation);
    assert_eq!(offer.spec, spec());
    assert_eq!(offer.policy_revision, 1);
    assert_eq!(offer.expires_at, expires);
    assert_eq!(offer.epoch, f.ledger.epoch());
    let wire = offer.to_offer();
    assert_eq!(wire.attempt, offer.attempt);
    assert_eq!(wire.manifest_digest, offer.manifest_digest);

    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Offered);
    let attempt = view.current_attempt().unwrap();
    assert_eq!(attempt.id, offer.attempt.attempt_id);
    assert_eq!(attempt.phase, JobPhase::Offered);
    assert_eq!(attempt.machine_name, "builder");
    assert!(!attempt.start_committed);
    assert_eq!(attempt.reserved, reservation);
    assert_eq!(attempt.steps.len(), 2);
    assert_eq!(attempt.steps[0].name, "compile");

    let record = f.ledger.attempt(&offer.attempt.attempt_id).unwrap();
    assert!(record.held);
    assert!(record.accepted_at.is_none());
    assert_eq!(record.offer_expires_at, expires);
    assert_eq!(f.held(&machine), reservation);
    assert_eq!(
        f.job_event_kinds(&job),
        ["job.queued", "attempt.offered", "job.offered"]
    );
}

/// A02: two offers race for the last memory on a machine; exactly one wins.
#[test]
fn a02_two_offers_race_for_the_last_memory() {
    for round in 0..25 {
        let f = fixture();
        let machine = f.machine("builder");
        // 10 of 16 GiB are taken; 6 GiB are left.
        let resident = f.submit("app");
        f.try_offer(&resident, &machine, &vector(1000, 10)).unwrap();
        let contenders = [f.submit("app"), f.submit("lib")];
        let barrier = Barrier::new(2);

        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = contenders
                .iter()
                .map(|job| {
                    let (f, machine, barrier) = (&f, &machine, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        f.try_offer(job, machine, &vector(1000, 6))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "round {round}: exactly one offer may succeed");
        let loser = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        match loser {
            LedgerError::Capacity { shortfall } => {
                assert_eq!(shortfall.dimension, "memory");
                assert_eq!(shortfall.requested, 6 * GIB);
                assert_eq!(shortfall.available, 0);
            }
            other => panic!("round {round}: unexpected error {other}"),
        }
        assert_eq!(loser.code(), ErrorCode::Conflict);
        assert_eq!(f.held(&machine), vector(2000, 16));
        // The loser is untouched: still queued, no attempt.
        let counts = f.ledger.job_counts().unwrap();
        assert_eq!((counts.offered, counts.queued), (2, 1));
    }
}

/// A04: an offered attempt that has used nothing still counts in full
/// against capacity.
#[test]
fn a04_an_idle_reservation_counts_in_full() {
    let f = fixture();
    let machine = f.machine("builder");
    let idle = f.submit("app");
    let offer = f.try_offer(&idle, &machine, &vector(1000, 8)).unwrap();

    // The worker reports the attempt as using no memory at all.
    let mut hb = heartbeat(&machine, &boot(1), 1, 1);
    hb.attempts = vec![AttemptReport {
        attempt: offer.attempt.clone(),
        job_id: idle.clone(),
        state: LocalAttemptState::OfferHeld,
        last_event_seq: 0,
        reserved: vector(1000, 8),
        usage: Some(UsageSummary::default()),
    }];
    f.ledger.record_heartbeat(&hb).unwrap();

    let next = f.submit("app");
    let error = f.try_offer(&next, &machine, &vector(1000, 9)).unwrap_err();
    match error {
        LedgerError::Capacity { shortfall } => {
            assert_eq!(shortfall.dimension, "memory");
            assert_eq!(shortfall.available, 8 * GIB);
        }
        other => panic!("unexpected error {other}"),
    }
    assert_eq!(f.held(&machine), vector(1000, 8));
    // Exactly the remainder still fits, also after the start is committed.
    f.accept(&offer.attempt);
    f.ledger
        .commit_start(&offer.attempt, f.ledger.epoch())
        .unwrap();
    f.try_offer(&next, &machine, &vector(1000, 8)).unwrap();
    assert_eq!(f.held(&machine), vector(2000, 16));
}

#[test]
fn capacity_is_checked_in_every_dimension() {
    let f = fixture();
    let machine = f.machine("builder");
    let mut policy = f.ledger.machine_policy(&machine).unwrap();
    policy.tokens.insert("ios-simulator".into(), 1);
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    let cap = capacity().with_token("ios-simulator", 1);
    let job = f.submit("app");
    let try_with = |reservation: ResourceVector| {
        f.ledger
            .create_offer(&job, &machine, &reservation, &cap, None, f.expiry())
            .unwrap_err()
    };
    let dimension = |error: LedgerError| match error {
        LedgerError::Capacity { shortfall } => shortfall.dimension,
        other => panic!("unexpected error {other}"),
    };
    assert_eq!(dimension(try_with(vector(8001, 1))), "cpu");
    assert_eq!(dimension(try_with(vector(1000, 17))), "memory");
    assert_eq!(
        dimension(try_with(ResourceVector::new(1000, GIB, 101 * GIB))),
        "disk"
    );
    assert_eq!(
        dimension(try_with(vector(1000, 1).with_token("ios-simulator", 2))),
        "token:ios-simulator"
    );
    assert_eq!(
        dimension(try_with(vector(1000, 1).with_token("gpu", 1))),
        "token:gpu"
    );
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);

    // The token is exclusive: the second job cannot have it.
    f.ledger
        .create_offer(
            &job,
            &machine,
            &vector(1000, 1).with_token("ios-simulator", 1),
            &cap,
            None,
            f.expiry(),
        )
        .unwrap();
    let second = f.submit("app");
    let error = f
        .ledger
        .create_offer(
            &second,
            &machine,
            &vector(1000, 1).with_token("ios-simulator", 1),
            &cap,
            None,
            f.expiry(),
        )
        .unwrap_err();
    assert_eq!(dimension(error), "token:ios-simulator");
}

#[test]
fn capacity_never_exceeds_the_policy_ceiling() {
    let f = fixture();
    // 4 CPUs and 8 GiB: the builder profile leaves 3.6 CPUs and 6 GiB.
    let machine = f.enroll("small", 4, 8);
    f.ledger.machine_connected(&machine, 1).unwrap();
    f.ledger.reconcile(&machine, &boot(1), &[]).unwrap();
    let job = f.submit("app");
    let claimed = ResourceVector::new(64_000, 512 * GIB, 100 * GIB);
    let error = f
        .ledger
        .create_offer(&job, &machine, &vector(1000, 7), &claimed, None, f.expiry())
        .unwrap_err();
    match error {
        LedgerError::Capacity { shortfall } => {
            assert_eq!(shortfall.dimension, "memory");
            assert_eq!(shortfall.available, 6 * GIB);
        }
        other => panic!("unexpected error {other}"),
    }
    f.ledger
        .create_offer(&job, &machine, &vector(3600, 6), &claimed, None, f.expiry())
        .unwrap();
}

#[test]
fn held_attempts_are_limited_by_caller_and_policy() {
    let f = fixture();
    let machine = f.machine("builder");
    let jobs: Vec<_> = (0..4).map(|_| f.submit("app")).collect();
    let small = vector(100, 1);
    let offer = |job, limit| {
        f.ledger
            .create_offer(job, &machine, &small, &capacity(), limit, f.expiry())
    };
    offer(&jobs[0], Some(2)).unwrap();
    offer(&jobs[1], Some(2)).unwrap();
    let error = offer(&jobs[2], Some(2)).unwrap_err();
    assert!(matches!(
        error,
        LedgerError::ConcurrencyLimit { limit: 2, held: 2 }
    ));
    assert_eq!(error.code(), ErrorCode::Conflict);
    offer(&jobs[2], Some(3)).unwrap();

    // The machine's own limit applies even when the caller passes none.
    let mut policy = f.ledger.machine_policy(&machine).unwrap();
    policy.max_concurrent_attempts = Some(3);
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    assert!(matches!(
        offer(&jobs[3], None),
        Err(LedgerError::ConcurrencyLimit { limit: 3, held: 3 })
    ));
    assert!(matches!(
        offer(&jobs[3], Some(10)),
        Err(LedgerError::ConcurrencyLimit { limit: 3, held: 3 })
    ));
}

#[test]
fn offers_are_refused_when_job_or_machine_is_not_eligible() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let size = vector(1000, 1);

    let unknown_job = bella_core::ids::JobId::generate();
    assert_eq!(
        f.try_offer(&unknown_job, &machine, &size)
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );
    let unknown_machine = bella_core::ids::MachineId::generate();
    assert_eq!(
        f.try_offer(&job, &unknown_machine, &size)
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );
    // An offer that is already expired is never created.
    assert!(matches!(
        f.ledger
            .create_offer(&job, &machine, &size, &capacity(), None, f.now()),
        Err(LedgerError::Invalid(_))
    ));

    // A machine that has not reconciled gets nothing.
    let fresh = f.enroll("fresh", 16, 64);
    assert!(matches!(
        f.try_offer(&job, &fresh, &size),
        Err(LedgerError::NotReconciled(_))
    ));

    // Machine policy: builds disabled, then a project allow-list.
    let mut policy = f.ledger.machine_policy(&machine).unwrap();
    policy.accept_builds = false;
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    let error = f.try_offer(&job, &machine, &size).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyDenied);
    let other_project = f.ledger.get_or_create_project("lib").unwrap();
    policy.accept_builds = true;
    policy.allowed_projects = Some(vec![other_project.id]);
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();
    let error = f.try_offer(&job, &machine, &size).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyDenied);
    policy.allowed_projects = None;
    f.ledger
        .replace_machine_policy(&f.admin, &machine, &policy)
        .unwrap();

    // Nothing above changed anything.
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.job_event_kinds(&job), ["job.queued"]);

    // A job that is offered, running or finished cannot be offered again.
    let attempt = f.offer(&job, &machine);
    let second_machine = f.machine("second");
    assert!(matches!(
        f.try_offer(&job, &second_machine, &size),
        Err(LedgerError::InvalidState(_))
    ));
    f.accept(&attempt);
    f.ledger.commit_start(&attempt, f.ledger.epoch()).unwrap();
    assert!(matches!(
        f.try_offer(&job, &second_machine, &size),
        Err(LedgerError::InvalidState(_))
    ));
    f.finish(&attempt, bella_core::job::Outcome::Succeeded);
    assert!(matches!(
        f.try_offer(&job, &second_machine, &size),
        Err(LedgerError::InvalidState(_))
    ));
    assert_eq!(f.ledger.job_attempts(&job).unwrap().len(), 1);
}

#[test]
fn a_rejection_releases_the_reservation_and_requeues_the_job() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);

    assert_eq!(
        f.ledger.record_offer_reply(&attempt, &rejected()).unwrap(),
        OfferReplyOutcome::Requeued
    );
    assert!(f.held(&machine).is_zero());
    let view = f.ledger.job(&job).unwrap();
    assert_eq!(view.phase, JobPhase::Queued);
    assert_eq!(
        view.queue_reason.unwrap().code,
        QueueReasonCode::OfferRejected
    );
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert!(!record.held);
    assert_eq!(record.release_reason.as_deref(), Some("rejected"));
    assert_eq!(record.phase, JobPhase::Finished);
    assert!(record.outcome.is_none());

    // Hearing the rejection again changes nothing; a late acceptance is refused.
    assert_eq!(
        f.ledger.record_offer_reply(&attempt, &rejected()).unwrap(),
        OfferReplyOutcome::AlreadyReleased
    );
    assert!(matches!(
        f.ledger
            .record_offer_reply(&attempt, &OfferDecision::Accepted),
        Err(LedgerError::InvalidState(_))
    ));
    assert_eq!(
        f.job_event_kinds(&job),
        [
            "job.queued",
            "attempt.offered",
            "job.offered",
            "attempt.rejected",
            "job.queued"
        ]
    );

    // A rejected offer does not use up one of the job's attempts.
    let queued = f.ledger.queued_jobs(10).unwrap();
    assert_eq!(queued[0].started_attempts, 0);
    assert_eq!(queued[0].last_machine.as_ref(), Some(&machine));

    // The next attempt gets the next generation; the old one is now stale.
    let next = f.offer(&job, &machine);
    assert_eq!(next.generation, 2);
    assert!(matches!(
        f.ledger
            .record_offer_reply(&attempt, &OfferDecision::Accepted),
        Err(LedgerError::StaleGeneration {
            got: 1,
            current: 2,
            ..
        })
    ));
}

#[test]
fn acceptance_is_recorded_once() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    for _ in 0..2 {
        assert_eq!(
            f.ledger
                .record_offer_reply(&attempt, &OfferDecision::Accepted)
                .unwrap(),
            OfferReplyOutcome::Accepted
        );
    }
    assert_eq!(
        f.ledger.attempt(&attempt.attempt_id).unwrap().accepted_at,
        Some(f.now())
    );
    let accepted = f
        .job_event_kinds(&job)
        .iter()
        .filter(|kind| *kind == "attempt.accepted")
        .count();
    assert_eq!(accepted, 1);
    assert_eq!(
        f.ledger
            .record_offer_reply(&unknown_attempt(), &OfferDecision::Accepted)
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );
}

/// A13: revoke before start. A later start commit fails and the job is
/// queued again.
#[test]
fn a13_revoke_before_start_prevents_the_start() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);

    assert_eq!(
        f.ledger
            .revoke_offer(&attempt, "a better machine appeared")
            .unwrap(),
        RevokeOutcome::Revoked
    );
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);

    let error = f
        .ledger
        .commit_start(&attempt, f.ledger.epoch())
        .unwrap_err();
    assert!(matches!(error, LedgerError::InvalidState(_)), "{error}");
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert!(record.start_committed_at.is_none());
    assert_eq!(record.release_reason.as_deref(), Some("revoked"));
    assert_eq!(
        f.ledger.revoke_offer(&attempt, "again").unwrap(),
        RevokeOutcome::AlreadyReleased
    );

    // The job can be placed again, and only the new attempt can start.
    let next = f.offer(&job, &machine);
    f.accept(&next);
    assert!(matches!(
        f.ledger.commit_start(&attempt, f.ledger.epoch()),
        Err(LedgerError::StaleGeneration { .. })
    ));
    let permit = f.ledger.commit_start(&next, f.ledger.epoch()).unwrap();
    assert!(!permit.already_committed);
    assert_eq!(f.held(&machine), vector(1000, 1));
}

/// A12: after the start is committed, with no acknowledgement ever
/// arriving, nothing but a release proof frees the reservation and no
/// second attempt can be created.
#[test]
fn a12_a_committed_start_is_assumed_to_be_running() {
    let f = fixture();
    let machine = f.machine("builder");
    let spare = f.machine("spare");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);
    let permit = f.ledger.commit_start(&attempt, f.ledger.epoch()).unwrap();
    assert_eq!(permit.start_committed_at, f.now());
    let events_before = f.ledger.last_event_seq().unwrap();

    // No acknowledgement, no event, and the offer's expiry passes.
    f.advance_secs(3600);
    assert!(f.ledger.expire_offers(f.now()).unwrap().is_empty());
    let revoke = f
        .ledger
        .revoke_offer(&attempt, "no acknowledgement")
        .unwrap_err();
    assert!(matches!(revoke, LedgerError::InvalidState(_)));
    assert_eq!(revoke.code(), ErrorCode::InvalidState);
    let reject = f
        .ledger
        .record_offer_reply(&attempt, &rejected())
        .unwrap_err();
    assert!(matches!(reject, LedgerError::InvalidState(_)));

    // No second attempt, on this machine or another.
    for target in [&machine, &spare] {
        assert!(matches!(
            f.try_offer(&job, target, &vector(1000, 1)),
            Err(LedgerError::InvalidState(_))
        ));
    }
    assert_eq!(
        f.ledger.last_event_seq().unwrap(),
        events_before,
        "nothing changed"
    );
    assert_eq!(f.held(&machine), vector(1000, 1));
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert!(record.held && record.start_committed());
    assert!(record.start_delivered_at.is_none());
    assert_eq!(f.ledger.job_attempts(&job).unwrap().len(), 1);
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Offered);

    // Cancelling asks; it does not release.
    assert!(matches!(
        f.ledger.request_cancel(&job).unwrap(),
        CancelOutcome::Requested { .. }
    ));
    assert_eq!(f.held(&machine), vector(1000, 1));

    // Only the worker's proof frees it.
    assert!(matches!(
        f.ledger.record_release(&attempt, &proof()).unwrap(),
        ReleaseOutcome::Released(_)
    ));
    assert!(f.held(&machine).is_zero());
}

#[test]
fn expiry_withdraws_only_offers_without_a_start_permit() {
    let f = fixture();
    let machine = f.machine("builder");
    let jobs: Vec<_> = (0..3).map(|_| f.submit("app")).collect();
    let unanswered = f.offer(&jobs[0], &machine);
    let accepted = f.offer(&jobs[1], &machine);
    f.accept(&accepted);
    let started = f.start(&jobs[2], &machine);

    // Not yet due.
    f.advance_secs(14);
    assert!(f.ledger.expire_offers(f.now()).unwrap().is_empty());
    f.advance_secs(1);
    let expired = f.ledger.expire_offers(f.now()).unwrap();
    let expired_ids: BTreeSet<_> = expired
        .iter()
        .map(|t| t.attempt.attempt_id.clone())
        .collect();
    assert_eq!(
        expired_ids,
        BTreeSet::from([unanswered.attempt_id.clone(), accepted.attempt_id.clone()])
    );
    let target = expired.iter().find(|t| t.attempt == unanswered).unwrap();
    assert_eq!(target.machine_id, machine);
    assert_eq!(target.job_id, jobs[0]);

    assert_eq!(
        f.held(&machine),
        vector(1000, 1),
        "only the started attempt"
    );
    for job in &jobs[..2] {
        let view = f.ledger.job(job).unwrap();
        assert_eq!(view.phase, JobPhase::Queued);
        assert_eq!(
            view.queue_reason.unwrap().code,
            QueueReasonCode::OfferRejected
        );
    }
    assert_eq!(
        f.ledger
            .attempt(&unanswered.attempt_id)
            .unwrap()
            .release_reason
            .as_deref(),
        Some("expired")
    );
    assert!(f.ledger.attempt(&started.attempt_id).unwrap().held);
    assert!(f.ledger.expire_offers(f.now()).unwrap().is_empty());
}

#[test]
fn a_start_needs_an_accepted_unexpired_current_offer() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let epoch = f.ledger.epoch();
    let attempt = f.offer(&job, &machine);

    // Not accepted yet.
    let error = f.ledger.commit_start(&attempt, epoch).unwrap_err();
    assert!(matches!(&error, LedgerError::InvalidState(m) if m.contains("not been accepted")));
    f.accept(&attempt);

    // A wrong epoch, either way, is stale.
    for wrong in [epoch - 1, epoch + 1] {
        let error = f.ledger.commit_start(&attempt, wrong).unwrap_err();
        assert!(matches!(error, LedgerError::StaleEpoch { .. }));
        assert_eq!(error.code(), ErrorCode::StaleGeneration);
    }
    // A wrong generation is stale.
    let wrong = AttemptRef {
        generation: 7,
        ..attempt.clone()
    };
    assert!(matches!(
        f.ledger.commit_start(&wrong, epoch),
        Err(LedgerError::StaleGeneration {
            got: 7,
            current: 1,
            ..
        })
    ));
    assert_eq!(
        f.ledger
            .commit_start(&unknown_attempt(), epoch)
            .unwrap_err()
            .code(),
        ErrorCode::NotFound
    );

    // Expired: the offer can no longer start.
    f.advance_secs(15);
    let error = f.ledger.commit_start(&attempt, epoch).unwrap_err();
    assert!(matches!(&error, LedgerError::InvalidState(m) if m.contains("expired")));
    assert!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .start_committed_at
            .is_none()
    );
    assert_eq!(f.ledger.expire_offers(f.now()).unwrap().len(), 1);

    // A fresh offer starts, and committing twice gives the same permit.
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);
    let permit = f.ledger.commit_start(&attempt, epoch).unwrap();
    assert!(!permit.already_committed);
    assert_eq!(permit.epoch, epoch);
    assert_eq!(permit.job_id, job);
    assert_eq!(permit.machine_id, machine);
    assert_eq!(permit.authority.valid_until, f.now().plus_secs(60));
    assert_eq!(permit.authority.hard_deadline, f.now().plus_secs(600));
    assert_eq!(permit.authority.disconnected_continuation_secs, 120);

    f.advance_secs(5);
    let again = f.ledger.commit_start(&attempt, epoch).unwrap();
    assert!(again.already_committed);
    assert_eq!(again.start_committed_at, permit.start_committed_at);
    assert_eq!(again.authority, permit.authority);
    let committed = f
        .job_event_kinds(&job)
        .iter()
        .filter(|kind| *kind == "attempt.start_committed")
        .count();
    assert_eq!(committed, 1);
    assert!(
        f.ledger
            .job(&job)
            .unwrap()
            .current_attempt()
            .unwrap()
            .start_committed
    );
}

#[test]
fn a_cancelled_job_cannot_start() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);
    assert!(matches!(
        f.ledger.request_cancel(&job).unwrap(),
        CancelOutcome::Cancelled { revoked: Some(_) }
    ));
    assert!(matches!(
        f.ledger.commit_start(&attempt, f.ledger.epoch()),
        Err(LedgerError::InvalidState(_))
    ));
    assert!(f.held(&machine).is_zero());
}

#[test]
fn authority_is_renewed_in_windows_up_to_the_hard_deadline() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let epoch = f.ledger.epoch();
    let attempt = f.start(&job, &machine);
    let started = f.now();
    assert!(f.ledger.record_start_delivered(&attempt).unwrap());
    assert!(!f.ledger.record_start_delivered(&attempt).unwrap());

    // Nothing is due while the first window has more than 20 s left.
    assert!(
        f.ledger
            .attempts_needing_renewal(f.now().plus_secs(20))
            .unwrap()
            .is_empty()
    );
    f.advance_secs(50);
    let due = f
        .ledger
        .attempts_needing_renewal(f.now().plus_secs(20))
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].target.attempt, attempt);
    assert_eq!(due[0].target.machine_id, machine);

    let renewed = f.ledger.renew_authority(&attempt, epoch).unwrap();
    assert_eq!(renewed.valid_until, f.now().plus_secs(60));
    assert_eq!(renewed.hard_deadline, started.plus_secs(600));
    assert!(
        f.ledger
            .attempts_needing_renewal(f.now().plus_secs(20))
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        f.ledger.renew_authority(&attempt, epoch + 1),
        Err(LedgerError::StaleEpoch { .. })
    ));

    // A disconnected machine's attempts are not listed for renewal.
    f.advance_secs(50);
    f.ledger.machine_disconnected(&machine, 1).unwrap();
    assert!(
        f.ledger
            .attempts_needing_renewal(f.now().plus_secs(20))
            .unwrap()
            .is_empty()
    );
    f.ledger.machine_connected(&machine, 2).unwrap();

    // Renewal never passes the hard deadline.
    f.advance_secs(480);
    let last = f.ledger.renew_authority(&attempt, epoch).unwrap();
    assert_eq!(last.valid_until, started.plus_secs(600));

    // A cancelled or released attempt is not renewed.
    f.ledger.request_cancel(&job).unwrap();
    assert!(matches!(
        f.ledger.renew_authority(&attempt, epoch),
        Err(LedgerError::InvalidState(_))
    ));
    assert!(
        f.ledger
            .attempts_needing_renewal(f.now().plus_secs(3600))
            .unwrap()
            .is_empty()
    );
    f.events(&attempt, 1, vec![AttemptEvent::Released { proof: proof() }]);
    assert!(matches!(
        f.ledger.renew_authority(&attempt, epoch),
        Err(LedgerError::InvalidState(_))
    ));
}

#[test]
fn a_start_is_not_committed_for_a_machine_that_cannot_receive_it() {
    let f = fixture();
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.offer(&job, &machine);
    f.accept(&attempt);
    let epoch = f.ledger.epoch();

    // The connection drops between the acceptance and the start.
    f.ledger.machine_disconnected(&machine, 1).unwrap();
    let error = f.ledger.commit_start(&attempt, epoch).unwrap_err();
    assert!(matches!(error, LedgerError::NotReconciled(_)), "{error}");
    assert!(
        f.ledger
            .attempt(&attempt.attempt_id)
            .unwrap()
            .start_committed_at
            .is_none()
    );
    // The offer is still an ordinary offer: it can be withdrawn.
    assert_eq!(f.held(&machine), vector(1000, 1));

    f.ledger.retire_machine(&machine).unwrap();
    let error = f.ledger.commit_start(&attempt, epoch).unwrap_err();
    assert!(matches!(error, LedgerError::InvalidState(_)), "{error}");
    assert_eq!(
        f.ledger.revoke_offer(&attempt, "machine retired").unwrap(),
        RevokeOutcome::Revoked
    );
    assert!(f.held(&machine).is_zero());
    assert_eq!(f.ledger.job(&job).unwrap().phase, JobPhase::Queued);
}
