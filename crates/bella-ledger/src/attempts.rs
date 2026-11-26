//! Attempts after the offer: worker events, release, quarantine,
//! cancellation and uncertainty.
//!
//! The reservation of an attempt is released by exactly one statement,
//! `UPDATE attempts SET held = 0 ... WHERE held = 1`, in [`release`]. Every
//! path that frees capacity goes through it, so a release applies at most
//! once whatever arrives twice or out of order.

use crate::db::{
    Ctx, all_rows, count, enum_text, enumeration, exec, flag, id, int, json, ms, opt_json, opt_row,
    opt_text, opt_time, opt_uint, time, to_json, uint, uint32,
};
use crate::events::emit;
use crate::identity::require_role;
use crate::jobs::{self, JobRow};
use crate::{
    AttemptRecord, AttemptTarget, CancelOutcome, EventDisposition, JobDisposition, Ledger,
    LedgerError, ReleaseOutcome, Result, artifacts, environments, machines, phase,
};
use bella_core::api::{EntityKind, Role};
use bella_core::ids::{AttemptId, JobId, MachineId, PrincipalId};
use bella_core::job::{CleanupState, JobPhase, Outcome, QueueReason};
use bella_core::protocol::{AttemptEvent, AttemptEventMsg, AttemptRef, ReleaseProof, StepState};
use rusqlite::{Row, params};
use serde_json::json;

const ATTEMPT_COLUMNS: &str = "id, job_id, generation, machine_id, phase, outcome, cleanup, \
     uncertain, reserved, held, release_reason, policy_revision, offer_epoch, offered_at, \
     offer_expires_at, accepted_at, start_committed_at, start_epoch, start_delivered_at, \
     authority, usage, started_at, exited_at, released_at, last_event_seq, log_bytes, \
     log_truncated";

fn attempt_from(row: &Row<'_>) -> Result<AttemptRecord> {
    Ok(AttemptRecord {
        id: id(row, "id")?,
        job_id: id(row, "job_id")?,
        generation: uint32(row, "generation")?,
        machine_id: id(row, "machine_id")?,
        phase: enumeration(row, "phase")?,
        outcome: opt_json(row, "outcome")?,
        cleanup: enumeration(row, "cleanup")?,
        uncertain: flag(row, "uncertain")?,
        reserved: json(row, "reserved")?,
        held: flag(row, "held")?,
        release_reason: opt_text(row, "release_reason")?,
        policy_revision: uint(row, "policy_revision")?,
        offer_epoch: uint(row, "offer_epoch")?,
        offered_at: time(row, "offered_at")?,
        offer_expires_at: time(row, "offer_expires_at")?,
        accepted_at: opt_time(row, "accepted_at")?,
        start_committed_at: opt_time(row, "start_committed_at")?,
        start_epoch: opt_uint(row, "start_epoch")?,
        start_delivered_at: opt_time(row, "start_delivered_at")?,
        authority: opt_json(row, "authority")?,
        usage: opt_json(row, "usage")?,
        started_at: opt_time(row, "started_at")?,
        exited_at: opt_time(row, "exited_at")?,
        released_at: opt_time(row, "released_at")?,
        last_event_seq: uint(row, "last_event_seq")?,
        log_bytes: uint(row, "log_bytes")?,
        log_truncated: flag(row, "log_truncated")?,
    })
}

pub(crate) fn find(c: &Ctx<'_>, attempt: &AttemptId) -> Result<Option<AttemptRecord>> {
    opt_row(
        c.conn,
        &format!("SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE id = ?1"),
        params![attempt.as_str()],
        attempt_from,
    )
}

pub(crate) fn load(c: &Ctx<'_>, attempt: &AttemptId) -> Result<AttemptRecord> {
    find(c, attempt)?.ok_or_else(|| LedgerError::NotFound(format!("attempt {attempt}")))
}

/// Attempts whose reservation still counts against the machine.
pub(crate) fn held_on_machine(c: &Ctx<'_>, machine: &MachineId) -> Result<Vec<AttemptRecord>> {
    all_rows(
        c.conn,
        &format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts
             WHERE machine_id = ?1 AND held = 1 ORDER BY offered_at, id"
        ),
        params![machine.as_str()],
        attempt_from,
    )
}

