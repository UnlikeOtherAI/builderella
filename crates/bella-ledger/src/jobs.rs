//! Jobs: idempotent submission, queries, and the job-level phase changes
//! the attempt transitions drive.

use crate::db::{
    Ctx, all_rows, clamp_int, count, enum_text, enumeration, exec, flag, id, json, ms, opt_id,
    opt_json, opt_row, opt_time, snapshot_id, text, time, to_json, uint, uint32,
};
use crate::events::emit;
use crate::identity::load_principal;
use crate::{JobCounts, Ledger, LedgerError, QueuedJob, Result, phase, projects};
use bella_core::api::{
    AttemptView, EntityKind, JobView, ListJobsQuery, StepView, SubmitJobRequest,
};
use bella_core::ids::{Digest, JobId, PrincipalId, ProjectId, SnapshotId};
use bella_core::job::{
    CleanupState, JobPhase, JobSpec, Outcome, Priority, QueueReason, RetryClass,
};
use rusqlite::{Row, params};
use serde_json::json;

/// The part of a job that transitions read.
pub(crate) struct JobRow {
    pub id: JobId,
    pub project_id: ProjectId,
    pub snapshot_id: SnapshotId,
    pub phase: JobPhase,
    pub outcome: Option<Outcome>,
    pub cleanup: CleanupState,
    pub cancel_requested: bool,
    /// Generation of the newest attempt; 0 before the first offer.
    pub generation: u32,
    pub retry_class: RetryClass,
    pub max_attempts: u32,
    pub max_log_bytes: u64,
}

pub(crate) fn load_row(c: &Ctx<'_>, job: &JobId) -> Result<JobRow> {
    opt_row(
        c.conn,
        "SELECT id, project_id, snapshot_id, phase, outcome, cleanup, cancel_requested,
                generation, retry_class, max_attempts, max_log_bytes
         FROM jobs WHERE id = ?1",
        params![job.as_str()],
        |row| {
            Ok(JobRow {
                id: id(row, "id")?,
                project_id: id(row, "project_id")?,
                snapshot_id: snapshot_id(row, "snapshot_id")?,
                phase: enumeration(row, "phase")?,
                outcome: opt_json(row, "outcome")?,
                cleanup: enumeration(row, "cleanup")?,
                cancel_requested: flag(row, "cancel_requested")?,
                generation: uint32(row, "generation")?,
                retry_class: enumeration(row, "retry_class")?,
                max_attempts: uint32(row, "max_attempts")?,
                max_log_bytes: uint(row, "max_log_bytes")?,
            })
        },
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("job {job}")))
}

pub(crate) fn load_spec(c: &Ctx<'_>, job: &JobId) -> Result<JobSpec> {
    opt_row(
        c.conn,
        "SELECT spec FROM jobs WHERE id = ?1",
        params![job.as_str()],
        |row| json(row, 0),
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("job {job}")))
}

/// Attempts of this job that received a start permit.
pub(crate) fn started_attempts(c: &Ctx<'_>, job: &JobId) -> Result<u64> {
    count(
        c.conn,
        "SELECT COUNT(*) FROM attempts WHERE job_id = ?1 AND start_committed_at IS NOT NULL",
        params![job.as_str()],
    )
}

/// May a fresh attempt be created automatically after `outcome`?
pub(crate) fn will_retry(c: &Ctx<'_>, job: &JobRow, outcome: &Outcome) -> Result<bool> {
    if job.cancel_requested || !outcome.is_retryable(job.retry_class) {
        return Ok(false);
    }
    Ok(started_attempts(c, &job.id)? < u64::from(job.max_attempts.max(1)))
}

/// Move a job to `to` as part of its attempt's progress. Does nothing when
/// it is already there.
pub(crate) fn set_phase(c: &Ctx<'_>, job: &JobRow, to: JobPhase) -> Result<()> {
    if job.phase == to {
        return Ok(());
    }
    phase::check(&format!("job {}", job.id), job.phase, to)?;
    let name = enum_text(&to)?;
    exec(
        c.conn,
        "UPDATE jobs SET phase = ?1, queue_reason = NULL WHERE id = ?2",
        params![name, job.id.as_str()],
    )?;
    emit(
        c,
        EntityKind::Job,
        job.id.as_str(),
        Some(&job.id),
        &format!("job.{name}"),
        json!({ "generation": job.generation }),
    )?;
    Ok(())
}

