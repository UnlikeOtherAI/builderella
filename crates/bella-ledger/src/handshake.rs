//! The reserve → offer → start handshake.
//!
//! Each function is one transaction. No transaction is ever held open
//! while waiting for a worker: the controller commits the reservation,
//! sends the offer, and commits the start permit in a later transaction.

use crate::attempts::{self, Release, fence};
use crate::db::{Ctx, all_rows, enum_text, exec, id, ms, to_json};
use crate::events::emit;
use crate::jobs::{self, JobRow};
use crate::{
    AttemptRecord, AttemptTarget, AuthorityDue, Ledger, LedgerError, OfferRecord,
    OfferReplyOutcome, Result, RevokeOutcome, StartPermit, machines, projects,
};
use bella_core::UnixMillis;
use bella_core::api::EntityKind;
use bella_core::defaults::AUTHORITY_WINDOW;
use bella_core::ids::{AttemptId, JobId, MachineId};
use bella_core::job::{JobPhase, QueueReason, QueueReasonCode};
use bella_core::protocol::{AttemptRef, ExecutionAuthority, OfferDecision};
use bella_core::resources::{ResourceVector, Shortfall};
use rusqlite::params;
use serde_json::json;
use std::collections::BTreeSet;

/// The first dimension in which `held + request` exceeds `capacity`.
///
/// Every dimension is checked, including ones the request does not use: a
/// machine that is over its capacity in some dimension takes no new work.
fn shortfall(
    held: &ResourceVector,
    request: &ResourceVector,
    capacity: &ResourceVector,
) -> Option<Shortfall> {
    let over = |dimension: String, held: u64, requested: u64, capacity: u64| {
        let fits = held
            .checked_add(requested)
            .is_some_and(|total| total <= capacity);
        (!fits).then(|| Shortfall {
            dimension,
            requested,
            available: capacity.saturating_sub(held),
        })
    };
    let fixed = [
        ("cpu", held.cpu_milli, request.cpu_milli, capacity.cpu_milli),
        (
            "memory",
            held.memory_bytes,
            request.memory_bytes,
            capacity.memory_bytes,
        ),
        (
            "disk",
            held.disk_bytes,
            request.disk_bytes,
            capacity.disk_bytes,
        ),
    ];
    for (name, held, requested, capacity) in fixed {
        if let Some(found) = over(name.to_string(), held, requested, capacity) {
            return Some(found);
        }
    }
    let names: BTreeSet<&String> = held.tokens.keys().chain(request.tokens.keys()).collect();
    for name in names {
        let found = over(
            format!("token:{name}"),
            held.token(name),
            request.token(name),
            capacity.token(name),
        );
        if found.is_some() {
            return found;
        }
    }
    None
}

