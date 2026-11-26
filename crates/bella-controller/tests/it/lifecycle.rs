//! Jobs from submission to release, through the real HTTP API and a
//! scripted worker on the real WebSocket.

use crate::common::*;
use bella_core::api::{
    ArtifactView, ExplainResponse, JobView, LogPage, WaitResponse,
};
use bella_core::ids::{AttemptId, Digest, JobId, MachineId};
use bella_core::job::{CleanupState, JobPhase, Outcome, QueueReasonCode};
use bella_core::machine::AvailabilityFlag;
use bella_core::protocol::{
    ArtifactKind, AttemptEvent, AttemptRef, AttemptReport, ControllerCommand, ControllerMsg,
    LocalAttemptState, OfferDecision, ReconcileAction, StepState,
};
use bella_core::snapshot::Manifest;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

/// How long a test listens to be sure that something does not happen.
const QUIET: Duration = Duration::from_millis(600);

fn current(job: &JobView) -> &bella_core::api::AttemptView {
    job.current_attempt().expect("the job has an attempt")
}

/// Read messages for `limit` and return the offers among them.
async fn offers_within(worker: &mut FakeWorker, limit: Duration) -> Vec<(bella_core::ids::CommandId, bella_core::protocol::Offer)> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut offers = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return offers;
        }
        match worker.poll(left).await {
            Some(ControllerMsg::Command { envelope }) => match envelope.command {
                ControllerCommand::Offer { offer } => offers.push((envelope.command_id, *offer)),
                other => worker.skipped.push(ControllerMsg::Command {
                    envelope: bella_core::protocol::CommandEnvelope {
                        command: other,
                        ..envelope
                    },
                }),
            },
            Some(other) => worker.skipped.push(other),
            None => return offers,
        }
    }
}

/// Assert that `worker` is sent no `Start` and no `Offer` for a while.
async fn assert_no_start_or_offer(worker: &mut FakeWorker, what: &str) {
    let deadline = tokio::time::Instant::now() + QUIET;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        match worker.poll(left).await {
            Some(ControllerMsg::Command { envelope }) => match &envelope.command {
                ControllerCommand::Start { .. } | ControllerCommand::Offer { .. } => {
                    panic!("{what}: unexpected {:?}", envelope.command)
                }
                _ => {}
            },
            Some(_) => {}
            None => return,
        }
    }
}

fn revoke_for(message: &ControllerMsg, attempt: &AttemptRef) -> bool {
    matches!(message, ControllerMsg::Command { envelope }
        if matches!(&envelope.command, ControllerCommand::Revoke { attempt: a, .. } if a == attempt))
}

/// The whole path: enrollment, connection, submission, offer, start, logs,
/// an artifact, the outcome and the release, all readable over the API.
#[tokio::test]
async fn happy_path_from_enrollment_to_release() {
    let harness = Harness::start().await;
    let mut worker = harness.worker("builder").await;
    let machine_api = harness.api(&worker.token);
    let developer = harness.developer().await;

    let mut job_spec = spec();
    job_spec.outputs.artifacts = vec!["out/app".into()];
    let job = harness.submit(&developer, "app", job_spec).await;
    assert_eq!(job.phase, JobPhase::Queued);

    let offer = worker.start_next().await;
    assert_eq!(offer.job_id, job.id);
    let attempt = offer.attempt.clone();
    worker
        .events(
            &attempt,
            vec![
                AttemptEvent::Preparing,
                AttemptEvent::StepChanged {
                    step: "compile".into(),
                    state: StepState::Running,
                    exit_code: None,
                },
            ],
        )
        .await;
    let running = harness.job(&job.id).await;
    assert_eq!(running.phase, JobPhase::Executing);
    assert!(current(&running).start_committed);

    worker.log(&attempt, "compile", b"hello from the build\n").await;
    worker.log(&attempt, "compile", b"second line\n").await;

    let artifact = b"\x7fELF built program".to_vec();
    let digest = Digest::of_bytes(&artifact);
    machine_api.put_object(&digest, &artifact).await;
    worker
        .events(
            &attempt,
            vec![
                AttemptEvent::StepChanged {
                    step: "compile".into(),
                    state: StepState::Succeeded,
                    exit_code: Some(0),
                },
                AttemptEvent::Collecting,
                AttemptEvent::ArtifactPublished {
                    path: "out/app".into(),
                    kind: ArtifactKind::Artifact,
                    digest: digest.clone(),
                    size: artifact.len() as u64,
                },
                exited(Outcome::Succeeded),
            ],
        )
        .await;
    // The outcome is known while the reservation is still held.
    let exited_job = harness.job(&job.id).await;
    assert_eq!(exited_job.outcome, Some(Outcome::Succeeded));
    assert!(!harness.machine(&worker.machine_id).await.reserved.is_zero());

    worker.event(&attempt, released()).await;
    let waited: WaitResponse = developer
        .get(&format!("/jobs/{}/wait?timeout_ms=10000", job.id))
        .await;
    assert!(!waited.timed_out);
    assert_eq!(waited.job.outcome, Some(Outcome::Succeeded));
    let done = harness
        .job_until(&job.id, "the release", |job| job.phase == JobPhase::Finished)
        .await;
    assert_eq!(current(&done).cleanup, CleanupState::Released);

    let logs: LogPage = developer.get(&format!("/jobs/{}/logs", job.id)).await;
    let text: String = logs.entries.iter().map(|entry| entry.text.as_str()).collect();
    assert_eq!(text, "hello from the build\nsecond line\n");
    assert!(!logs.truncated);

    let artifacts: Vec<ArtifactView> = developer.get(&format!("/jobs/{}/artifacts", job.id)).await;
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].path, "out/app");
    assert_eq!(artifacts[0].digest, digest);
    let fetched = developer
        .raw(reqwest::Method::GET, &format!("/objects/{digest}?raw=1"))
        .await
        .bytes()
        .await
        .unwrap();
    assert_eq!(&fetched[..], &artifact[..]);

    let machine = harness.machine(&worker.machine_id).await;
    assert!(machine.reserved.is_zero());
    assert!(machine.attempts.is_empty());
    let explain: ExplainResponse = developer.get(&format!("/jobs/{}/explain", job.id)).await;
    assert_eq!(explain.phase, JobPhase::Finished);

    // A wait on a finished job answers at once; one that times out on a
    // running job cancels nothing.
    let other = harness.submit(&developer, "app", spec()).await;
    let offer = worker.start_next().await;
    assert_eq!(offer.job_id, other.id);
    let waited: WaitResponse = developer
        .get(&format!("/jobs/{}/wait?timeout_ms=200", other.id))
        .await;
    assert!(waited.timed_out);
    let still = harness.job(&other.id).await;
    assert!(!still.cancel_requested);
    assert!(still.outcome.is_none());
}

