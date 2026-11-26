//! Reconciliation with a worker after it connects.

use crate::attempts::{self, Release};
use crate::db::{Ctx, exec};
use crate::events::emit;
use crate::{Ledger, Result, jobs, machines};
use bella_core::api::EntityKind;
use bella_core::ids::{AttemptId, BootId, MachineId};
use bella_core::job::{JobPhase, QueueReason, QueueReasonCode};
use bella_core::protocol::{AttemptReport, ReconcileAction, ReconcileVerdict};
use rusqlite::params;
use serde_json::json;
use std::collections::BTreeSet;

fn forget(c: &Ctx<'_>, attempt: &AttemptId, proves_isolation: bool) -> Result<()> {
    attempts::release(
        c,
        attempt,
        Release {
            reason: "forgotten",
            proof: None,
            proves_isolation,
            lost_detail: "",
            queue_reason: Some(QueueReason::new(
                QueueReasonCode::OfferRejected,
                "the offer was dropped during reconciliation before it started",
            )),
        },
    )?;
    Ok(())
}

fn lose(c: &Ctx<'_>, attempt: &AttemptId, reason: &'static str, detail: &str) -> Result<()> {
    attempts::release(
        c,
        attempt,
        Release {
            reason,
            proof: None,
            // Only a reboot shows that nothing of the attempt is left.
            proves_isolation: reason == "reboot",
            lost_detail: detail,
            queue_reason: None,
        },
    )?;
    Ok(())
}

fn reconcile(
    c: &Ctx<'_>,
    machine_id: &MachineId,
    boot_id: &BootId,
    reports: &[AttemptReport],
) -> Result<Vec<ReconcileVerdict>> {
    let machine = machines::load(c, machine_id)?;
    // Without a recorded boot there is nothing to compare with, so a
    // reboot is not proven.
    let boot_changed = machine.boot_id.as_ref().is_some_and(|old| old != boot_id);

    let mut reported = BTreeSet::new();
    let mut verdicts = Vec::with_capacity(reports.len());
    for report in reports {
        let known = attempts::find(c, &report.attempt.attempt_id)?
            .filter(|attempt| attempt.machine_id == *machine_id);
        let action = match known {
            // Not an attempt of this machine: it must not keep running.
            None => ReconcileAction::Abort,
            Some(attempt) => {
                reported.insert(attempt.id.clone());
                let job = jobs::load_row(c, &attempt.job_id)?;
                let current = report.attempt.generation == attempt.generation
                    && attempt.generation == job.generation;
                if !current {
                    ReconcileAction::Abort
                } else if attempt.held && attempt.phase == JobPhase::Quarantined {
                    // Quarantined capacity returns on proof, not on a verdict.
                    attempts::set_uncertain(c, &attempt, false)?;
                    if attempt.start_committed() && !job.cancel_requested {
                        ReconcileAction::Continue
                    } else {
                        ReconcileAction::Abort
                    }
                } else if !attempt.start_committed() {
                    // No start permit was ever committed for this offer.
                    if attempt.held {
                        forget(c, &attempt.id, false)?;
                    }
                    ReconcileAction::Forget
                } else if !attempt.held || job.cancel_requested {
                    // Already released here, or no longer wanted. A wanted
                    // attempt stays held until the worker proves release.
                    attempts::set_uncertain(c, &attempt, false)?;
                    ReconcileAction::Abort
                } else {
                    attempts::set_uncertain(c, &attempt, false)?;
                    ReconcileAction::Continue
                }
            }
        };
        verdicts.push(ReconcileVerdict {
            attempt: report.attempt.clone(),
            action,
        });
    }

    // What the ledger holds on this machine but the worker did not report.
    let mut released = 0u32;
    let mut uncertain = 0u32;
    for attempt in attempts::held_on_machine(c, machine_id)? {
        if reported.contains(&attempt.id) {
            continue;
        }
        let quarantined = attempt.phase == JobPhase::Quarantined;
        if !attempt.start_committed() && (boot_changed || !quarantined) {
            forget(c, &attempt.id, boot_changed)?;
            released += 1;
        } else if boot_changed {
            lose(
                c,
                &attempt.id,
                "reboot",
                "the machine restarted while the attempt was running",
            )?;
            released += 1;
        } else if attempt.start_delivered_at.is_none() && !quarantined {
            lose(
                c,
                &attempt.id,
                "never_delivered",
                "the start permit never reached the machine",
            )?;
            released += 1;
        } else {
            // The start was delivered, the machine did not restart and the
            // worker does not account for the attempt: nothing is proven.
            attempts::set_uncertain(c, &attempt, true)?;
            uncertain += 1;
        }
    }

    exec(
        c.conn,
        "UPDATE machines SET boot_id = ?1, reconciled_epoch = ?2 WHERE id = ?3",
        params![
            boot_id.as_str(),
            crate::db::int(c.epoch)?,
            machine_id.as_str()
        ],
    )?;
    emit(
        c,
        EntityKind::Machine,
        machine_id.as_str(),
        None,
        "machine.reconciled",
        json!({
            "epoch": c.epoch,
            "boot_changed": boot_changed,
            "reported": reports.len(),
            "released": released,
            "uncertain": uncertain,
        }),
    )?;
    Ok(verdicts)
}

impl Ledger {
    /// Reconcile the ledger with what a worker reports after connecting,
    /// and return one verdict per report, in order.
    ///
    /// For each reported attempt:
    ///
    /// * current generation with a start permit → `Continue` (if the worker
    ///   still only holds the offer, the start never arrived: send it
    ///   again; [`Ledger::commit_start`] returns the existing permit);
    /// * an offer without a start permit → its reservation is released,
    ///   the job is queued again, and the verdict is `Forget`;
    /// * unknown to the ledger, an old generation, already released here,
    ///   or cancel-requested → `Abort`. A cancel-requested attempt stays
    ///   held until the worker proves its release.
    ///
    /// For each attempt the ledger holds on the machine that the worker
    /// did not report:
    ///
    /// * no start permit → released, job queued again;
    /// * `boot_id` differs from the recorded one (its processes cannot
    ///   have survived) → released as `InfraInterrupted`;
    /// * the start permit was never delivered (no acknowledgement and no
    ///   event) → released as `InfraInterrupted`;
    /// * otherwise it stays held and is flagged uncertain.
    ///
    /// A quarantined attempt is the exception to the rules that release
    /// without proof: on the same boot it stays held whatever is reported.
    ///
    /// Reported attempts that stay held have their uncertain flag cleared.
    /// The machine's boot id is updated and the machine becomes eligible
    /// for offers in the current epoch.
    pub fn reconcile(
        &self,
        machine: &MachineId,
        boot_id: &BootId,
        reports: &[AttemptReport],
    ) -> Result<Vec<ReconcileVerdict>> {
        self.write(|c| reconcile(c, machine, boot_id, reports))
    }
}