fn create_offer(
    c: &Ctx<'_>,
    job_id: &JobId,
    machine_id: &MachineId,
    reservation: &ResourceVector,
    capacity: &ResourceVector,
    max_concurrent: Option<u32>,
    expires_at: UnixMillis,
) -> Result<OfferRecord> {
    let job = jobs::load_row(c, job_id)?;
    if job.phase != JobPhase::Queued {
        return Err(LedgerError::InvalidState(format!(
            "job {job_id} is {}, not queued",
            enum_text(&job.phase)?
        )));
    }
    if job.cancel_requested {
        return Err(LedgerError::InvalidState(format!(
            "job {job_id} is being cancelled"
        )));
    }
    if let Some(unresolved) = attempts::held_for_job(c, job_id)? {
        return Err(LedgerError::UnresolvedAttempt {
            attempt: unresolved.id,
        });
    }
    if expires_at <= c.now {
        return Err(LedgerError::Invalid(
            "the offer would already be expired".into(),
        ));
    }

    let machine = machines::load(c, machine_id)?;
    if !machine.is_live() {
        return Err(LedgerError::PolicyDenied(format!(
            "machine {machine_id} is {}",
            enum_text(&machine.lifecycle)?
        )));
    }
    if machine.reconciled_epoch != Some(c.epoch) {
        return Err(LedgerError::NotReconciled(machine_id.clone()));
    }
    if !machine.policy.accept_builds {
        return Err(LedgerError::PolicyDenied(format!(
            "machine {machine_id} does not accept builds"
        )));
    }
    if let Some(allowed) = &machine.policy.allowed_projects
        && !allowed.contains(&job.project_id)
    {
        return Err(LedgerError::PolicyDenied(format!(
            "machine {machine_id} does not accept this project"
        )));
    }

    // Capacity and concurrency are checked against what is held right now,
    // inside the transaction that adds the new reservation.
    let held = attempts::held_on_machine(c, machine_id)?;
    let ceiling = machine.policy.managed_ceiling(&machine.inventory.hardware);
    let capacity = capacity.min(&ceiling);
    if let Some(shortfall) = shortfall(&machines::reserved(&held), reservation, &capacity) {
        return Err(LedgerError::Capacity { shortfall });
    }
    let limit = match (max_concurrent, machine.policy.max_concurrent_attempts) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let held_count = u32::try_from(held.len()).unwrap_or(u32::MAX);
    if let Some(limit) = limit
        && held_count >= limit
    {
        return Err(LedgerError::ConcurrencyLimit {
            limit,
            held: held_count,
        });
    }

    let spec = jobs::load_spec(c, job_id)?;
    let snapshot = projects::snapshot(c, &job.snapshot_id)?;
    let attempt = AttemptRef {
        attempt_id: AttemptId::generate(),
        generation: job.generation + 1,
    };
    exec(
        c.conn,
        "INSERT INTO attempts
             (id, job_id, generation, machine_id, phase, cleanup, reserved, held,
              policy_revision, offer_epoch, offered_at, offer_expires_at)
         VALUES (?1, ?2, ?3, ?4, 'offered', 'not_started', ?5, 1, ?6, ?7, ?8, ?9)",
        params![
            attempt.attempt_id.as_str(),
            job_id.as_str(),
            attempt.generation,
            machine_id.as_str(),
            to_json(reservation)?,
            crate::db::int(machine.policy.revision)?,
            crate::db::int(c.epoch)?,
            ms(c.now),
            ms(expires_at)
        ],
    )?;
    for (position, step) in spec.steps.iter().enumerate() {
        exec(
            c.conn,
            "INSERT INTO attempt_steps (attempt_id, position, name, kind, state)
             VALUES (?1, ?2, ?3, ?4, 'pending')",
            params![
                attempt.attempt_id.as_str(),
                i64::try_from(position).unwrap_or(i64::MAX),
                step.name,
                enum_text(&step.kind)?
            ],
        )?;
    }
    exec(
        c.conn,
        "UPDATE jobs SET generation = ?1 WHERE id = ?2",
        params![attempt.generation, job_id.as_str()],
    )?;
    emit(
        c,
        EntityKind::Attempt,
        attempt.attempt_id.as_str(),
        Some(job_id),
        "attempt.offered",
        json!({
            "generation": attempt.generation,
            "machine_id": machine_id,
            "reserved": reservation,
            "expires_at": expires_at,
        }),
    )?;
    let offered = JobRow {
        generation: attempt.generation,
        ..job
    };
    jobs::set_phase(c, &offered, JobPhase::Offered)?;

    Ok(OfferRecord {
        attempt,
        job_id: job_id.clone(),
        machine_id: machine_id.clone(),
        project_id: offered.project_id,
        snapshot_id: offered.snapshot_id,
        manifest_digest: snapshot.manifest_digest,
        spec,
        reservation: reservation.clone(),
        policy_revision: machine.policy.revision,
        expires_at,
        epoch: c.epoch,
    })
}

/// Load an attempt for a handshake message and check its generation.
fn current(c: &Ctx<'_>, reference: &AttemptRef) -> Result<(AttemptRecord, JobRow)> {
    let attempt = attempts::load(c, &reference.attempt_id)?;
    let job = jobs::load_row(c, &attempt.job_id)?;
    fence(&attempt, &job, reference)?;
    Ok((attempt, job))
}