/// A01: concurrent submissions with repeated idempotency keys make one job
/// per key, and two workers taking every offer are never reserved beyond
/// their budget.
#[tokio::test]
async fn a01_idempotent_concurrent_submissions_never_over_reserve() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let snapshot = harness
        .snapshot(&developer, "app", &[("main.rs", &b"fn main() {}\n"[..])])
        .await;
    let mut worker_a = harness.worker("builder-a").await;
    let mut worker_b = harness.worker("builder-b").await;
    // Each machine fits three of these at once (48 GiB of budget).
    let job_spec = spec_with_memory(13);
    let keys = 40;
    let mut tasks = Vec::new();
    for n in 0..120 {
        let api = developer.clone();
        let body = request("app", snapshot.clone(), job_spec.clone());
        let key = format!("key-{}", n % keys);
        tasks.push(tokio::spawn(async move {
            let job = api.submit(&body, Some(&key)).await.expect("submitted");
            (key, job.id)
        }));
    }
    let mut by_key: HashMap<String, HashSet<JobId>> = HashMap::new();
    for task in tasks {
        let (key, id) = task.await.unwrap();
        by_key.entry(key).or_default().insert(id);
    }
    assert_eq!(by_key.len(), keys);
    assert!(by_key.values().all(|ids| ids.len() == 1), "{by_key:?}");
    let jobs: HashSet<JobId> = by_key.into_values().flatten().collect();
    assert_eq!(jobs.len(), keys);

    // The same key with a different body is refused.
    let mut different = job_spec.clone();
    different.timeout_secs += 1;
    let refusal = developer
        .submit(&request("app", snapshot.clone(), different), Some("key-0"))
        .await
        .unwrap_err();
    assert_eq!(refusal.error.code, bella_core::ErrorCode::IdempotencyMismatch);

    // Run every job to completion on the two workers, checking after every
    // offer that neither machine is reserved beyond what it reported.
    let budget = worker_a.budget.clone();
    let mut finished: HashSet<JobId> = HashSet::new();
    let mut held: HashMap<MachineId, usize> = HashMap::new();
    let mut job_of: HashMap<AttemptId, JobId> = HashMap::new();
    let mut max_held = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while finished.len() < keys {
        assert!(tokio::time::Instant::now() < deadline, "only {} jobs finished", finished.len());
        for worker in [&mut worker_a, &mut worker_b] {
            let Some(message) = worker.poll(Duration::from_millis(20)).await else {
                continue;
            };
            let ControllerMsg::Command { envelope } = message else {
                continue;
            };
            match envelope.command {
                ControllerCommand::Offer { offer } => {
                    let count = held.entry(worker.machine_id.clone()).or_default();
                    *count += 1;
                    max_held = max_held.max(*count);
                    assert!(*count <= 3, "more offers held than fit on the machine");
                    let view = harness.machine(&worker.machine_id).await;
                    assert!(
                        view.reserved.fits_within(&budget),
                        "over-reserved: {:?} of {:?}",
                        view.reserved,
                        budget
                    );
                    job_of.insert(offer.attempt.attempt_id.clone(), offer.job_id.clone());
                    worker.accept(&envelope.command_id, &offer).await;
                }
                ControllerCommand::Start { attempt, .. } => {
                    worker.ack(&envelope.command_id).await;
                    worker.send_event(&attempt, AttemptEvent::Preparing).await;
                    worker.send_event(&attempt, exited(Outcome::Succeeded)).await;
                    worker.send_event(&attempt, released()).await;
                    *held.get_mut(&worker.machine_id).unwrap() -= 1;
                    finished.insert(job_of[&attempt.attempt_id].clone());
                }
                _ => {}
            }
        }
    }
    assert!(max_held >= 2, "the machines were used in parallel");
    for id in &jobs {
        harness
            .job_until(id, "every job to finish", |job| job.phase == JobPhase::Finished)
            .await;
    }
    for id in [&worker_a.machine_id, &worker_b.machine_id] {
        let machine = harness
            .machine_until(id, "the releases", |machine| machine.reserved.is_zero())
            .await;
        assert!(machine.attempts.is_empty());
    }
}

