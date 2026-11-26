//! The dispatch loop: the only place offers are created.
//!
//! A pass rebuilds the scheduler's input from the ledger, asks
//! `bella-scheduler` what to offer, writes the reasons for everything that
//! stays queued, and turns each planned offer into a ledger reservation
//! followed by an `Offer` command. No transaction is open while a worker is
//! awaited: the reservation is committed, then the offer is sent, and the
//! worker's answer is handled by its connection.

use crate::fleet::fleet_state;
use crate::state::App;
use bella_core::ids::JobId;
use bella_core::job::QueueReason;
use bella_core::protocol::ControllerCommand;
use bella_ledger::LedgerError;
use bella_scheduler::{PlannedOffer, Schedule};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Start-up recovery, then dispatch passes until shutdown.
pub(crate) async fn run(app: App) {
    while let Err(error) = recover(&app).await {
        warn!(%error, "start-up recovery failed; retrying");
        tokio::select! {
            () = app.shutdown.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
    app.reconciling.store(false, Ordering::SeqCst);
    app.changed();
    info!(epoch = app.epoch, "start-up recovery finished; dispatching");

    let mut tick = tokio::time::interval(app.tuning.scheduler_tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Reasons as last written, so an unchanged queue costs no write.
    let mut written: HashMap<JobId, QueueReason> = HashMap::new();
    loop {
        tokio::select! {
            () = app.shutdown.cancelled() => return,
            () = app.dispatch_kicked() => {}
            _ = tick.tick() => {}
        }
        if let Err(error) = pass(&app, &mut written).await {
            warn!(%error, "dispatch pass failed");
        }
        // A burst of changes is served by the next pass, not one pass each.
        tokio::select! {
            () = app.shutdown.cancelled() => return,
            () = tokio::time::sleep(app.tuning.dispatch_min_interval) => {}
        }
    }
}

/// Withdraw every offer a previous controller epoch left without a start
/// permit. Their workers were never told to start, so nothing can be
/// running; a worker that still holds such an offer is told to forget it
/// when it reconciles. Offers with a start permit are untouched: they stay
/// held until their worker accounts for them.
async fn recover(app: &App) -> Result<(), LedgerError> {
    let epoch = app.epoch;
    let withdrawn = app
        .db(move |ledger| {
            let mut withdrawn = 0u32;
            for machine in ledger.machine_views()? {
                for attempt in ledger.machine_attempts(&machine.id)? {
                    if attempt.may_be_withdrawn() && attempt.offer_epoch < epoch {
                        ledger.revoke_offer(
                            &attempt.reference(),
                            "the controller restarted before the offer was started",
                        )?;
                        withdrawn += 1;
                    }
                }
            }
            Ok(withdrawn)
        })
        .await?;
    if withdrawn > 0 {
        info!(withdrawn, "withdrew offers left by the previous controller");
    }
    Ok(())
}

async fn pass(app: &App, written: &mut HashMap<JobId, QueueReason>) -> Result<(), LedgerError> {
    let shared = app.clone();
    let schedule: Schedule = app
        .db(move |ledger| {
            let state = fleet_state(&shared, ledger)?;
            Ok(bella_scheduler::schedule(&state))
        })
        .await?;

    let changed: Vec<(JobId, QueueReason)> = schedule
        .reasons
        .iter()
        .filter(|(job, reason)| written.get(*job) != Some(*reason))
        .map(|(job, reason)| (job.clone(), reason.clone()))
        .collect();
    if !changed.is_empty() {
        let reasons = changed.clone();
        app.db(move |ledger| ledger.set_queue_reasons(&reasons))
            .await?;
        written.extend(changed);
    }
    // A job that left the queue gets its reason written afresh if it returns.
    written.retain(|job, _| schedule.reasons.contains_key(job));

    let mut offered = false;
    for planned in schedule.offers {
        offered |= offer(app, planned).await;
    }
    if offered {
        app.events.touch();
    }
    Ok(())
}

/// Reserve and send one planned offer. Returns whether a reservation was
/// made. A refusal by the ledger (a capacity race, a machine that is no
/// longer reconciled, a job cancelled meanwhile) leaves the job queued for
/// the next pass.
async fn offer(app: &App, planned: PlannedOffer) -> bool {
    let expires_at = app.now().plus(app.tuning.offer_expiry);
    let PlannedOffer {
        job_id,
        machine_id,
        reservation,
        capacity,
        max_concurrent,
    } = planned;
    let (job, machine) = (job_id.clone(), machine_id.clone());
    let reserved = app
        .db(move |ledger| {
            ledger.create_offer(
                &job,
                &machine,
                &reservation,
                &capacity,
                max_concurrent,
                expires_at,
            )
        })
        .await;
    let record = match reserved {
        Ok(record) => record,
        Err(error) => {
            debug!(%job_id, %machine_id, %error, "offer not reserved; the job stays queued");
            return false;
        }
    };
    info!(
        job_id = %record.job_id,
        attempt_id = %record.attempt.attempt_id,
        generation = record.attempt.generation,
        machine_id = %record.machine_id,
        snapshot_id = %record.snapshot_id,
        expires_at = record.expires_at.0,
        "reserved; sending Offer"
    );
    let command = ControllerCommand::Offer {
        offer: Box::new(record.to_offer()),
    };
    if let Err(error) = app
        .hub
        .send(&record.machine_id, command, app.epoch, app.now())
    {
        // The machine's connection went away between the scheduling pass
        // and now. Nobody can answer the offer, so it is withdrawn at once.
        info!(
            job_id = %record.job_id,
            attempt_id = %record.attempt.attempt_id,
            generation = record.attempt.generation,
            machine_id = %record.machine_id,
            %error,
            "offer could not be sent; revoking"
        );
        let attempt = record.attempt.clone();
        let reason = format!("the offer could not be sent: {error}");
        if let Err(error) = app
            .db(move |ledger| ledger.revoke_offer(&attempt, &reason))
            .await
        {
            warn!(%error, attempt_id = %record.attempt.attempt_id, "could not revoke an unsent offer");
        }
        app.note_rejection(&record.job_id, &record.machine_id);
        app.kick_dispatch();
    }
    true
}