/// The job's unresolved attempt, if it has one. The schema allows at most one.
pub(crate) fn held_for_job(c: &Ctx<'_>, job: &JobId) -> Result<Option<AttemptRecord>> {
    opt_row(
        c.conn,
        &format!("SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE job_id = ?1 AND held = 1"),
        params![job.as_str()],
        attempt_from,
    )
}

/// Refuse a message that does not name the job's current generation.
pub(crate) fn fence(attempt: &AttemptRecord, job: &JobRow, reference: &AttemptRef) -> Result<()> {
    if reference.generation != attempt.generation || attempt.generation != job.generation {
        return Err(LedgerError::StaleGeneration {
            attempt: attempt.id.clone(),
            got: reference.generation,
            current: job.generation,
        });
    }
    Ok(())
}

fn attempt_event(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    kind: &str,
    mut data: serde_json::Value,
) -> Result<()> {
    if let Some(fields) = data.as_object_mut() {
        fields.insert("generation".into(), json!(attempt.generation));
        fields.insert("machine_id".into(), json!(attempt.machine_id));
    }
    emit(
        c,
        EntityKind::Attempt,
        attempt.id.as_str(),
        Some(&attempt.job_id),
        kind,
        data,
    )?;
    Ok(())
}

// ------------------------------------------------------------------ release

/// How a reservation is being released.
pub(crate) struct Release<'a> {
    /// Stored as the attempt's release reason. For an offer that never
    /// started it is also the event name: `attempt.<reason>`.
    pub reason: &'static str,
    pub proof: Option<&'a ReleaseProof>,
    /// Detail of the `InfraInterrupted` outcome given to a started attempt
    /// that never reported a result.
    pub lost_detail: &'a str,
    /// Why the job waits again, when an offer that never started is withdrawn.
    pub queue_reason: Option<QueueReason>,
    /// The release rests on evidence that nothing of the attempt is left on
    /// the machine: a worker's proof, a reboot or an administrator's word.
    /// Only such a release may free a quarantined reservation.
    pub proves_isolation: bool,
}

/// Release an attempt's reservation and settle its job. Returns `None`,
/// changing nothing, when the reservation was already released.
///
/// An offer that never received a start permit goes back to the queue (or
/// finishes as cancelled when cancellation was requested). A started
/// attempt finishes its job, or queues it again when the outcome is
/// retryable for the job's retry class and attempts remain.
pub(crate) fn release(
    c: &Ctx<'_>,
    attempt: &AttemptId,
    how: Release<'_>,
) -> Result<Option<JobDisposition>> {
    let attempt = load(c, attempt)?;
    if !attempt.held {
        return Ok(None);
    }
    if attempt.phase == JobPhase::Quarantined && !how.proves_isolation {
        return Err(LedgerError::InvalidState(format!(
            "attempt {} is quarantined; its reservation is released only by proof",
            attempt.id
        )));
    }
    let job = jobs::load_row(c, &attempt.job_id)?;
    let started = attempt.start_committed();
    let reason = if !started && how.reason == "proof" {
        "forgotten"
    } else {
        how.reason
    };
    let outcome = match (&attempt.outcome, started) {
        (Some(outcome), _) => Some(outcome.clone()),
        (None, false) => None,
        (None, true) if job.cancel_requested => Some(Outcome::Cancelled),
        (None, true) => Some(Outcome::InfraInterrupted {
            detail: how.lost_detail.to_string(),
        }),
    };
    phase::check(
        &format!("attempt {}", attempt.id),
        attempt.phase,
        JobPhase::Finished,
    )?;
    let changed = exec(
        c.conn,
        "UPDATE attempts SET held = 0, released_at = ?1, release_reason = ?2, release_proof = ?3,
                phase = 'finished', cleanup = 'released', uncertain = 0, outcome = ?4
         WHERE id = ?5 AND held = 1",
        params![
            ms(c.now),
            reason,
            match how.proof {
                Some(proof) => Some(to_json(proof)?),
                None => None,
            },
            match &outcome {
                Some(outcome) => Some(to_json(outcome)?),
                None => None,
            },
            attempt.id.as_str()
        ],
    )?;
    if changed != 1 {
        return Ok(None);
    }
    environments::end_for_attempt(c, &attempt.id, reason)?;
    let kind = if started {
        "attempt.released".to_string()
    } else {
        format!("attempt.{reason}")
    };
    attempt_event(
        c,
        &attempt,
        &kind,
        json!({ "reason": reason, "proof": how.proof, "outcome": outcome, "reserved": attempt.reserved }),
    )?;

    let disposition = match outcome {
        Some(outcome) if started => {
            if jobs::will_retry(c, &job, &outcome)? {
                jobs::requeue(c, &job, None, true)?;
                JobDisposition::Requeued
            } else {
                jobs::finish(c, &job, &outcome, CleanupState::Released)?;
                JobDisposition::Finished
            }
        }
        _ if job.cancel_requested => {
            jobs::finish(c, &job, &Outcome::Cancelled, CleanupState::NotStarted)?;
            JobDisposition::Finished
        }
        _ => {
            jobs::requeue(c, &job, how.queue_reason.as_ref(), false)?;
            JobDisposition::Requeued
        }
    };
    Ok(Some(disposition))
}