/// A02: two jobs that each need most of a machine's memory: one is
/// offered, the other stays queued with a reason.
#[tokio::test]
async fn a02_two_jobs_compete_for_the_last_memory() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let mut worker = harness.worker("builder").await;
    let first = harness.submit(&developer, "app", spec_with_memory(30)).await;
    let second = harness.submit(&developer, "app", spec_with_memory(30)).await;

    let offers = offers_within(&mut worker, Duration::from_secs(2)).await;
    assert_eq!(offers.len(), 1, "exactly one of the two jobs fits");
    let offered = offers[0].1.job_id.clone();
    let waiting = if offered == first.id { &second.id } else { &first.id };
    assert_eq!(offers[0].1.reservation.memory_bytes, 30 * GIB);

    let queued = harness
        .job_until(waiting, "a queue reason", |job| job.queue_reason.is_some())
        .await;
    assert_eq!(queued.phase, JobPhase::Queued);
    let reason = queued.queue_reason.unwrap();
    assert_eq!(reason.code, QueueReasonCode::WaitingForCapacity, "{reason:?}");
    let explain: ExplainResponse = developer.get(&format!("/jobs/{waiting}/explain")).await;
    assert_eq!(explain.reason.code, QueueReasonCode::WaitingForCapacity);
    assert_eq!(explain.position, Some(1));
    assert!(!explain.suggestions.is_empty());
    let machine = harness.machine(&worker.machine_id).await;
    assert_eq!(machine.reserved.memory_bytes, 30 * GIB);

    // Once the first is rejected the second gets its turn.
    let (command_id, offer) = &offers[0];
    worker
        .reply(
            command_id,
            offer,
            OfferDecision::Rejected {
                code: QueueReasonCode::MemoryPressure,
                message: "busy".into(),
            },
        )
        .await;
    let (_, next) = worker.expect_offer().await;
    assert_eq!(&next.job_id, waiting);
}

/// A37: the job runs against the snapshot registered when it was
/// submitted, whatever is registered later.
#[tokio::test]
async fn a37_the_offer_names_the_snapshot_registered_at_submission() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let before = harness
        .snapshot(&developer, "app", &[("main.rs", &b"version one\n"[..])])
        .await;
    let job: JobView = developer
        .post("/jobs", &request("app", before.clone(), spec()))
        .await;
    assert_eq!(job.snapshot_id, before);
    // The author keeps editing.
    let after = harness
        .snapshot(&developer, "app", &[("main.rs", &b"version two\n"[..])])
        .await;
    assert_ne!(before, after);

    let mut worker = harness.worker("builder").await;
    let (_, offer) = worker.expect_offer().await;
    assert_eq!(offer.job_id, job.id);
    assert_eq!(offer.snapshot_id, before);
    assert_eq!(&offer.manifest_digest, before.digest());
    // What the worker fetches is exactly the submitted tree.
    let manifest_bytes = harness
        .api(&worker.token)
        .raw(
            reqwest::Method::GET,
            &format!("/objects/{}?raw=1", offer.manifest_digest),
        )
        .await
        .bytes()
        .await
        .unwrap();
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest.id(), before);
    assert_eq!(
        manifest.digests().into_iter().collect::<Vec<_>>(),
        vec![Digest::of_bytes(b"version one\n")]
    );

    // A job naming a snapshot that was never registered is refused.
    let unknown = Manifest::new(Vec::new()).id();
    let refusal = developer
        .try_post::<JobView>("/jobs", &request("app", unknown, spec()))
        .await
        .unwrap_err();
    assert!(refusal.status == 404 || refusal.status == 422, "{refusal:?}");
}