/// Finish a job. An outcome the job already carries is kept.
pub(crate) fn finish(
    c: &Ctx<'_>,
    job: &JobRow,
    outcome: &Outcome,
    cleanup: CleanupState,
) -> Result<()> {
    phase::check(&format!("job {}", job.id), job.phase, JobPhase::Finished)?;
    let outcome = job.outcome.as_ref().unwrap_or(outcome);
    exec(
        c.conn,
        "UPDATE jobs SET phase = 'finished', outcome = ?1, cleanup = ?2, queue_reason = NULL
         WHERE id = ?3",
        params![to_json(outcome)?, enum_text(&cleanup)?, job.id.as_str()],
    )?;
    emit(
        c,
        EntityKind::Job,
        job.id.as_str(),
        Some(&job.id),
        "job.finished",
        json!({ "outcome": outcome, "generation": job.generation }),
    )?;
    Ok(())
}

/// Put a job back in the queue after its attempt's reservation was
/// released. `retry` marks a fresh attempt after a retryable outcome, as
/// opposed to an offer that never started.
pub(crate) fn requeue(
    c: &Ctx<'_>,
    job: &JobRow,
    reason: Option<&QueueReason>,
    retry: bool,
) -> Result<()> {
    phase::check(&format!("job {}", job.id), job.phase, JobPhase::Queued)?;
    let reason_json = match reason {
        Some(reason) => Some(to_json(reason)?),
        None => None,
    };
    exec(
        c.conn,
        "UPDATE jobs SET phase = 'queued', outcome = NULL, cleanup = 'not_started',
                queue_reason = ?1, environment_id = NULL,
                queued_at = CASE WHEN ?2 THEN ?3 ELSE queued_at END
         WHERE id = ?4",
        params![reason_json, retry, ms(c.now), job.id.as_str()],
    )?;
    emit(
        c,
        EntityKind::Job,
        job.id.as_str(),
        Some(&job.id),
        "job.queued",
        json!({ "retry": retry, "after_generation": job.generation }),
    )?;
    Ok(())
}

// -------------------------------------------------------------------- views

const JOB_VIEW: &str = "SELECT j.id, j.project_id, p.name AS project_name, j.principal_id, \
     j.session, j.snapshot_id, j.recipe, j.priority, j.phase, j.outcome, j.cleanup, \
     j.cancel_requested, j.queue_reason, j.envelope, j.created_at, j.updated_at, \
     j.environment_id, j.last_event, j.spec \
     FROM jobs j JOIN projects p ON p.id = j.project_id";

fn view_from(row: &Row<'_>, with_spec: bool) -> Result<JobView> {
    let phase: JobPhase = enumeration(row, "phase")?;
    Ok(JobView {
        id: id(row, "id")?,
        project_id: id(row, "project_id")?,
        project_name: text(row, "project_name")?,
        principal_id: id(row, "principal_id")?,
        session: opt_json(row, "session")?,
        snapshot_id: snapshot_id(row, "snapshot_id")?,
        recipe: text(row, "recipe")?,
        priority: enumeration(row, "priority")?,
        phase,
        outcome: opt_json(row, "outcome")?,
        cleanup: enumeration(row, "cleanup")?,
        cancel_requested: flag(row, "cancel_requested")?,
        queue_reason: if phase == JobPhase::Queued {
            opt_json(row, "queue_reason")?
        } else {
            None
        },
        envelope: json(row, "envelope")?,
        created_at: time(row, "created_at")?,
        updated_at: time(row, "updated_at")?,
        attempts: Vec::new(),
        environment_id: opt_id(row, "environment_id")?,
        last_event: uint(row, "last_event")?,
        spec: if with_spec {
            Some(json(row, "spec")?)
        } else {
            None
        },
    })
}