/// Withdraw an offer. Refused once a start permit exists; a no-op when the
/// reservation is already released.
fn withdraw(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    reason: &'static str,
    message: String,
) -> Result<bool> {
    if !attempt.held {
        return Ok(false);
    }
    if attempt.start_committed() {
        return Err(LedgerError::InvalidState(format!(
            "attempt {} has a start permit and may be running; it can only be released by proof",
            attempt.id
        )));
    }
    if !attempt.may_be_withdrawn() {
        return Err(LedgerError::InvalidState(format!(
            "attempt {} is quarantined; it can only be released by proof",
            attempt.id
        )));
    }
    let released = attempts::release(
        c,
        &attempt.id,
        Release {
            reason,
            proof: None,
            lost_detail: "",
            queue_reason: Some(QueueReason::new(QueueReasonCode::OfferRejected, message)),
            proves_isolation: false,
        },
    )?;
    Ok(released.is_some())
}

fn record_offer_reply(
    c: &Ctx<'_>,
    reference: &AttemptRef,
    decision: &OfferDecision,
) -> Result<OfferReplyOutcome> {
    let (attempt, _job) = current(c, reference)?;
    match decision {
        OfferDecision::Accepted => {
            if !attempt.held {
                return Err(LedgerError::InvalidState(format!(
                    "the offer for attempt {} is no longer held",
                    attempt.id
                )));
            }
            if attempt.accepted_at.is_none() {
                exec(
                    c.conn,
                    "UPDATE attempts SET accepted_at = ?1 WHERE id = ?2",
                    params![ms(c.now), attempt.id.as_str()],
                )?;
                emit(
                    c,
                    EntityKind::Attempt,
                    attempt.id.as_str(),
                    Some(&attempt.job_id),
                    "attempt.accepted",
                    json!({ "generation": attempt.generation, "machine_id": attempt.machine_id }),
                )?;
            }
            Ok(OfferReplyOutcome::Accepted)
        }
        OfferDecision::Rejected { code, message } => {
            let reason = format!(
                "the machine rejected the offer ({}): {message}",
                enum_text(code)?
            );
            if withdraw(c, &attempt, "rejected", reason)? {
                Ok(OfferReplyOutcome::Requeued)
            } else {
                Ok(OfferReplyOutcome::AlreadyReleased)
            }
        }
    }
}

fn authority_for(c: &Ctx<'_>, attempt: &AttemptRecord) -> Result<ExecutionAuthority> {
    let spec = jobs::load_spec(c, &attempt.job_id)?;
    let machine = machines::load(c, &attempt.machine_id)?;
    let lifetime = spec.timeout_secs.saturating_add(
        spec.environment
            .map(|lease| lease.max_lifetime_secs)
            .unwrap_or(0),
    );
    let hard_deadline = c.now.plus_secs(lifetime);
    Ok(ExecutionAuthority {
        valid_until: c.now.plus(AUTHORITY_WINDOW).min(hard_deadline),
        hard_deadline,
        disconnected_continuation_secs: spec
            .disconnected_continuation_secs
            .min(machine.policy.max_disconnected_secs),
    })
}

fn check_epoch(c: &Ctx<'_>, epoch: u64) -> Result<()> {
    if epoch != c.epoch {
        return Err(LedgerError::StaleEpoch {
            got: epoch,
            current: c.epoch,
        });
    }
    Ok(())
}