/// A13: an offer that expires or is rejected before it starts puts the job
/// back in the queue, and a late acceptance of the expired offer starts
/// nothing.
#[tokio::test]
async fn a13_expired_or_rejected_offers_requeue_and_late_accepts_start_nothing() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let mut worker = harness.worker("builder").await;
    let job = harness.submit(&developer, "app", spec()).await;
    let (command_id, offer) = worker.expect_offer().await;

    harness.advance(Duration::from_secs(16));
    worker.heartbeat().await;
    let revoke = worker
        .expect("the expiry's Revoke", |message| {
            revoke_for(message, &offer.attempt).then_some(())
        })
        .await;
    let _ = revoke;
    let requeued = harness
        .job_until(&job.id, "the job back in the queue", |job| {
            job.phase == JobPhase::Queued
        })
        .await;
    assert_eq!(requeued.attempts.len(), 1);
    assert!(!requeued.attempts[0].start_committed);
    assert!(harness.machine(&worker.machine_id).await.reserved.is_zero());

    // The worker answers too late: it is told to drop the offer, and no
    // Start follows.
    worker.accept(&command_id, &offer).await;
    worker
        .expect("a Revoke for the late acceptance", |message| {
            revoke_for(message, &offer.attempt).then_some(())
        })
        .await;
    assert_no_start_or_offer(&mut worker, "after a late acceptance").await;
    let still = harness.job(&job.id).await;
    assert_eq!(still.phase, JobPhase::Queued);
    assert!(still.attempts.iter().all(|attempt| !attempt.start_committed));

    // After the back-off the job is offered again, with a new generation;
    // this time it is rejected and goes back to the queue once more.
    harness.advance(Duration::from_secs(31));
    worker.heartbeat().await;
    let (command_id, second) = worker.expect_offer().await;
    assert_eq!(second.job_id, job.id);
    assert!(second.attempt.generation > offer.attempt.generation);
    worker
        .reply(
            &command_id,
            &second,
            OfferDecision::Rejected {
                code: QueueReasonCode::ForegroundProtection,
                message: "the user is busy".into(),
            },
        )
        .await;
    let requeued = harness
        .job_until(&job.id, "the rejection", |job| {
            job.phase == JobPhase::Queued && job.attempts.len() == 2
        })
        .await;
    assert!(requeued.attempts.iter().all(|attempt| !attempt.start_committed));
    assert!(harness.machine(&worker.machine_id).await.reserved.is_zero());
}

/// A12: the worker accepts, the controller commits the start, and the
/// worker never acknowledges it and goes away. The attempt is assumed to
/// be running: it stays held, becomes uncertain, and no second attempt is
/// made anywhere.
#[tokio::test]
async fn a12_a_lost_start_acknowledgement_keeps_the_attempt_held() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let mut lost = harness.worker("lost").await;
    let job = harness.submit(&developer, "app", spec()).await;
    let (command_id, offer) = lost.expect_offer().await;
    lost.accept(&command_id, &offer).await;
    let _ = lost.expect_start(&offer.attempt).await;
    let lost_id = lost.machine_id.clone();
    lost.disconnect().await;

    let mut other = harness.worker("other").await;
    harness.advance(Duration::from_secs(31));
    other.heartbeat().await;
    let machine = harness
        .machine_until(&lost_id, "the unreachable flag", |machine| {
            machine.flags.contains(&AvailabilityFlag::Unreachable)
        })
        .await;
    assert_eq!(machine.reserved.memory_bytes, GIB);
    let held = harness
        .job_until(&job.id, "the attempt to become uncertain", |job| {
            current(job).uncertain
        })
        .await;
    assert_eq!(held.attempts.len(), 1);
    assert!(current(&held).start_committed);
    assert_ne!(held.phase, JobPhase::Queued);
    assert!(held.outcome.is_none());
    assert_no_start_or_offer(&mut other, "a second attempt of a job that may be running").await;
    assert_eq!(harness.job(&job.id).await.attempts.len(), 1);
}