/// Keep an attempt's reservation held because its cleanup cannot be
/// verified. Returns `false` when the attempt is already released or
/// already quarantined.
pub(crate) fn quarantine(c: &Ctx<'_>, attempt: &AttemptId, reason: &str) -> Result<bool> {
    let attempt = load(c, attempt)?;
    if !attempt.held || attempt.phase == JobPhase::Quarantined {
        return Ok(false);
    }
    let job = jobs::load_row(c, &attempt.job_id)?;
    phase::check(
        &format!("attempt {}", attempt.id),
        attempt.phase,
        JobPhase::Quarantined,
    )?;
    exec(
        c.conn,
        "UPDATE attempts SET phase = 'quarantined', cleanup = 'quarantined' WHERE id = ?1",
        params![attempt.id.as_str()],
    )?;
    exec(
        c.conn,
        "UPDATE jobs SET cleanup = 'quarantined' WHERE id = ?1",
        params![job.id.as_str()],
    )?;
    environments::quarantine_for_attempt(c, &attempt.id, reason)?;
    attempt_event(
        c,
        &attempt,
        "attempt.quarantined",
        json!({ "reason": reason }),
    )?;
    jobs::set_phase(c, &job, JobPhase::Quarantined)?;
    Ok(true)
}

/// Apply a worker's release proof. A proof that does not show an empty
/// scope and a removed workspace quarantines the attempt instead.
pub(crate) fn release_with_proof(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    proof: &ReleaseProof,
) -> Result<ReleaseOutcome> {
    if !attempt.held {
        return Ok(ReleaseOutcome::AlreadyReleased);
    }
    if !(proof.scope_empty && proof.workspace_removed) {
        let reason = format!(
            "release proof is incomplete: scope_empty={}, workspace_removed={}, method={}",
            proof.scope_empty, proof.workspace_removed, proof.method
        );
        quarantine(c, &attempt.id, &reason)?;
        return Ok(ReleaseOutcome::Quarantined);
    }
    let released = release(
        c,
        &attempt.id,
        Release {
            reason: "proof",
            proof: Some(proof),
            lost_detail: "the attempt was released without reporting a result",
            queue_reason: None,
            proves_isolation: true,
        },
    )?;
    Ok(match released {
        Some(disposition) => ReleaseOutcome::Released(disposition),
        None => ReleaseOutcome::AlreadyReleased,
    })
}

// ------------------------------------------------------------------- events

/// Move an attempt and its job forward to `target`. Reports never move an
/// attempt backwards.
fn advance(c: &Ctx<'_>, attempt: &AttemptRecord, job: &JobRow, target: JobPhase) -> Result<()> {
    if !phase::advances(attempt.phase, target) {
        return Ok(());
    }
    phase::check(&format!("attempt {}", attempt.id), attempt.phase, target)?;
    exec(
        c.conn,
        "UPDATE attempts SET phase = ?1 WHERE id = ?2",
        params![enum_text(&target)?, attempt.id.as_str()],
    )?;
    jobs::set_phase(c, job, target)
}