fn steps(c: &Ctx<'_>, attempt: &str) -> Result<Vec<StepView>> {
    all_rows(
        c.conn,
        "SELECT name, kind, state, exit_code, started_at, finished_at FROM attempt_steps
         WHERE attempt_id = ?1 ORDER BY position",
        params![attempt],
        |row| {
            Ok(StepView {
                name: text(row, "name")?,
                kind: enumeration(row, "kind")?,
                state: enumeration(row, "state")?,
                exit_code: row.get::<_, Option<i32>>("exit_code")?,
                started_at: opt_time(row, "started_at")?,
                finished_at: opt_time(row, "finished_at")?,
            })
        },
    )
}

fn attempt_views(c: &Ctx<'_>, job: &JobId) -> Result<Vec<AttemptView>> {
    let mut views = all_rows(
        c.conn,
        "SELECT a.id, a.generation, a.machine_id, m.name AS machine_name, a.phase, a.outcome,
                a.cleanup, a.uncertain, a.start_committed_at, a.reserved, a.usage, a.offered_at,
                a.started_at, a.exited_at, a.released_at
         FROM attempts a JOIN machines m ON m.id = a.machine_id
         WHERE a.job_id = ?1 ORDER BY a.generation",
        params![job.as_str()],
        |row| {
            Ok(AttemptView {
                id: id(row, "id")?,
                generation: uint32(row, "generation")?,
                machine_id: id(row, "machine_id")?,
                machine_name: text(row, "machine_name")?,
                phase: enumeration(row, "phase")?,
                outcome: opt_json(row, "outcome")?,
                cleanup: enumeration(row, "cleanup")?,
                uncertain: flag(row, "uncertain")?,
                start_committed: opt_time(row, "start_committed_at")?.is_some(),
                reserved: json(row, "reserved")?,
                usage: opt_json(row, "usage")?,
                offered_at: time(row, "offered_at")?,
                started_at: opt_time(row, "started_at")?,
                exited_at: opt_time(row, "exited_at")?,
                released_at: opt_time(row, "released_at")?,
                steps: Vec::new(),
            })
        },
    )?;
    for view in &mut views {
        view.steps = steps(c, view.id.as_str())?;
    }
    Ok(views)
}

pub(crate) fn view(c: &Ctx<'_>, job: &JobId, with_spec: bool) -> Result<JobView> {
    let mut view = opt_row(
        c.conn,
        &format!("{JOB_VIEW} WHERE j.id = ?1"),
        params![job.as_str()],
        |row| view_from(row, with_spec),
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("job {job}")))?;
    view.attempts = attempt_views(c, job)?;
    Ok(view)
}