/// A14: a machine whose heartbeats stop is flagged stale, then
/// unreachable. Its reservation stays held and it is offered nothing new
/// until it is heard from again.
#[tokio::test]
async fn a14_silent_machines_are_flagged_but_keep_their_reservations() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let mut worker = harness.worker("builder").await;
    let running = harness.submit(&developer, "app", spec()).await;
    let offer = worker.start_next().await;
    worker.event(&offer.attempt, AttemptEvent::Preparing).await;

    harness.advance(Duration::from_secs(16));
    let stale = harness
        .machine_until(&worker.machine_id, "the stale flag", |machine| {
            machine.flags.contains(&AvailabilityFlag::Stale)
        })
        .await;
    assert!(!stale.flags.contains(&AvailabilityFlag::Unreachable));
    assert_eq!(stale.reserved.memory_bytes, GIB);

    let waiting = harness.submit(&developer, "app", spec()).await;
    assert_no_start_or_offer(&mut worker, "a stale machine").await;
    let queued = harness
        .job_until(&waiting.id, "a queue reason", |job| job.queue_reason.is_some())
        .await;
    assert_eq!(queued.phase, JobPhase::Queued);

    harness.advance(Duration::from_secs(15));
    let unreachable = harness
        .machine_until(&worker.machine_id, "the unreachable flag", |machine| {
            machine.flags.contains(&AvailabilityFlag::Unreachable)
        })
        .await;
    assert_eq!(unreachable.reserved.memory_bytes, GIB);
    let uncertain = harness
        .job_until(&running.id, "the attempt to become uncertain", |job| {
            current(job).uncertain
        })
        .await;
    assert!(uncertain.outcome.is_none());
    assert_no_start_or_offer(&mut worker, "an unreachable machine").await;

    // Heard from again: the flags clear and the queued job is offered.
    worker.heartbeat().await;
    let (_, next) = worker.expect_offer().await;
    assert_eq!(next.job_id, waiting.id);
    let machine = harness.machine(&worker.machine_id).await;
    assert!(!machine.flags.contains(&AvailabilityFlag::Stale));
    assert!(!machine.flags.contains(&AvailabilityFlag::Unreachable));
}

/// A15 and A60: after a controller restart the epoch is new, a
/// reconnecting worker is reconciled before it is offered anything, and an
/// event from the attempt the restart made obsolete never sets the job's
/// result.
#[tokio::test]
async fn a15_a60_restart_reconciles_before_offers_and_fences_old_generations() {
    let mut harness = Harness::start().await;
    let developer_token = harness.token(bella_core::api::Role::Developer).await;
    let developer = harness.api(&developer_token);
    let enrolled = harness.enroll("builder", 16, 64).await;
    let (mut worker, _) = FakeWorker::connect(&harness, &enrolled, boot(1), 1, Vec::new()).await;
    worker.heartbeat().await;
    let first_epoch = worker.epoch;
    let job = harness.submit(&developer, "app", spec()).await;
    let offer = worker.start_next().await;
    let old = offer.attempt.clone();
    worker.event(&old, AttemptEvent::Preparing).await;

    harness.restart().await;
    let developer = harness.api(&developer_token);
    let reason = worker.expect_close().await;
    assert!(reason.contains("shutting down"), "{reason}");
    assert_eq!(harness.handle().epoch(), first_epoch + 1);
    eventually("start-up recovery", || async {
        (!harness.fleet().await.reconciling).then_some(())
    })
    .await;
    // Disconnected and not reconciled: nothing is dispatched to it.
    let machine = harness.machine(&enrolled.machine_id).await;
    assert!(!machine.connected);
    assert_eq!(machine.reserved.memory_bytes, GIB, "the started attempt stays held");

    // The machine rebooted, so its processes cannot have survived. The
    // handshake reconciles first: Welcome and Reconcile are the first two
    // messages, before any offer.
    let (mut worker, verdicts) =
        FakeWorker::connect(&harness, &enrolled, boot(2), 2, Vec::new()).await;
    assert_eq!(worker.epoch, first_epoch + 1);
    assert!(verdicts.is_empty(), "nothing reported, nothing to rule on");
    worker.heartbeat().await;
    let (command_id, retry) = worker.expect_offer().await;
    assert_eq!(retry.job_id, job.id);
    assert!(retry.attempt.generation > old.generation);
    let interrupted = harness.job(&job.id).await;
    let first = interrupted
        .attempts
        .iter()
        .find(|attempt| attempt.id == old.attempt_id)
        .unwrap();
    assert!(matches!(first.outcome, Some(Outcome::InfraInterrupted { .. })), "{first:?}");

    // A late event from the old generation claims success. It is
    // acknowledged so the worker stops resending it, and changes nothing.
    worker.send_event_at(&old, 2, exited(Outcome::Succeeded)).await;
    worker.expect_events_acked(&old, 2).await;
    assert!(harness.job(&job.id).await.outcome.is_none());

    // The current generation decides.
    worker.accept(&command_id, &retry).await;
    let (start_id, _) = worker.expect_start(&retry.attempt).await;
    worker.ack(&start_id).await;
    worker
        .finish(
            &retry.attempt,
            Outcome::Failed {
                step: Some("compile".into()),
                exit_code: Some(2),
            },
        )
        .await;
    let done = harness
        .job_until(&job.id, "the job to finish", |job| job.phase == JobPhase::Finished)
        .await;
    assert!(matches!(done.outcome, Some(Outcome::Failed { .. })), "{done:?}");
    let _ = developer;
}

