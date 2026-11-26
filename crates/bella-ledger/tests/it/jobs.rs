use crate::common::{capacity, fixture, spec, vector};
use bella_core::ErrorCode;
use bella_core::api::{ListJobsQuery, ProjectPolicy, Role, SessionInfo};
use bella_core::ids::{Digest, JobId, SessionId, SnapshotId};
use bella_core::job::{JobPhase, Outcome, Priority, QueueReason, QueueReasonCode};
use bella_ledger::LedgerError;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Barrier, Mutex};

#[test]
fn a_submitted_job_is_queued_with_a_frozen_spec() {
    let f = fixture();
    let request = f.request("app", spec());
    let view = f.ledger.submit_job(&f.dev, None, &request).unwrap();
    assert_eq!(view.phase, JobPhase::Queued);
    assert_eq!(view.project_name, "app");
    assert_eq!(view.principal_id, f.dev);
    assert_eq!(view.recipe, "build");
    assert_eq!(view.envelope, request.spec.envelope);
    assert!(view.outcome.is_none());
    assert!(!view.cancel_requested);
    assert!(view.attempts.is_empty());
    assert!(view.spec.is_none(), "the spec is only returned by `job`");
    assert_eq!(view.created_at, f.now());

    let full = f.ledger.job(&view.id).unwrap();
    assert_eq!(full.spec.as_ref(), Some(&request.spec));
    assert_eq!(f.job_event_kinds(&view.id), ["job.queued"]);
    let newest = f.ledger.last_event_seq().unwrap();
    assert_eq!(full.last_event, newest);
}

#[test]
fn the_same_key_and_request_return_the_same_job() {
    let f = fixture();
    let request = f.request("app", spec());
    let first = f
        .ledger
        .submit_job(&f.dev, Some("key-1"), &request)
        .unwrap();
    let events = f.ledger.last_event_seq().unwrap();
    f.advance_secs(30);
    let second = f
        .ledger
        .submit_job(&f.dev, Some("key-1"), &request)
        .unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(second.created_at, first.created_at);
    assert_eq!(f.ledger.job_counts().unwrap().queued, 1);
    assert_eq!(f.ledger.last_event_seq().unwrap(), events, "no new event");

    // A different key, or no key, is a different job.
    let other = f
        .ledger
        .submit_job(&f.dev, Some("key-2"), &request)
        .unwrap();
    assert_ne!(other.id, first.id);
    let unkeyed = f.ledger.submit_job(&f.dev, None, &request).unwrap();
    assert_ne!(unkeyed.id, first.id);
    assert_eq!(f.ledger.job_counts().unwrap().queued, 3);
}

#[test]
fn the_same_key_with_a_different_request_is_an_error() {
    let f = fixture();
    let request = f.request("app", spec());
    let first = f
        .ledger
        .submit_job(&f.dev, Some("key-1"), &request)
        .unwrap();

    let mut changed = request.clone();
    changed.spec.timeout_secs += 1;
    let error = f
        .ledger
        .submit_job(&f.dev, Some("key-1"), &changed)
        .unwrap_err();
    assert!(matches!(&error, LedgerError::IdempotencyMismatch { key } if key == "key-1"));
    assert_eq!(error.code(), ErrorCode::IdempotencyMismatch);
    assert_eq!(f.ledger.job_counts().unwrap().queued, 1);

    // Keys belong to the principal that used them.
    let other = f
        .ledger
        .create_principal("other", Role::Developer, "hash-of-other-token")
        .unwrap()
        .id;
    let theirs = f
        .ledger
        .submit_job(&other, Some("key-1"), &changed)
        .unwrap();
    assert_ne!(theirs.id, first.id);

    // The original request still resolves to the original job, even after
    // that job has moved on.
    f.ledger.request_cancel(&first.id).unwrap();
    let replay = f
        .ledger
        .submit_job(&f.dev, Some("key-1"), &request)
        .unwrap();
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.phase, JobPhase::Finished);
}