fn apply_step(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    step: &str,
    state: StepState,
    exit_code: Option<i32>,
    at: i64,
) -> Result<()> {
    // A step the specification did not declare is appended as a task.
    let known = count(
        c.conn,
        "SELECT COUNT(*) FROM attempt_steps WHERE attempt_id = ?1 AND name = ?2",
        params![attempt.id.as_str(), step],
    )?;
    if known == 0 {
        exec(
            c.conn,
            "INSERT INTO attempt_steps (attempt_id, position, name, kind, state)
             SELECT ?1, COALESCE(MAX(position) + 1, 0), ?2, 'task', 'pending'
             FROM attempt_steps WHERE attempt_id = ?1",
            params![attempt.id.as_str(), step],
        )?;
    }
    let finished = matches!(
        state,
        StepState::Succeeded
            | StepState::Failed
            | StepState::TimedOut
            | StepState::Skipped
            | StepState::Stopped
    );
    let ran = !matches!(state, StepState::Pending | StepState::Skipped);
    exec(
        c.conn,
        "UPDATE attempt_steps SET state = ?1, exit_code = COALESCE(?2, exit_code),
                started_at = CASE WHEN ?3 THEN COALESCE(started_at, ?4) ELSE started_at END,
                finished_at = CASE WHEN ?5 THEN ?4 ELSE finished_at END
         WHERE attempt_id = ?6 AND name = ?7",
        params![
            enum_text(&state)?,
            exit_code,
            ran,
            at,
            finished,
            attempt.id.as_str(),
            step
        ],
    )?;
    Ok(())
}

fn apply_cleanup(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    job: &JobRow,
    state: CleanupState,
    detail: &str,
) -> Result<()> {
    if state == CleanupState::Quarantined {
        quarantine(c, &attempt.id, detail)?;
        return Ok(());
    }
    // Only a release marks cleanup as released.
    let reported = state.min(CleanupState::CleanupComplete);
    let was_quarantined = attempt.phase == JobPhase::Quarantined;
    let next = if was_quarantined {
        reported
    } else {
        reported.max(attempt.cleanup)
    };
    let name = enum_text(&next)?;
    exec(
        c.conn,
        "UPDATE attempts SET cleanup = ?1 WHERE id = ?2",
        params![name, attempt.id.as_str()],
    )?;
    exec(
        c.conn,
        "UPDATE jobs SET cleanup = ?1 WHERE id = ?2",
        params![name, job.id.as_str()],
    )?;
    if was_quarantined {
        // Cleanup is being attempted again: quarantine is left for cleaning.
        phase::check(
            &format!("attempt {}", attempt.id),
            attempt.phase,
            JobPhase::Cleaning,
        )?;
        exec(
            c.conn,
            "UPDATE attempts SET phase = 'cleaning' WHERE id = ?1",
            params![attempt.id.as_str()],
        )?;
        jobs::set_phase(c, job, JobPhase::Cleaning)?;
    } else if reported >= CleanupState::WorkerAcknowledged {
        advance(c, attempt, job, JobPhase::Cleaning)?;
    }
    Ok(())
}

fn apply_exit(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    job: &JobRow,
    outcome: &Outcome,
    usage: &bella_core::protocol::UsageSummary,
    at: i64,
) -> Result<()> {
    // The first reported result stands.
    if attempt.outcome.is_none() {
        exec(
            c.conn,
            "UPDATE attempts SET outcome = ?1, usage = ?2, exited_at = ?3 WHERE id = ?4",
            params![to_json(outcome)?, to_json(usage)?, at, attempt.id.as_str()],
        )?;
        // A result that will be retried is not the job's result.
        if job.outcome.is_none() && !jobs::will_retry(c, job, outcome)? {
            exec(
                c.conn,
                "UPDATE jobs SET outcome = ?1 WHERE id = ?2",
                params![to_json(outcome)?, job.id.as_str()],
            )?;
        }
    }
    environments::finishing_for_attempt(c, &attempt.id, "the attempt exited")?;
    advance(c, attempt, job, JobPhase::Cleaning)
}

fn event_kind(event: &AttemptEvent) -> &'static str {
    match event {
        AttemptEvent::Preparing => "attempt.preparing",
        AttemptEvent::Started { .. } => "attempt.started",
        AttemptEvent::StepChanged { .. } => "attempt.step_changed",
        AttemptEvent::EnvironmentReady { .. } => "attempt.environment_ready",
        AttemptEvent::Collecting => "attempt.collecting",
        AttemptEvent::ArtifactPublished { .. } => "attempt.artifact_published",
        AttemptEvent::Exited { .. } => "attempt.exited",
        AttemptEvent::Cleanup { .. } => "attempt.cleanup",
        AttemptEvent::Released { .. } => "attempt.released",
        AttemptEvent::Quarantined { .. } => "attempt.quarantined",
    }
}