/// A worker that reconnects on the same boot still running a started
/// attempt is told to continue; one holding an offer the restart withdrew
/// is told to forget it.
#[tokio::test]
async fn a60_reconcile_verdicts_after_a_restart() {
    let mut harness = Harness::start().await;
    let developer_token = harness.token(bella_core::api::Role::Developer).await;
    let enrolled = harness.enroll("builder", 16, 64).await;
    let (mut worker, _) = FakeWorker::connect(&harness, &enrolled, boot(1), 1, Vec::new()).await;
    worker.heartbeat().await;
    let developer = harness.api(&developer_token);
    let running = harness.submit(&developer, "app", spec()).await;
    let started = worker.start_next().await;
    worker.event(&started.attempt, AttemptEvent::Preparing).await;
    let pending = harness.submit(&developer, "app", spec()).await;
    let (_, offered) = worker.expect_offer().await;
    assert_eq!(offered.job_id, pending.id);

    harness.restart().await;
    let _ = worker.expect_close().await;
    eventually("start-up recovery", || async {
        (!harness.fleet().await.reconciling).then_some(())
    })
    .await;
    // The unstarted offer was withdrawn at start-up.
    harness
        .job_until(&pending.id, "the withdrawn offer", |job| {
            job.phase == JobPhase::Queued
        })
        .await;

    let report = |offer: &bella_core::protocol::Offer, state, seq| AttemptReport {
        attempt: offer.attempt.clone(),
        job_id: offer.job_id.clone(),
        state,
        last_event_seq: seq,
        reserved: offer.reservation.clone(),
        usage: None,
    };
    let (mut worker, verdicts) = FakeWorker::connect(
        &harness,
        &enrolled,
        boot(1),
        2,
        vec![
            report(&started, LocalAttemptState::Running, 1),
            report(&offered, LocalAttemptState::OfferHeld, 0),
        ],
    )
    .await;
    let actions: BTreeMap<AttemptId, ReconcileAction> = verdicts
        .into_iter()
        .map(|verdict| (verdict.attempt.attempt_id, verdict.action))
        .collect();
    assert_eq!(actions[&started.attempt.attempt_id], ReconcileAction::Continue);
    assert_eq!(actions[&offered.attempt.attempt_id], ReconcileAction::Forget);

    // The started attempt carries on and finishes normally.
    worker.heartbeat().await;
    worker.finish(&started.attempt, Outcome::Succeeded).await;
    let done = harness
        .job_until(&running.id, "the job to finish", |job| job.phase == JobPhase::Finished)
        .await;
    assert_eq!(done.outcome, Some(Outcome::Succeeded));
}

/// A22: when an environment's lease ends the worker is told to finish it,
/// and its reservation is released only on the worker's release proof. A
/// renewal that is refused leaves the deadline as it was.
#[tokio::test]
async fn a22_environment_lease_expiry_finishes_and_releases_on_proof() {
    let harness = Harness::start_with(|config| {
        // Long silences are part of this test; liveness is not.
        config.tuning.telemetry_stale_after = Duration::from_secs(86_400);
        config.tuning.unreachable_after = Duration::from_secs(86_400);
    })
    .await;
    let developer = harness.developer().await;
    let mut worker = harness.worker("builder").await;
    let mut job_spec = spec();
    job_spec.environment = Some(bella_core::job::LeaseSpec {
        idle_timeout_secs: 900,
        lease_secs: 1800,
        max_lifetime_secs: 14_400,
    });
    let job = harness.submit(&developer, "app", job_spec).await;
    let offer = worker.start_next().await;
    let attempt = offer.attempt.clone();
    worker
        .events(
            &attempt,
            vec![
                AttemptEvent::Preparing,
                AttemptEvent::EnvironmentReady {
                    endpoints: vec![bella_core::protocol::Endpoint {
                        step: "server".into(),
                        host: "127.0.0.1".into(),
                        port: 8080,
                    }],
                },
            ],
        )
        .await;
    let environments: Vec<bella_core::api::EnvironmentView> =
        developer.get("/environments").await;
    assert_eq!(environments.len(), 1);
    let environment = environments[0].clone();
    assert_eq!(environment.job_id, job.id);
    assert_eq!(environment.state, bella_core::api::EnvironmentState::Ready);

    // Renewal moves the deadline from now.
    harness.advance(Duration::from_secs(600));
    let renewed: bella_core::api::EnvironmentView = developer
        .action(&format!("/environments/{}/renew", environment.id))
        .await;
    assert!(renewed.effective_deadline > environment.effective_deadline);

    // The idle timeout passes: FinishEnvironment is sent.
    harness.advance(Duration::from_secs(901));
    worker
        .expect_command("FinishEnvironment", |command| match command {
            ControllerCommand::FinishEnvironment { attempt: a, .. } if *a == attempt => Some(()),
            _ => None,
        })
        .await;
    let finishing: bella_core::api::EnvironmentView =
        developer.get(&format!("/environments/{}", environment.id)).await;
    assert_eq!(finishing.state, bella_core::api::EnvironmentState::Finishing);
    // A renewal now is refused and changes nothing.
    let refused = developer
        .try_action::<bella_core::api::EnvironmentView>(&format!(
            "/environments/{}/renew",
            environment.id
        ))
        .await;
    assert!(refused.is_err(), "a finishing environment cannot be renewed");
    let unchanged: bella_core::api::EnvironmentView =
        developer.get(&format!("/environments/{}", environment.id)).await;
    assert_eq!(unchanged.effective_deadline, finishing.effective_deadline);
    assert_eq!(unchanged.lease, finishing.lease);

    // Until the worker proves the cleanup the capacity stays held, and the
    // request is repeated.
    assert_eq!(harness.machine(&worker.machine_id).await.reserved.memory_bytes, GIB);
    harness.advance(Duration::from_secs(6));
    worker
        .expect_command("FinishEnvironment again", |command| match command {
            ControllerCommand::FinishEnvironment { attempt: a, .. } if *a == attempt => Some(()),
            _ => None,
        })
        .await;
    assert_eq!(harness.machine(&worker.machine_id).await.reserved.memory_bytes, GIB);

    worker.event(&attempt, exited(Outcome::Succeeded)).await;
    assert_eq!(harness.machine(&worker.machine_id).await.reserved.memory_bytes, GIB);
    worker.event(&attempt, released()).await;
    harness
        .machine_until(&worker.machine_id, "the release", |machine| machine.reserved.is_zero())
        .await;
    let ended: bella_core::api::EnvironmentView =
        developer.get(&format!("/environments/{}", environment.id)).await;
    assert_eq!(ended.state, bella_core::api::EnvironmentState::Ended);
    let done = harness
        .job_until(&job.id, "the job to finish", |job| job.phase == JobPhase::Finished)
        .await;
    assert_eq!(done.outcome, Some(Outcome::Succeeded));
}

