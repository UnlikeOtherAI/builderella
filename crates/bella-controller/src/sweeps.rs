//! Periodic work: offer expiry, authority renewal, liveness, command
//! retransmission, environment leases and object garbage collection.
//!
//! Every sweep decides from the ledger and the controller's clock, so
//! running one twice, late, or after a restart converges on the same state.

use crate::error::Failure;
use crate::state::{App, PendingCommand, lock};
use bella_core::api::{EnvironmentState, EnvironmentView};
use bella_core::defaults::AUTHORITY_WINDOW;
use bella_core::ids::{AttemptId, Digest, EnvironmentId};
use bella_core::job::CleanupState;
use bella_core::lease::LeaseEnd;
use bella_core::machine::{AvailabilityFlag, MachineLifecycle};
use bella_core::protocol::ControllerCommand;
use bella_core::snapshot::Manifest;
use bella_core::store::{ObjectStore, get_bytes};
use bella_ledger::{AttemptTarget, LedgerError};
use bella_store::GcReport;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{debug, info, warn};

fn interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period.max(Duration::from_millis(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// The frequent sweeps. Each runs on its own interval.
pub(crate) async fn run(app: App) {
    let mut sweep = interval(app.tuning.sweep_interval);
    let mut authority = interval(app.tuning.authority_renew_interval);
    loop {
        tokio::select! {
            () = app.shutdown.cancelled() => return,
            _ = sweep.tick() => {
                report("offer expiry", expire_offers(&app).await);
                report("liveness", liveness(&app).await);
                report("cancellation", cancellations(&app).await);
                report("environment leases", environment_leases(&app).await);
            }
            _ = authority.tick() => report("authority renewal", renew_authority(&app).await),
        }
    }
}

/// Garbage collection, on its own task because a pass over a large store
/// takes a while.
pub(crate) async fn run_gc(app: App) {
    let period = app.tuning.gc_interval.max(Duration::from_millis(1));
    let mut due = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    due.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = app.shutdown.cancelled() => return,
            _ = due.tick() => {
                if let Err(failure) = collect_garbage(&app).await {
                    warn!(?failure, "object garbage collection failed");
                }
            }
        }
    }
}

fn report(sweep: &str, result: Result<(), LedgerError>) {
    if let Err(error) = result {
        warn!(sweep, %error, "sweep failed");
    }
}

/// Withdraw offers nobody answered in time and tell their workers.
async fn expire_offers(app: &App) -> Result<(), LedgerError> {
    let now = app.now();
    let expired = app.db(move |ledger| ledger.expire_offers(now)).await?;
    if expired.is_empty() {
        return Ok(());
    }
    for target in expired {
        info!(
            job_id = %target.job_id,
            attempt_id = %target.attempt.attempt_id,
            generation = target.attempt.generation,
            machine_id = %target.machine_id,
            "offer expired; job queued again"
        );
        app.note_rejection(&target.job_id, &target.machine_id);
        let revoke = ControllerCommand::Revoke {
            attempt: target.attempt,
            reason: "the offer expired".into(),
        };
        // A machine that is gone learns of it when it reconciles.
        let _ = app.hub.send(&target.machine_id, revoke, app.epoch, now);
    }
    app.changed();
    Ok(())
}

/// Renew the execution authority of every started attempt on a machine
/// that can be told about it.
async fn renew_authority(app: &App) -> Result<(), LedgerError> {
    let before = app.now().plus(AUTHORITY_WINDOW);
    let epoch = app.epoch;
    let shared = app.clone();
    let renewed = app
        .db(move |ledger| {
            let mut renewed = Vec::new();
            for due in ledger.attempts_needing_renewal(before)? {
                // A renewal that cannot be delivered would only make the
                // ledger believe in authority the worker does not have.
                if shared.hub.ready(&due.target.machine_id).is_none() {
                    continue;
                }
                match ledger.renew_authority(&due.target.attempt, epoch) {
                    Ok(authority) => renewed.push((due.target, authority)),
                    Err(error) => {
                        debug!(attempt_id = %due.target.attempt.attempt_id, %error, "authority not renewed");
                    }
                }
            }
            Ok(renewed)
        })
        .await?;
    for (target, authority) in renewed {
        debug!(
            job_id = %target.job_id,
            attempt_id = %target.attempt.attempt_id,
            generation = target.attempt.generation,
            machine_id = %target.machine_id,
            valid_until = authority.valid_until.0,
            "authority renewed"
        );
        let renew = ControllerCommand::RenewAuthority {
            attempt: target.attempt,
            authority,
        };
        let _ = app.hub.send(&target.machine_id, renew, epoch, app.now());
    }
    Ok(())
}