fn commit_start(c: &Ctx<'_>, reference: &AttemptRef, epoch: u64) -> Result<StartPermit> {
    check_epoch(c, epoch)?;
    let (attempt, job) = current(c, reference)?;
    if !attempt.held {
        return Err(LedgerError::InvalidState(format!(
            "the reservation of attempt {} was released ({})",
            attempt.id,
            attempt.release_reason.as_deref().unwrap_or("unknown")
        )));
    }
    if let (Some(start_committed_at), Some(start_epoch), Some(authority)) = (
        attempt.start_committed_at,
        attempt.start_epoch,
        attempt.authority,
    ) {
        // Committing twice must not create a second permit.
        return Ok(StartPermit {
            attempt: reference.clone(),
            job_id: attempt.job_id,
            machine_id: attempt.machine_id,
            epoch: start_epoch,
            start_committed_at,
            authority,
            already_committed: true,
        });
    }
    if attempt.accepted_at.is_none() {
        return Err(LedgerError::InvalidState(format!(
            "the offer for attempt {} has not been accepted",
            attempt.id
        )));
    }
    if c.now >= attempt.offer_expires_at {
        return Err(LedgerError::InvalidState(format!(
            "the offer for attempt {} has expired",
            attempt.id
        )));
    }
    if job.cancel_requested {
        return Err(LedgerError::InvalidState(format!(
            "job {} is being cancelled",
            job.id
        )));
    }
    if job.phase != JobPhase::Offered || attempt.phase != JobPhase::Offered {
        return Err(LedgerError::InvalidState(format!(
            "job {} is {}, not offered",
            job.id,
            enum_text(&job.phase)?
        )));
    }
    // A start is only worth committing if it can be delivered.
    let machine = machines::load(c, &attempt.machine_id)?;
    if !machine.is_live() {
        return Err(LedgerError::InvalidState(format!(
            "machine {} is no longer enrolled",
            machine.id
        )));
    }
    if machine.reconciled_epoch != Some(c.epoch) {
        return Err(LedgerError::NotReconciled(machine.id));
    }
    let authority = authority_for(c, &attempt)?;
    exec(
        c.conn,
        "UPDATE attempts SET start_committed_at = ?1, start_epoch = ?2, authority = ?3,
                authority_valid_until = ?4
         WHERE id = ?5 AND held = 1 AND start_committed_at IS NULL",
        params![
            ms(c.now),
            crate::db::int(epoch)?,
            to_json(&authority)?,
            ms(authority.valid_until),
            attempt.id.as_str()
        ],
    )?;
    emit(
        c,
        EntityKind::Attempt,
        attempt.id.as_str(),
        Some(&attempt.job_id),
        "attempt.start_committed",
        json!({
            "generation": attempt.generation,
            "machine_id": attempt.machine_id,
            "epoch": epoch,
            "authority": authority,
        }),
    )?;
    Ok(StartPermit {
        attempt: reference.clone(),
        job_id: attempt.job_id,
        machine_id: attempt.machine_id,
        epoch,
        start_committed_at: c.now,
        authority,
        already_committed: false,
    })
}

fn renew_authority(c: &Ctx<'_>, reference: &AttemptRef, epoch: u64) -> Result<ExecutionAuthority> {
    check_epoch(c, epoch)?;
    let (attempt, job) = current(c, reference)?;
    let Some(mut authority) = attempt.authority.filter(|_| attempt.held) else {
        return Err(LedgerError::InvalidState(format!(
            "attempt {} holds no start permit to renew",
            attempt.id
        )));
    };
    if job.cancel_requested {
        return Err(LedgerError::InvalidState(format!(
            "job {} is being cancelled; its authority is not renewed",
            job.id
        )));
    }
    authority.valid_until = c.now.plus(AUTHORITY_WINDOW).min(authority.hard_deadline);
    exec(
        c.conn,
        "UPDATE attempts SET authority = ?1, authority_valid_until = ?2 WHERE id = ?3",
        params![
            to_json(&authority)?,
            ms(authority.valid_until),
            attempt.id.as_str()
        ],
    )?;
    Ok(authority)
}