/// Views of the jobs selected by a `WHERE ... ORDER BY ... LIMIT` tail.
pub(crate) fn views_where(
    c: &Ctx<'_>,
    tail: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<JobView>> {
    let mut views = all_rows(c.conn, &format!("{JOB_VIEW} {tail}"), params, |row| {
        view_from(row, false)
    })?;
    for view in &mut views {
        view.attempts = attempt_views(c, &view.id)?;
    }
    Ok(views)
}

fn submit(
    c: &Ctx<'_>,
    principal: &PrincipalId,
    idempotency_key: Option<&str>,
    request: &SubmitJobRequest,
) -> Result<JobView> {
    let submitter = load_principal(c, principal)?;
    if submitter.revoked_at.is_some() {
        return Err(LedgerError::Unauthorized(format!(
            "principal {principal} is revoked"
        )));
    }
    let request_digest = Digest::of_bytes(to_json(request)?.as_bytes());
    if let Some(key) = idempotency_key {
        if key.is_empty() || key.len() > 255 {
            return Err(LedgerError::Invalid(
                "an idempotency key must have 1 to 255 bytes".into(),
            ));
        }
        let previous = opt_row(
            c.conn,
            "SELECT request_digest, job_id FROM idempotency_keys
             WHERE principal_id = ?1 AND key = ?2",
            params![principal.as_str(), key],
            |row| Ok((text(row, 0)?, id::<JobId, _>(row, 1)?)),
        )?;
        if let Some((digest, job)) = previous {
            if digest != request_digest.as_str() {
                return Err(LedgerError::IdempotencyMismatch {
                    key: key.to_string(),
                });
            }
            return view(c, &job, false);
        }
    }

    let spec = &request.spec;
    if spec.steps.is_empty() {
        return Err(LedgerError::Invalid("a job needs at least one step".into()));
    }
    if !spec.envelope.is_well_formed() {
        return Err(LedgerError::Invalid(
            "the resource envelope must satisfy initial <= peak <= maximum".into(),
        ));
    }
    let project = projects::get_or_create(c, &request.project)?;
    if spec.priority == Priority::Urgent && !project.policy.allow_urgent {
        return Err(LedgerError::PolicyDenied(format!(
            "project `{}` does not allow urgent jobs",
            project.name
        )));
    }
    projects::snapshot(c, &request.snapshot_id)?;

    let job = JobId::generate();
    exec(
        c.conn,
        "INSERT INTO jobs
             (id, project_id, principal_id, session_id, session, snapshot_id, origin_platform,
              recipe, priority, spec, envelope, retry_class, max_attempts, max_log_bytes, phase,
              cleanup, created_at, queued_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'queued',
                 'not_started', ?15, ?15, ?15)",
        params![
            job.as_str(),
            project.id.as_str(),
            principal.as_str(),
            request.session.as_ref().map(|s| s.id.as_str()),
            match &request.session {
                Some(session) => Some(to_json(session)?),
                None => None,
            },
            request.snapshot_id.digest().as_str(),
            to_json(&request.origin_platform)?,
            spec.recipe,
            enum_text(&spec.priority)?,
            to_json(spec)?,
            to_json(&spec.envelope)?,
            enum_text(&spec.retry_class)?,
            spec.max_attempts.max(1),
            clamp_int(spec.outputs.max_log_bytes),
            ms(c.now)
        ],
    )?;
    if let Some(key) = idempotency_key {
        exec(
            c.conn,
            "INSERT INTO idempotency_keys (principal_id, key, request_digest, job_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                principal.as_str(),
                key,
                request_digest.as_str(),
                job.as_str(),
                ms(c.now)
            ],
        )?;
    }
    emit(
        c,
        EntityKind::Job,
        job.as_str(),
        Some(&job),
        "job.queued",
        json!({
            "project_id": project.id,
            "principal_id": principal,
            "snapshot_id": request.snapshot_id,
            "recipe": spec.recipe,
            "priority": spec.priority,
        }),
    )?;
    view(c, &job, false)
}

fn queued_from(row: &Row<'_>) -> Result<QueuedJob> {
    Ok(QueuedJob {
        job_id: id(row, "id")?,
        submit_seq: uint(row, "seq")?,
        project_id: id(row, "project_id")?,
        project_name: text(row, "project_name")?,
        principal_id: id(row, "principal_id")?,
        session: opt_json(row, "session")?,
        session_id: opt_id(row, "session_id")?,
        snapshot_id: snapshot_id(row, "snapshot_id")?,
        snapshot_meta: json(row, "snapshot_meta")?,
        origin_platform: json(row, "origin_platform")?,
        priority: enumeration(row, "priority")?,
        spec: json(row, "spec")?,
        created_at: time(row, "created_at")?,
        queued_at: time(row, "queued_at")?,
        started_attempts: uint32(row, "started_attempts")?,
        last_machine: opt_id(row, "last_machine")?,
    })
}

impl Ledger {
    /// Submit a job. It starts `Queued` with a frozen copy of the
    /// specification.
    ///
    /// With an idempotency key, the same principal submitting the same
    /// request again gets the job created the first time and no new row;
    /// the same key with a different request is
    /// [`LedgerError::IdempotencyMismatch`]. Keys are scoped to the
    /// principal. Without a key every call creates a job.
    ///
    /// The project is created with the default policy if needed. The
    /// snapshot must be registered. An urgent job needs a project that
    /// allows urgent work.
    pub fn submit_job(
        &self,
        principal: &PrincipalId,
        idempotency_key: Option<&str>,
        request: &SubmitJobRequest,
    ) -> Result<JobView> {
        self.write(|c| submit(c, principal, idempotency_key, request))
    }

    /// One job with its attempts, steps and specification.
    pub fn job(&self, job: &JobId) -> Result<JobView> {
        self.read(|c| view(c, job, true))
    }