#[test]
fn submission_validates_what_it_depends_on() {
    let f = fixture();
    let mut request = f.request("app", spec());

    let mut unknown_snapshot = request.clone();
    unknown_snapshot.snapshot_id = SnapshotId(Digest::of_bytes(b"never registered"));
    let error = f
        .ledger
        .submit_job(&f.dev, None, &unknown_snapshot)
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);

    let mut no_steps = request.clone();
    no_steps.spec.steps.clear();
    assert!(matches!(
        f.ledger.submit_job(&f.dev, None, &no_steps),
        Err(LedgerError::Invalid(_))
    ));

    let mut bad_envelope = request.clone();
    bad_envelope.spec.envelope.initial = vector(64_000, 1);
    assert!(matches!(
        f.ledger.submit_job(&f.dev, None, &bad_envelope),
        Err(LedgerError::Invalid(_))
    ));

    assert!(matches!(
        f.ledger.submit_job(&f.dev, Some(""), &request),
        Err(LedgerError::Invalid(_))
    ));

    // Urgent work needs a project that allows it.
    request.spec.priority = Priority::Urgent;
    let error = f.ledger.submit_job(&f.dev, None, &request).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyDenied);
    let allowing = ProjectPolicy {
        allow_urgent: true,
        ..ProjectPolicy::default()
    };
    f.ledger
        .update_project_policy(&f.admin, "app", &allowing)
        .unwrap();
    let urgent = f.ledger.submit_job(&f.dev, None, &request).unwrap();
    assert_eq!(urgent.priority, Priority::Urgent);

    // A revoked principal submits nothing.
    f.ledger.revoke_principal(&f.dev).unwrap();
    let error = f.ledger.submit_job(&f.dev, None, &request).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unauthorized);
    assert_eq!(f.ledger.job_counts().unwrap().queued, 1);
}

/// A01: 120 submissions from 16 threads, with distinct and repeated
/// idempotency keys, create exactly one job per key and no duplicate ids;
/// a storm of offers for those jobs never over-reserves a machine.
#[test]
fn a01_concurrent_submissions_and_offers_stay_consistent() {
    const THREADS: usize = 16;
    const SUBMISSIONS: usize = 120;
    const KEYS: usize = 40;

    let f = fixture();
    let request = f.request("app", spec());
    let machines = [f.machine("alpha"), f.machine("beta")];
    let by_key: Mutex<BTreeMap<usize, BTreeSet<JobId>>> = Mutex::new(BTreeMap::new());
    let barrier = Barrier::new(THREADS);

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let (f, request, by_key, barrier) = (&f, &request, &by_key, &barrier);
            scope.spawn(move || {
                barrier.wait();
                for n in (thread..SUBMISSIONS).step_by(THREADS) {
                    let key = n % KEYS;
                    let job = f
                        .ledger
                        .submit_job(&f.dev, Some(&format!("key-{key}")), request)
                        .unwrap();
                    by_key
                        .lock()
                        .unwrap()
                        .entry(key)
                        .or_default()
                        .insert(job.id);
                }
            });
        }
    });

    let by_key = by_key.into_inner().unwrap();
    assert_eq!(by_key.len(), KEYS);
    let mut ids = BTreeSet::new();
    for (key, jobs) in &by_key {
        assert_eq!(jobs.len(), 1, "key {key} produced {} jobs", jobs.len());
        ids.extend(jobs.iter().cloned());
    }
    assert_eq!(ids.len(), KEYS, "no job id is shared between keys");
    assert_eq!(f.ledger.job_counts().unwrap().queued, KEYS as u32);
    assert_eq!(f.ledger.queued_jobs(1000).unwrap().len(), KEYS);

    // Every thread tries to place every job on both machines at once. Each
    // machine has room for four 2-CPU, 4 GiB reservations.
    let jobs: Vec<JobId> = ids.into_iter().collect();
    let reservation = vector(2000, 4);
    let granted = Mutex::new(Vec::new());
    let barrier = Barrier::new(THREADS);
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let (f, jobs, machines, reservation, granted, barrier) =
                (&f, &jobs, &machines, &reservation, &granted, &barrier);
            scope.spawn(move || {
                barrier.wait();
                for (index, job) in jobs.iter().enumerate() {
                    let machine = &machines[(index + thread) % machines.len()];
                    let offer = f.ledger.create_offer(
                        job,
                        machine,
                        reservation,
                        &capacity(),
                        None,
                        f.expiry(),
                    );
                    match offer {
                        Ok(offer) => granted.lock().unwrap().push((job.clone(), offer)),
                        Err(
                            LedgerError::Capacity { .. }
                            | LedgerError::InvalidState(_)
                            | LedgerError::UnresolvedAttempt { .. },
                        ) => {}
                        Err(other) => panic!("unexpected offer error: {other}"),
                    }
                }
            });
        }
    });

    let granted = granted.into_inner().unwrap();
    assert_eq!(granted.len(), 8, "two machines, four reservations each");
    let offered_jobs: BTreeSet<&JobId> = granted.iter().map(|(job, _)| job).collect();
    assert_eq!(offered_jobs.len(), 8, "no job was offered twice");
    for machine in &machines {
        let view = f.ledger.machine_view(machine).unwrap();
        assert_eq!(view.reserved, vector(8000, 16));
        assert!(view.reserved.fits_within(&capacity()));
        assert_eq!(view.attempts.len(), 4);
    }
    for (job, offer) in &granted {
        assert_eq!(offer.attempt.generation, 1);
        assert_eq!(f.ledger.job_attempts(job).unwrap().len(), 1);
    }
    let counts = f.ledger.job_counts().unwrap();
    assert_eq!((counts.offered, counts.queued), (8, KEYS as u32 - 8));
}