/// A59: an attempt reports success, but the artifact it announced never
/// reached the store. The outcome is recorded; the artifact is listed only
/// once its object exists.
#[tokio::test]
async fn a59_an_announced_artifact_is_listed_only_once_published() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;
    let mut worker = harness.worker("builder").await;
    let job = harness.submit(&developer, "app", spec()).await;
    let offer = worker.start_next().await;
    let content = b"report contents".to_vec();
    let digest = Digest::of_bytes(&content);
    worker
        .events(
            &offer.attempt,
            vec![
                AttemptEvent::Preparing,
                AttemptEvent::ArtifactPublished {
                    path: "report.xml".into(),
                    kind: ArtifactKind::TestReport,
                    digest: digest.clone(),
                    size: content.len() as u64,
                },
                exited(Outcome::Succeeded),
                released(),
            ],
        )
        .await;
    let done = harness
        .job_until(&job.id, "the job to finish", |job| job.phase == JobPhase::Finished)
        .await;
    assert_eq!(done.outcome, Some(Outcome::Succeeded));
    let listed: Vec<ArtifactView> = developer.get(&format!("/jobs/{}/artifacts", job.id)).await;
    assert!(listed.is_empty(), "an artifact without its object is not listed");

    harness.api(&worker.token).put_object(&digest, &content).await;
    let listed: Vec<ArtifactView> = developer.get(&format!("/jobs/{}/artifacts", job.id)).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].digest, digest);
    assert_eq!(listed[0].kind, ArtifactKind::TestReport);
}