fn record_event(c: &Ctx<'_>, msg: &AttemptEventMsg) -> Result<EventDisposition> {
    let attempt = load(c, &msg.attempt.attempt_id)?;
    let job = jobs::load_row(c, &attempt.job_id)?;
    fence(&attempt, &job, &msg.attempt)?;
    if msg.seq <= attempt.last_event_seq {
        return Ok(EventDisposition::Duplicate);
    }
    if !attempt.held {
        return Ok(EventDisposition::AttemptReleased);
    }
    let expected = attempt.last_event_seq + 1;
    if msg.seq != expected {
        return Err(LedgerError::SequenceGap {
            expected,
            got: msg.seq,
        });
    }
    let started = attempt.start_committed();
    if !started
        && !matches!(
            msg.event,
            AttemptEvent::Released { .. } | AttemptEvent::Quarantined { .. }
        )
    {
        return Err(LedgerError::InvalidState(format!(
            "attempt {} reported `{}` without a start permit",
            attempt.id,
            event_kind(&msg.event)
        )));
    }
    // Any event from a started attempt proves the start was delivered.
    exec(
        c.conn,
        "UPDATE attempts SET last_event_seq = ?1,
                start_delivered_at = CASE WHEN ?2 THEN COALESCE(start_delivered_at, ?3)
                                          ELSE start_delivered_at END
         WHERE id = ?4",
        params![int(msg.seq)?, started, ms(c.now), attempt.id.as_str()],
    )?;

    let at = ms(msg.at);
    match &msg.event {
        AttemptEvent::Released { proof } => {
            release_with_proof(c, &attempt, proof)?;
            return Ok(EventDisposition::Applied);
        }
        AttemptEvent::Quarantined { reason } => {
            quarantine(c, &attempt.id, reason)?;
            return Ok(EventDisposition::Applied);
        }
        AttemptEvent::Cleanup {
            state: CleanupState::Quarantined,
            detail,
        } => {
            quarantine(c, &attempt.id, detail)?;
            return Ok(EventDisposition::Applied);
        }
        _ => {}
    }
    // The transition's own event precedes the job events it causes.
    attempt_event(
        c,
        &attempt,
        event_kind(&msg.event),
        json!({ "seq": msg.seq, "worker_at": msg.at, "event": msg.event }),
    )?;
    match &msg.event {
        AttemptEvent::Preparing => advance(c, &attempt, &job, JobPhase::Preparing)?,
        AttemptEvent::Started { scope } => {
            exec(
                c.conn,
                "UPDATE attempts SET scope = ?1, started_at = COALESCE(started_at, ?2)
                 WHERE id = ?3",
                params![to_json(scope)?, at, attempt.id.as_str()],
            )?;
            advance(c, &attempt, &job, JobPhase::Executing)?;
        }
        AttemptEvent::StepChanged {
            step,
            state,
            exit_code,
        } => {
            apply_step(c, &attempt, step, *state, *exit_code, at)?;
            if matches!(state, StepState::Running | StepState::Ready) {
                advance(c, &attempt, &job, JobPhase::Executing)?;
            }
        }
        AttemptEvent::EnvironmentReady { endpoints } => {
            environments::ready(c, &attempt, &job, endpoints)?;
        }
        AttemptEvent::Collecting => advance(c, &attempt, &job, JobPhase::Collecting)?,
        AttemptEvent::ArtifactPublished {
            path,
            kind,
            digest,
            size,
        } => {
            artifacts::record(c, &attempt, &job, path, *kind, digest, *size)?;
        }
        AttemptEvent::Exited { outcome, usage } => {
            apply_exit(c, &attempt, &job, outcome, usage, at)?;
        }
        AttemptEvent::Cleanup { state, detail } => {
            apply_cleanup(c, &attempt, &job, *state, detail)?;
        }
        AttemptEvent::Released { .. } | AttemptEvent::Quarantined { .. } => {}
    }
    Ok(EventDisposition::Applied)
}