#[test]
fn jobs_are_listed_by_project_session_and_activity() {
    let f = fixture();
    let session = SessionId::generate();
    let mut request = f.request("app", spec());
    request.session = Some(SessionInfo {
        id: session.clone(),
        label: Some("fix the build".into()),
        agent: Some("claude".into()),
        origin_machine: None,
        thread_ref: None,
    });
    let in_session = f.ledger.submit_job(&f.dev, None, &request).unwrap();
    assert_eq!(in_session.session, request.session);
    let app = f.submit("app");
    let lib = f.submit("lib");
    f.ledger.request_cancel(&app).unwrap();

    let ids = |query: ListJobsQuery| -> Vec<JobId> {
        f.ledger
            .list_jobs(&query)
            .unwrap()
            .into_iter()
            .map(|job| job.id)
            .collect()
    };
    // Newest first.
    assert_eq!(
        ids(ListJobsQuery::default()),
        [lib.clone(), app.clone(), in_session.id.clone()]
    );
    let by_project = ListJobsQuery {
        project: Some("app".into()),
        ..ListJobsQuery::default()
    };
    assert_eq!(ids(by_project), [app.clone(), in_session.id.clone()]);
    let by_session = ListJobsQuery {
        session: Some(session),
        ..ListJobsQuery::default()
    };
    assert_eq!(ids(by_session), std::slice::from_ref(&in_session.id));
    let active = ListJobsQuery {
        active: Some(true),
        ..ListJobsQuery::default()
    };
    assert_eq!(ids(active), [lib.clone(), in_session.id.clone()]);
    let limited = ListJobsQuery {
        limit: Some(1),
        ..ListJobsQuery::default()
    };
    assert_eq!(ids(limited), [lib]);
    let unknown = ListJobsQuery {
        project: Some("nothing".into()),
        ..ListJobsQuery::default()
    };
    assert!(ids(unknown).is_empty());
    assert!(matches!(
        f.ledger.job(&JobId::generate()),
        Err(LedgerError::NotFound(_))
    ));
}