impl Ledger {
    /// Reserve resources on a machine for a queued job and create the
    /// attempt that will carry the offer.
    ///
    /// In one transaction this requires that:
    ///
    /// * the job is `Queued` and not being cancelled;
    /// * no earlier attempt of the job still holds its reservation;
    /// * the machine is enrolled, accepts builds and this project, and has
    ///   reconciled since it connected in the current epoch;
    /// * the machine's held reservations plus `reservation` fit, in every
    ///   dimension, within `capacity` capped by the machine policy's static
    ///   managed ceiling. A reservation counts in full from the moment it is
    ///   created, whatever its attempt has used so far;
    /// * the machine holds fewer attempts than `max_concurrent` and than
    ///   its policy's own limit.
    ///
    /// It then creates the attempt with `generation = previous + 1` and a
    /// held reservation, and moves the job to `Offered`. On any error
    /// nothing changes.
    pub fn create_offer(
        &self,
        job: &JobId,
        machine: &MachineId,
        reservation: &ResourceVector,
        capacity: &ResourceVector,
        max_concurrent: Option<u32>,
        expires_at: UnixMillis,
    ) -> Result<OfferRecord> {
        self.write(|c| {
            create_offer(
                c,
                job,
                machine,
                reservation,
                capacity,
                max_concurrent,
                expires_at,
            )
        })
    }

    /// Record the worker's answer to an offer.
    ///
    /// An acceptance is remembered; nothing runs yet. A rejection releases
    /// the reservation and returns the job to the queue, but only while no
    /// start permit exists: after [`Ledger::commit_start`] it is
    /// `InvalidState` and changes nothing.
    pub fn record_offer_reply(
        &self,
        attempt: &AttemptRef,
        decision: &OfferDecision,
    ) -> Result<OfferReplyOutcome> {
        self.write(|c| record_offer_reply(c, attempt, decision))
    }

    /// Withdraw an offer and return its job to the queue. Refused with
    /// `InvalidState`, changing nothing, once a start permit exists.
    pub fn revoke_offer(&self, attempt: &AttemptRef, reason: &str) -> Result<RevokeOutcome> {
        self.write(|c| {
            let (attempt, _job) = current(c, attempt)?;
            let message = format!("the offer was revoked: {reason}");
            if withdraw(c, &attempt, "revoked", message)? {
                Ok(RevokeOutcome::Revoked)
            } else {
                Ok(RevokeOutcome::AlreadyReleased)
            }
        })
    }

    /// Withdraw every offer that expired at or before `now` without a
    /// start permit, returning their jobs to the queue. Offers with a
    /// start permit, and quarantined ones, are never touched. Returns the withdrawn attempts so
    /// their workers can be told.
    pub fn expire_offers(&self, now: UnixMillis) -> Result<Vec<AttemptTarget>> {
        self.write(|c| {
            let expired = all_rows(
                c.conn,
                "SELECT id FROM attempts
                 WHERE held = 1 AND start_committed_at IS NULL AND phase = 'offered'
                   AND offer_expires_at <= ?1
                 ORDER BY offered_at, id",
                params![ms(now)],
                |row| id::<AttemptId, _>(row, 0),
            )?;
            let mut withdrawn = Vec::with_capacity(expired.len());
            for attempt in expired {
                let attempt = attempts::load(c, &attempt)?;
                let message = "the offer expired before the machine could start it".to_string();
                if withdraw(c, &attempt, "expired", message)? {
                    withdrawn.push(attempt.target());
                }
            }
            Ok(withdrawn)
        })
    }

    /// Persist the start permit for an accepted offer.
    ///
    /// Requires `epoch` to be the current controller epoch, the attempt to
    /// be the job's current generation, its reservation still held, the
    /// offer accepted and unexpired, the job not being cancelled, and the
    /// machine still enrolled, connected and reconciled.
    /// From this commit on the attempt is treated as possibly running:
    /// it can no longer be rejected, revoked or expired, and no other
    /// attempt can be created for the job until it is released.
    ///
    /// Calling it again for an attempt that already has a permit returns
    /// that permit with `already_committed` set and changes nothing.
    pub fn commit_start(&self, attempt: &AttemptRef, epoch: u64) -> Result<StartPermit> {
        self.write(|c| commit_start(c, attempt, epoch))
    }