// ------------------------------------------------------------- cancellation

fn mark_cancel_requested(c: &Ctx<'_>, job: &JobRow, attempt: Option<&AttemptRecord>) -> Result<()> {
    if job.cancel_requested {
        return Ok(());
    }
    exec(
        c.conn,
        "UPDATE jobs SET cancel_requested = 1 WHERE id = ?1",
        params![job.id.as_str()],
    )?;
    emit(
        c,
        EntityKind::Job,
        job.id.as_str(),
        Some(&job.id),
        "job.cancel_requested",
        json!({ "attempt_id": attempt.map(|a| &a.id) }),
    )?;
    Ok(())
}

fn request_cancel(c: &Ctx<'_>, job_id: &JobId) -> Result<CancelOutcome> {
    let job = jobs::load_row(c, job_id)?;
    if job.phase == JobPhase::Finished {
        return Ok(CancelOutcome::AlreadyFinished);
    }
    let Some(attempt) = held_for_job(c, job_id)? else {
        // Nothing is reserved: the job ends here, so no offer can follow.
        mark_cancel_requested(c, &job, None)?;
        jobs::finish(c, &job, &Outcome::Cancelled, job.cleanup)?;
        return Ok(CancelOutcome::Cancelled { revoked: None });
    };
    mark_cancel_requested(c, &job, Some(&attempt))?;
    if attempt.may_be_withdrawn() {
        // No start permit: the offer can be withdrawn and the job ended now.
        release(
            c,
            &attempt.id,
            Release {
                reason: "cancelled",
                proof: None,
                lost_detail: "",
                queue_reason: None,
                proves_isolation: false,
            },
        )?;
        return Ok(CancelOutcome::Cancelled {
            revoked: Some(attempt.target()),
        });
    }
    // Possibly running: cancellation is a request until cleanup is observed.
    if !job.cancel_requested {
        let job_cleanup = enum_text(&job.cleanup.max(CleanupState::Requested))?;
        let attempt_cleanup = enum_text(&attempt.cleanup.max(CleanupState::Requested))?;
        exec(
            c.conn,
            "UPDATE jobs SET cleanup = ?1 WHERE id = ?2",
            params![job_cleanup, job.id.as_str()],
        )?;
        exec(
            c.conn,
            "UPDATE attempts SET cleanup = ?1 WHERE id = ?2",
            params![attempt_cleanup, attempt.id.as_str()],
        )?;
    }
    Ok(CancelOutcome::Requested {
        target: attempt.target(),
    })
}

/// Flag or clear uncertainty on one attempt. Returns whether it changed.
pub(crate) fn set_uncertain(c: &Ctx<'_>, attempt: &AttemptRecord, uncertain: bool) -> Result<bool> {
    if attempt.uncertain == uncertain || !attempt.held {
        return Ok(false);
    }
    exec(
        c.conn,
        "UPDATE attempts SET uncertain = ?1 WHERE id = ?2",
        params![uncertain, attempt.id.as_str()],
    )?;
    let kind = if uncertain {
        "attempt.uncertain"
    } else {
        "attempt.uncertain_cleared"
    };
    attempt_event(c, attempt, kind, json!({}))?;
    Ok(true)
}

impl Ledger {
    /// Apply one journaled worker event.
    ///
    /// * **Fenced by generation.** An event that does not name the job's
    ///   current generation is [`LedgerError::StaleGeneration`] and changes
    ///   nothing, so an old attempt can never set the job's result.
    /// * **Idempotent by `(attempt, seq)`.** A sequence number at or below
    ///   the last applied one is a `Duplicate` and changes nothing. A
    ///   sequence number that skips ahead is
    ///   [`LedgerError::SequenceGap`] carrying the expected one.
    /// * An event for an attempt whose reservation was already released is
    ///   dropped (`AttemptReleased`).
    /// * An attempt without a start permit may only report `Released` or
    ///   `Quarantined`; anything else is `InvalidState`.
    ///
    /// `Exited` records the attempt's result and, unless the result will be
    /// retried, the job's canonical outcome; the reservation stays held.
    /// `Released` releases it exactly once, see
    /// [`Ledger::record_release`]. `Quarantined` keeps it held.
    pub fn record_attempt_event(&self, event: &AttemptEventMsg) -> Result<EventDisposition> {
        self.write(|c| record_event(c, event))
    }