/// Flag machines that have gone quiet. Stale machines get no new offers;
/// unreachable ones also have their attempts marked uncertain. Neither
/// releases anything: silence proves nothing about the processes.
async fn liveness(app: &App) -> Result<(), LedgerError> {
    let now = app.now();
    let stale_after = app.tuning.telemetry_stale_after;
    let unreachable_after = app.tuning.unreachable_after;
    let started_at = app.started_at;
    let shared = app.clone();
    let changed = app
        .db(move |ledger| {
            let mut changed = false;
            for view in ledger.machine_views()? {
                if view.lifecycle != MachineLifecycle::Ready {
                    continue;
                }
                // A machine that has never connected has nothing to lose
                // contact with.
                let Some(seen) = view.last_seen_at else {
                    continue;
                };
                // Machines get a full window to come back after a
                // controller restart before they are flagged.
                let silent = now.since(seen.max(started_at));
                let stale = silent >= stale_after;
                let unreachable = silent >= unreachable_after;
                let flagged_stale = view.flags.contains(&AvailabilityFlag::Stale);
                let flagged_unreachable = view.flags.contains(&AvailabilityFlag::Unreachable);
                if stale || unreachable {
                    lock(&shared.out_of_contact).insert(view.id.clone());
                }
                if stale && !flagged_stale {
                    changed |= ledger.set_machine_condition(&view.id, AvailabilityFlag::Stale, true)?;
                    warn!(machine_id = %view.id, silent_ms = silent.as_millis() as u64, "machine is stale: no new offers");
                }
                if unreachable && !flagged_unreachable {
                    changed |=
                        ledger.set_machine_condition(&view.id, AvailabilityFlag::Unreachable, true)?;
                    let held = ledger.mark_machine_uncertain(&view.id)?;
                    warn!(
                        machine_id = %view.id,
                        silent_ms = silent.as_millis() as u64,
                        held_attempts = held.len(),
                        "machine is unreachable: its reservations stay held and its attempts are uncertain"
                    );
                    for target in held {
                        info!(
                            job_id = %target.job_id,
                            attempt_id = %target.attempt.attempt_id,
                            generation = target.attempt.generation,
                            machine_id = %target.machine_id,
                            "attempt is uncertain"
                        );
                    }
                }
                if !stale && view.connected && (flagged_stale || flagged_unreachable) {
                    // Heard from again, but the flags were not cleared by a
                    // heartbeat (they date from before a restart).
                    changed |= ledger.set_machine_condition(&view.id, AvailabilityFlag::Stale, false)?;
                    changed |=
                        ledger.set_machine_condition(&view.id, AvailabilityFlag::Unreachable, false)?;
                    lock(&shared.out_of_contact).remove(&view.id);
                }
            }
            Ok(changed)
        })
        .await?;
    if changed {
        app.changed();
    }
    Ok(())
}

/// Send `Cancel` for a started attempt whose job is being cancelled, now.
pub(crate) async fn send_cancel(app: &App, target: &AttemptTarget, reason: &str) {
    let machine = target.machine_id.clone();
    let grace_secs = match app.db(move |ledger| ledger.machine_policy(&machine)).await {
        Ok(policy) => policy.cleanup_grace_secs,
        Err(_) => bella_core::defaults::TERMINATE_GRACE.as_secs(),
    };
    let cancel = ControllerCommand::Cancel {
        attempt: target.attempt.clone(),
        reason: reason.to_string(),
        grace_secs,
    };
    let now = app.now();
    let sent = app.hub.send(&target.machine_id, cancel, app.epoch, now);
    info!(
        job_id = %target.job_id,
        attempt_id = %target.attempt.attempt_id,
        generation = target.attempt.generation,
        machine_id = %target.machine_id,
        delivered = sent.is_ok(),
        "cancel requested"
    );
    let mut pending = lock(&app.pending_cancels);
    let entry = pending
        .entry(target.attempt.attempt_id.clone())
        .or_insert_with(|| PendingCommand {
            target: target.clone(),
            reason: reason.to_string(),
            last_sent: None,
        });
    if sent.is_ok() {
        entry.last_sent = Some(now);
    }
}