#[tokio::test]
async fn cancelling_queued_offered_and_running_jobs() {
    let harness = Harness::start().await;
    let developer = harness.developer().await;

    // Queued: nothing is reserved, so it ends at once.
    let queued = harness.submit(&developer, "app", spec_with_memory(500)).await;
    let cancelled: JobView = developer.action(&format!("/jobs/{}/cancel", queued.id)).await;
    assert_eq!(cancelled.phase, JobPhase::Finished);
    assert_eq!(cancelled.outcome, Some(Outcome::Cancelled));

    // Offered: the offer is withdrawn and the worker told so.
    let mut worker = harness.worker("builder").await;
    let offered = harness.submit(&developer, "app", spec()).await;
    let (command_id, offer) = worker.expect_offer().await;
    let cancelled: JobView = developer.action(&format!("/jobs/{}/cancel", offered.id)).await;
    assert_eq!(cancelled.phase, JobPhase::Finished);
    assert_eq!(cancelled.outcome, Some(Outcome::Cancelled));
    worker
        .expect("the Revoke", |message| revoke_for(message, &offer.attempt).then_some(()))
        .await;
    // Accepting it now starts nothing.
    worker.accept(&command_id, &offer).await;
    assert_no_start_or_offer(&mut worker, "a cancelled offer").await;
    assert!(harness.machine(&worker.machine_id).await.reserved.is_zero());

    // Running: a request, repeated until the worker shows cleanup, also
    // across a reconnect, and finished only by the release proof.
    let running = harness.submit(&developer, "app", spec()).await;
    let offer = worker.start_next().await;
    worker.event(&offer.attempt, AttemptEvent::Preparing).await;
    let requested: JobView = developer.action(&format!("/jobs/{}/cancel", running.id)).await;
    assert!(requested.cancel_requested);
    assert_ne!(requested.phase, JobPhase::Finished);
    let is_cancel = |command: &ControllerCommand| match command {
        ControllerCommand::Cancel { attempt, .. } if *attempt == offer.attempt => Some(()),
        _ => None,
    };
    worker.expect_command("Cancel", is_cancel).await;
    harness.advance(Duration::from_secs(6));
    worker.heartbeat().await;
    worker.expect_command("Cancel again", is_cancel).await;

    let enrolled = Enrolled {
        machine_id: worker.machine_id.clone(),
        token: worker.token.clone(),
        inventory: worker.inventory.clone(),
    };
    worker.disconnect().await;
    let (mut worker, verdicts) = FakeWorker::connect(
        &harness,
        &enrolled,
        boot(1),
        2,
        vec![AttemptReport {
            attempt: offer.attempt.clone(),
            job_id: running.id.clone(),
            state: LocalAttemptState::Running,
            last_event_seq: 1,
            reserved: offer.reservation.clone(),
            usage: None,
        }],
    )
    .await;
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].action, ReconcileAction::Abort);
    worker.heartbeat().await;
    harness.advance(Duration::from_secs(6));
    worker.expect_command("Cancel after the reconnect", is_cancel).await;
    assert_eq!(harness.machine(&worker.machine_id).await.reserved.memory_bytes, GIB);

    worker.send_event_at(&offer.attempt, 2, AttemptEvent::Cleanup {
        state: CleanupState::ProcessesStopped,
        detail: "stopped".into(),
    })
    .await;
    worker.expect_events_acked(&offer.attempt, 2).await;
    worker.send_event_at(&offer.attempt, 3, exited(Outcome::Cancelled)).await;
    worker.send_event_at(&offer.attempt, 4, released()).await;
    worker.expect_events_acked(&offer.attempt, 4).await;
    let done = harness
        .job_until(&running.id, "the cancelled job to finish", |job| {
            job.phase == JobPhase::Finished
        })
        .await;
    assert_eq!(done.outcome, Some(Outcome::Cancelled));
    assert!(harness.machine(&worker.machine_id).await.reserved.is_zero());
}

/// The first frame must be a `Hello` in a protocol version this controller
/// speaks, naming the machine the token belongs to; anything else is
/// closed with the reason.
#[tokio::test]
async fn the_handshake_refuses_a_wrong_first_frame() {
    use bella_core::machine::LocalControl;
    use bella_core::protocol::{PROTOCOL_VERSION, WorkerMsg};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let harness = Harness::start().await;
    let enrolled = harness.enroll("builder", 4, 8).await;
    let hello = |protocol_version, machine_id: MachineId| WorkerMsg::Hello {
        protocol_version,
        machine_id,
        boot_id: boot(1),
        connection_generation: 1,
        known_epoch: 0,
        inventory: Box::new(enrolled.inventory.clone()),
        policy_revision: 1,
        local_control: LocalControl::default(),
        attempts: Vec::new(),
    };
    let firsts = [
        serde_json::json!({ "type": "heartbeat" }).to_string(),
        serde_json::to_string(&hello(PROTOCOL_VERSION + 1, enrolled.machine_id.clone())).unwrap(),
        serde_json::to_string(&hello(PROTOCOL_VERSION, MachineId::generate())).unwrap(),
    ];
    for first in firsts {
        let mut socket = open_socket(harness.addr(), None, &enrolled.token).await.unwrap();
        socket.send(Message::text(first.clone())).await.unwrap();
        let mut reason = None;
        while let Some(Ok(frame)) = socket.next().await {
            match frame {
                Message::Text(text) => match serde_json::from_str(text.as_str()) {
                    Ok(ControllerMsg::Close { reason: given }) => reason = Some(given),
                    _ => panic!("nothing but Close may follow a bad first frame: {text}"),
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
        assert!(reason.is_some(), "the controller says why it refuses {first}");
    }
    assert!(!harness.machine(&enrolled.machine_id).await.connected);
}

/// One connection per machine: a newer one replaces the older, which is
/// told why.
#[tokio::test]
async fn a_second_connection_replaces_the_first() {
    let harness = Harness::start().await;
    let enrolled = harness.enroll("builder", 16, 64).await;
    let (mut first, _) = FakeWorker::connect(&harness, &enrolled, boot(1), 1, Vec::new()).await;
    let (mut second, _) = FakeWorker::connect(&harness, &enrolled, boot(1), 2, Vec::new()).await;
    let reason = first.expect_close().await;
    assert!(reason.contains("replaced"), "{reason}");
    second.heartbeat().await;
    let developer = harness.developer().await;
    let job = harness.submit(&developer, "app", spec()).await;
    let (_, offer) = second.expect_offer().await;
    assert_eq!(offer.job_id, job.id);
    assert!(harness.machine(&enrolled.machine_id).await.connected);
}