    /// Record that the worker acknowledged the start command. Together
    /// with the first attempt event, this is the evidence
    /// [`Ledger::reconcile`] uses to decide whether a start was delivered.
    /// Returns `false` when it was already recorded or the attempt holds
    /// no start permit.
    pub fn record_start_delivered(&self, attempt: &AttemptRef) -> Result<bool> {
        self.write(|c| {
            let (attempt, _job) = current(c, attempt)?;
            if !attempt.held || !attempt.start_committed() || attempt.start_delivered_at.is_some() {
                return Ok(false);
            }
            exec(
                c.conn,
                "UPDATE attempts SET start_delivered_at = ?1 WHERE id = ?2",
                params![ms(c.now), attempt.id.as_str()],
            )?;
            emit(
                c,
                EntityKind::Attempt,
                attempt.id.as_str(),
                Some(&attempt.job_id),
                "attempt.start_delivered",
                json!({ "generation": attempt.generation, "machine_id": attempt.machine_id }),
            )?;
            Ok(true)
        })
    }

    /// Extend a running attempt's execution authority by one window, never
    /// past its hard deadline. Requires the current epoch and generation,
    /// a held start permit and no pending cancellation.
    pub fn renew_authority(&self, attempt: &AttemptRef, epoch: u64) -> Result<ExecutionAuthority> {
        self.write(|c| renew_authority(c, attempt, epoch))
    }

    /// Started, unreleased attempts on connected machines whose authority
    /// lapses at or before `before` and whose job is not being cancelled.
    pub fn attempts_needing_renewal(&self, before: UnixMillis) -> Result<Vec<AuthorityDue>> {
        self.read(|c| {
            let due = all_rows(
                c.conn,
                "SELECT a.id FROM attempts a
                 JOIN machines m ON m.id = a.machine_id
                 JOIN jobs j ON j.id = a.job_id
                 WHERE a.held = 1 AND a.start_committed_at IS NOT NULL
                   AND a.authority_valid_until <= ?1
                   AND a.phase IN ('offered', 'preparing', 'executing', 'collecting')
                   AND m.connected = 1 AND j.cancel_requested = 0
                 ORDER BY a.authority_valid_until, a.id",
                params![ms(before)],
                |row| id::<AttemptId, _>(row, 0),
            )?;
            let mut out = Vec::with_capacity(due.len());
            for attempt in due {
                let attempt = attempts::load(c, &attempt)?;
                if let Some(authority) = attempt.authority {
                    out.push(AuthorityDue {
                        target: attempt.target(),
                        authority,
                    });
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortfall_reports_what_is_left() {
        let held = ResourceVector::new(6000, 10, 0).with_token("gpu", 1);
        let capacity = ResourceVector::new(8000, 16, 100).with_token("gpu", 1);
        assert_eq!(
            shortfall(&held, &ResourceVector::new(2000, 6, 100), &capacity),
            None
        );
        let cpu = shortfall(&held, &ResourceVector::new(2001, 1, 0), &capacity).unwrap();
        assert_eq!((cpu.dimension.as_str(), cpu.available), ("cpu", 2000));
        let gpu = shortfall(
            &held,
            &ResourceVector::new(1, 1, 0).with_token("gpu", 1),
            &capacity,
        )
        .unwrap();
        assert_eq!(
            (gpu.dimension.as_str(), gpu.requested, gpu.available),
            ("token:gpu", 1, 0)
        );
        let unknown = shortfall(
            &ResourceVector::ZERO,
            &ResourceVector::ZERO.with_token("sim", 1),
            &capacity,
        )
        .unwrap();
        assert_eq!(unknown.dimension, "token:sim");
    }

    #[test]
    fn shortfall_does_not_overflow() {
        let held = ResourceVector::new(u64::MAX, 0, 0);
        let capacity = ResourceVector::new(u64::MAX, 0, 0);
        assert!(shortfall(&held, &ResourceVector::new(1, 0, 0), &capacity).is_some());
        assert!(shortfall(&held, &ResourceVector::ZERO, &capacity).is_none());
    }
}