/// Keep sending `Cancel` until the attempt reports that it is cleaning up
/// or its reservation is released. A worker that reconnects in between is
/// also told to abort by its reconcile verdict.
async fn cancellations(app: &App) -> Result<(), LedgerError> {
    let pending: Vec<PendingCommand> = lock(&app.pending_cancels).values().cloned().collect();
    let now = app.now();
    for command in pending {
        let attempt_id = command.target.attempt.attempt_id.clone();
        let id = attempt_id.clone();
        let settled = match app.db(move |ledger| ledger.attempt(&id)).await {
            Ok(record) => !record.held || record.cleanup > CleanupState::Requested,
            Err(LedgerError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if settled {
            lock(&app.pending_cancels).remove(&attempt_id);
            continue;
        }
        let due = command
            .last_sent
            .is_none_or(|sent| now.since(sent) >= app.tuning.command_resend_interval);
        if due && app.hub.ready(&command.target.machine_id).is_some() {
            send_cancel(app, &command.target, &command.reason).await;
        }
    }
    Ok(())
}

/// Ask for an environment to finish and tell its worker. Its resources stay
/// reserved until the worker's release proof arrives.
pub(crate) async fn finish_environment(
    app: &App,
    environment: &EnvironmentId,
    reason: &str,
) -> Result<EnvironmentView, LedgerError> {
    let id = environment.clone();
    let text = reason.to_string();
    let finish = app
        .db(move |ledger| ledger.request_environment_finish(&id, &text))
        .await?;
    if let Some(target) = finish.target {
        send_finish(app, &target, reason);
    }
    app.changed();
    Ok(finish.environment)
}

fn send_finish(app: &App, target: &AttemptTarget, reason: &str) {
    let now = app.now();
    let command = ControllerCommand::FinishEnvironment {
        attempt: target.attempt.clone(),
        reason: reason.to_string(),
    };
    let sent = app.hub.send(&target.machine_id, command, app.epoch, now);
    info!(
        job_id = %target.job_id,
        attempt_id = %target.attempt.attempt_id,
        generation = target.attempt.generation,
        machine_id = %target.machine_id,
        delivered = sent.is_ok(),
        reason,
        "environment finish requested"
    );
    if sent.is_ok() {
        lock(&app.finish_sent).insert(target.attempt.attempt_id.clone(), now);
    }
}

fn lease_end_reason(end: LeaseEnd) -> &'static str {
    match end {
        LeaseEnd::IdleTimeout => "the environment was idle for its whole idle timeout",
        LeaseEnd::LeaseExpired => "the environment's lease expired",
        LeaseEnd::MaxLifetime => "the environment reached its maximum lifetime",
    }
}

/// Finish environments whose lease has ended, and keep telling the worker
/// of every environment that is finishing until it lets go.
async fn environment_leases(app: &App) -> Result<(), LedgerError> {
    let now = app.now();
    let expired = app
        .db(move |ledger| ledger.expired_environments(now))
        .await?;
    for due in expired {
        finish_environment(app, &due.environment.id, lease_end_reason(due.reason)).await?;
    }

    let live = app.db(|ledger| ledger.environments(false)).await?;
    let finishing: Vec<&EnvironmentView> = live
        .iter()
        .filter(|environment| environment.state == EnvironmentState::Finishing)
        .collect();
    let attempts: HashSet<&AttemptId> = finishing.iter().map(|e| &e.attempt_id).collect();
    lock(&app.finish_sent).retain(|attempt, _| attempts.contains(attempt));
    for environment in finishing {
        let last = lock(&app.finish_sent).get(&environment.attempt_id).copied();
        let due = last.is_none_or(|sent| now.since(sent) >= app.tuning.command_resend_interval);
        if !due || app.hub.ready(&environment.machine_id).is_none() {
            continue;
        }
        let id = environment.id.clone();
        let finish = app
            .db(move |ledger| ledger.request_environment_finish(&id, "finish requested"))
            .await?;
        if let Some(target) = finish.target {
            send_finish(app, &target, "finish requested");
        }
    }
    Ok(())
}

/// Delete objects that nothing refers to and that are older than the grace
/// period. Live objects are the manifests of snapshots still in use, every
/// object those manifests list, and every recorded artifact. When a live
/// manifest cannot be read the pass collects nothing and fails, because
/// the objects it lists cannot be identified.
///
/// Snapshot registration and job submission are held off while this runs,
/// so neither can come to rely on an object this pass is about to remove:
/// they either see it gone and refuse, or commit first and make it live.
pub(crate) async fn collect_garbage(app: &App) -> Result<GcReport, Failure> {
    let _exclusive = app.gc_gate.write().await;
    lock(&app.complete_snapshots).clear();
    let grace = app.tuning.gc_grace;
    let report = app
        .blocking(move |app| {
            let roots = app.ledger.live_digests()?;
            let mut live: HashSet<Digest> = roots.artifacts.into_iter().collect();
            for manifest_digest in roots.snapshot_manifests {
                let manifest = get_bytes(app.store.as_ref(), &manifest_digest)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| {
                        serde_json::from_slice::<Manifest>(&bytes).map_err(|error| error.to_string())
                    });
                match manifest {
                    Ok(manifest) => live.extend(manifest.digests()),
                    Err(error) => {
                        // Its files cannot be named, so nothing can be told
                        // apart from them: collecting now could delete the
                        // source of a live snapshot. Collect nothing.
                        return Err(Failure::internal(format!(
                            "live snapshot manifest {manifest_digest} could not be read ({error}); \
                             no objects were collected"
                        )));
                    }
                }
                live.insert(manifest_digest);
            }
            let report = app.store.gc(&|digest| live.contains(digest), grace)?;
            Ok(report)
        })
        .await?;
    info!(
        removed = report.objects_removed,
        live = report.objects_live,
        in_grace = report.objects_in_grace,
        temps_removed = report.temps_removed,
        bytes_freed = report.bytes_freed,
        failed = report.failed,
        "object garbage collection finished"
    );
    Ok(report)
}

/// Whether every object a stored manifest lists is present. `Err` carries
/// the digests that are missing.
pub(crate) fn missing_objects(
    store: &dyn ObjectStore,
    digests: impl IntoIterator<Item = Digest>,
) -> Result<Vec<Digest>, Failure> {
    let mut missing = Vec::new();
    for digest in digests {
        if !store.contains(&digest)? {
            missing.push(digest);
        }
    }
    Ok(missing)
}