#[test]
fn the_queue_is_in_submission_order_with_project_shares() {
    let f = fixture();
    let machine = f.machine("builder");
    let first = f.submit("app");
    f.advance_secs(10);
    let second = f.submit("lib");
    f.advance_secs(10);
    let third = f.submit("app");
    let running = f.submit("app");
    f.ledger
        .create_offer(
            &running,
            &machine,
            &vector(2000, 4),
            &capacity(),
            None,
            f.expiry(),
        )
        .unwrap();
    f.ledger
        .update_project_policy(
            &f.admin,
            "lib",
            &ProjectPolicy {
                weight: 300,
                ..ProjectPolicy::default()
            },
        )
        .unwrap();

    let queue = f.ledger.queue_view().unwrap();
    let order: Vec<_> = queue
        .entries
        .iter()
        .map(|e| (e.position, e.job.id.clone()))
        .collect();
    assert_eq!(order, [(1, first.clone()), (2, second.clone()), (3, third)]);
    assert_eq!(queue.entries[0].waiting_ms, 20_000);
    assert_eq!(queue.entries[1].waiting_ms, 10_000);

    let shares: BTreeMap<_, _> = queue.shares.iter().map(|s| (s.name.as_str(), s)).collect();
    assert_eq!(shares.len(), 2);
    assert_eq!((shares["app"].queued, shares["app"].running), (2, 1));
    assert_eq!(shares["app"].weight, 100);
    assert_eq!(shares["app"].reserved, vector(2000, 4));
    assert_eq!((shares["lib"].queued, shares["lib"].running), (1, 0));
    assert_eq!(shares["lib"].weight, 300);
    assert!(shares["lib"].reserved.is_zero());

    let queued = f.ledger.queued_jobs(2).unwrap();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0].job_id, first);
    assert_eq!(queued[0].project_name, "app");
    assert_eq!(queued[0].started_attempts, 0);
    assert!(queued[0].last_machine.is_none());
    assert!(queued[0].submit_seq < queued[1].submit_seq);
    assert_eq!(queued[1].job_id, second);
    assert_eq!(queued[0].spec, spec());

    let overview = f.ledger.fleet_overview("0.1.0", false).unwrap();
    assert_eq!(overview.fleet_id, *f.ledger.fleet_id());
    assert_eq!(overview.controller_epoch, 1);
    assert_eq!(overview.server_time, f.now());
    assert_eq!(overview.machines.len(), 1);
    assert_eq!(
        (overview.queued, overview.running, overview.environments),
        (3, 0, 0)
    );
}

#[test]
fn queue_reasons_apply_only_to_queued_jobs() {
    let f = fixture();
    let machine = f.machine("builder");
    let waiting = f.submit("app");
    let offered = f.submit("app");
    f.offer(&offered, &machine);
    let reason = QueueReason::new(QueueReasonCode::WaitingForCapacity, "all machines are busy");

    assert!(f.ledger.set_queue_reason(&waiting, &reason).unwrap());
    assert!(!f.ledger.set_queue_reason(&offered, &reason).unwrap());
    assert_eq!(
        f.ledger.job(&waiting).unwrap().queue_reason,
        Some(reason.clone())
    );
    assert!(f.ledger.job(&offered).unwrap().queue_reason.is_none());
    let updated = f
        .ledger
        .set_queue_reasons(&[(waiting.clone(), reason.clone()), (offered, reason)])
        .unwrap();
    assert_eq!(updated, 1);

    // The reason goes away when the job leaves the queue.
    f.offer(&waiting, &machine);
    assert!(f.ledger.job(&waiting).unwrap().queue_reason.is_none());
}

#[test]
fn counts_follow_the_phases() {
    let f = fixture();
    let machine = f.machine("builder");
    let jobs: Vec<_> = (0..5).map(|_| f.submit("app")).collect();
    f.offer(&jobs[0], &machine);
    let running = f.start(&jobs[1], &machine);
    f.events(
        &running,
        1,
        vec![bella_core::protocol::AttemptEvent::Preparing],
    );
    let done = f.start(&jobs[2], &machine);
    f.finish(&done, Outcome::Succeeded);
    f.ledger.request_cancel(&jobs[3]).unwrap();

    let counts = f.ledger.job_counts().unwrap();
    assert_eq!(counts.queued, 1);
    assert_eq!(counts.offered, 1);
    assert_eq!(counts.preparing, 1);
    assert_eq!(counts.finished, 2);
    assert_eq!(counts.active(), 2);
    assert_eq!(f.ledger.fleet_overview("0.1.0", true).unwrap().running, 1);
}