    /// Release an attempt's reservation on the worker's proof that its
    /// containment scope is empty and its workspace removed.
    ///
    /// The release applies exactly once: a second call returns
    /// `AlreadyReleased` and changes nothing. The job becomes `Finished`,
    /// or `Queued` again when the outcome is retryable for the job's retry
    /// class and started attempts remain below `max_attempts`. A proof that
    /// does not show both facts quarantines the attempt and keeps the
    /// reservation held.
    pub fn record_release(
        &self,
        attempt: &AttemptRef,
        proof: &ReleaseProof,
    ) -> Result<ReleaseOutcome> {
        self.write(|c| {
            let found = load(c, &attempt.attempt_id)?;
            if attempt.generation != found.generation {
                return Err(LedgerError::StaleGeneration {
                    attempt: found.id,
                    got: attempt.generation,
                    current: found.generation,
                });
            }
            release_with_proof(c, &found, proof)
        })
    }

    /// Release a held attempt on an administrator's word that its machine
    /// no longer runs it, for a quarantined or permanently lost attempt.
    /// `actor` must be an administrator. A started attempt without a
    /// result is recorded as `InfraInterrupted` with `detail`, and the
    /// retry rules apply; an offer that never started is revoked.
    pub fn operator_release(
        &self,
        actor: &PrincipalId,
        attempt: &AttemptId,
        detail: &str,
    ) -> Result<ReleaseOutcome> {
        self.write(|c| {
            let actor = require_role(c, actor, Role::Admin)?;
            let detail = format!("released by {}: {detail}", actor.name);
            let started = load(c, attempt)?.start_committed();
            let released = release(
                c,
                attempt,
                Release {
                    // An offer that never started is simply revoked.
                    reason: if started { "operator" } else { "revoked" },
                    proof: None,
                    lost_detail: &detail,
                    queue_reason: None,
                    proves_isolation: true,
                },
            )?;
            Ok(match released {
                Some(disposition) => ReleaseOutcome::Released(disposition),
                None => ReleaseOutcome::AlreadyReleased,
            })
        })
    }

    /// Ask for a job to be cancelled. Repeating the call converges on the
    /// same state.
    ///
    /// * A queued job finishes as `Cancelled` in this transaction, so no
    ///   later offer can be created for it.
    /// * A job whose offer has no start permit has the offer withdrawn and
    ///   finishes as `Cancelled`; the caller tells the worker to drop it.
    /// * A job with a start permit is marked `cancel_requested`; the
    ///   caller tells the worker to cancel the returned attempt, and the
    ///   job finishes only when that attempt's release is recorded.
    pub fn request_cancel(&self, job: &JobId) -> Result<CancelOutcome> {
        self.write(|c| request_cancel(c, job))
    }

    /// Flag every unreleased attempt of a machine as uncertain because the
    /// machine is out of contact. Nothing is released: a missing worker
    /// proves nothing about its processes. The flag is cleared by
    /// [`Ledger::reconcile`]. Returns the machine's held attempts.
    pub fn mark_machine_uncertain(&self, machine: &MachineId) -> Result<Vec<AttemptTarget>> {
        self.write(|c| {
            machines::load(c, machine)?;
            let held = held_on_machine(c, machine)?;
            for attempt in &held {
                set_uncertain(c, attempt, true)?;
            }
            Ok(held.iter().map(AttemptRecord::target).collect())
        })
    }

    pub fn attempt(&self, attempt: &AttemptId) -> Result<AttemptRecord> {
        self.read(|c| load(c, attempt))
    }

    /// The attempts whose reservations are held on a machine, oldest first.
    pub fn machine_attempts(&self, machine: &MachineId) -> Result<Vec<AttemptRecord>> {
        self.read(|c| {
            machines::load(c, machine)?;
            held_on_machine(c, machine)
        })
    }

    /// Every attempt of a job, by generation.
    pub fn job_attempts(&self, job: &JobId) -> Result<Vec<AttemptRecord>> {
        self.read(|c| {
            jobs::load_row(c, job)?;
            all_rows(
                c.conn,
                &format!(
                    "SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE job_id = ?1 ORDER BY generation"
                ),
                params![job.as_str()],
                attempt_from,
            )
        })
    }
}