    /// Jobs matching `query`, newest first, without specifications. The
    /// limit defaults to 100 and is capped at 1000. An unknown project
    /// name matches nothing.
    pub fn list_jobs(&self, query: &ListJobsQuery) -> Result<Vec<JobView>> {
        self.read(|c| {
            let project = match &query.project {
                Some(name) => match projects::by_name(c, name)? {
                    Some(project) => Some(project.id),
                    None => return Ok(Vec::new()),
                },
                None => None,
            };
            let limit = i64::from(query.limit.unwrap_or(100).clamp(1, 1000));
            views_where(
                c,
                "WHERE (?1 IS NULL OR j.project_id = ?1)
                   AND (?2 IS NULL OR j.session_id = ?2)
                   AND (?3 IS NULL OR (j.phase != 'finished') = ?3)
                 ORDER BY j.seq DESC LIMIT ?4",
                params![
                    project.as_ref().map(ProjectId::as_str),
                    query.session.as_ref().map(|s| s.as_str()),
                    query.active,
                    limit
                ],
            )
        })
    }

    /// Queued jobs in submission order, with what a scheduler needs to
    /// place them. At most `limit` jobs.
    pub fn queued_jobs(&self, limit: u32) -> Result<Vec<QueuedJob>> {
        self.read(|c| {
            all_rows(
                c.conn,
                "SELECT j.seq, j.id, j.project_id, p.name AS project_name, j.principal_id,
                        j.session, j.session_id, j.snapshot_id, s.meta AS snapshot_meta,
                        j.origin_platform, j.priority, j.spec, j.created_at, j.queued_at,
                        (SELECT COUNT(*) FROM attempts a
                          WHERE a.job_id = j.id AND a.start_committed_at IS NOT NULL)
                            AS started_attempts,
                        (SELECT a.machine_id FROM attempts a
                          WHERE a.job_id = j.id ORDER BY a.generation DESC LIMIT 1)
                            AS last_machine
                 FROM jobs j
                 JOIN projects p ON p.id = j.project_id
                 JOIN snapshots s ON s.id = j.snapshot_id
                 WHERE j.phase = 'queued'
                 ORDER BY j.seq LIMIT ?1",
                params![i64::from(limit.max(1))],
                queued_from,
            )
        })
    }

    /// Number of jobs in each phase.
    pub fn job_counts(&self) -> Result<JobCounts> {
        self.read(counts)
    }

    /// Record why a queued job is waiting. Ignored, returning `false`, when
    /// the job is not queued. This is an explanation, not a transition: it
    /// writes no event.
    pub fn set_queue_reason(&self, job: &JobId, reason: &QueueReason) -> Result<bool> {
        self.write(|c| set_queue_reason(c, job, reason))
    }

    /// [`Ledger::set_queue_reason`] for many jobs in one transaction.
    /// Returns how many were updated.
    pub fn set_queue_reasons(&self, reasons: &[(JobId, QueueReason)]) -> Result<u32> {
        self.write(|c| {
            let mut updated = 0;
            for (job, reason) in reasons {
                if set_queue_reason(c, job, reason)? {
                    updated += 1;
                }
            }
            Ok(updated)
        })
    }
}

fn set_queue_reason(c: &Ctx<'_>, job: &JobId, reason: &QueueReason) -> Result<bool> {
    let changed = exec(
        c.conn,
        "UPDATE jobs SET queue_reason = ?1 WHERE id = ?2 AND phase = 'queued'",
        params![to_json(reason)?, job.as_str()],
    )?;
    Ok(changed == 1)
}

pub(crate) fn counts(c: &Ctx<'_>) -> Result<JobCounts> {
    let rows = all_rows(
        c.conn,
        "SELECT phase, COUNT(*) FROM jobs GROUP BY phase",
        params![],
        |row| Ok((enumeration::<JobPhase, _>(row, 0)?, uint32(row, 1)?)),
    )?;
    let mut counts = JobCounts::default();
    for (phase, n) in rows {
        match phase {
            JobPhase::Capturing | JobPhase::Queued => counts.queued += n,
            JobPhase::Offered => counts.offered += n,
            JobPhase::Preparing => counts.preparing += n,
            JobPhase::Executing => counts.executing += n,
            JobPhase::Collecting => counts.collecting += n,
            JobPhase::Cleaning => counts.cleaning += n,
            JobPhase::Quarantined => counts.quarantined += n,
            JobPhase::Finished => counts.finished += n,
        }
    }
    Ok(counts)
}
