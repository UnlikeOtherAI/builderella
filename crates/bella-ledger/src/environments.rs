//! Environments: services kept alive under a lease after a job's other
//! steps finish.

use crate::db::{
    Ctx, all_rows, enum_text, enumeration, exec, flag, id, json, ms, opt_row, opt_text,
    snapshot_id, text, time, to_json,
};
use crate::events::emit;
use crate::jobs::{self, JobRow};
use crate::{
    AttemptRecord, AttemptTarget, EnvironmentFinish, ExpiredEnvironment, Ledger, LedgerError,
    Result, attempts,
};
use bella_core::UnixMillis;
use bella_core::api::{EntityKind, EnvironmentState, EnvironmentView};
use bella_core::ids::{AttemptId, EnvironmentId, JobId};
use bella_core::lease::Lease;
use bella_core::protocol::Endpoint;
use bella_core::resources::ResourceVector;
use rusqlite::{Row, params};
use serde_json::json;

const ENVIRONMENT_VIEW: &str = "SELECT e.id, e.job_id, e.attempt_id, e.project_id, e.machine_id, \
     m.name AS machine_name, e.snapshot_id, e.state, e.endpoints, e.lease, e.effective_deadline, \
     e.ended_reason, a.reserved, a.held \
     FROM environments e \
     JOIN machines m ON m.id = e.machine_id \
     JOIN attempts a ON a.id = e.attempt_id";

fn view_from(row: &Row<'_>) -> Result<EnvironmentView> {
    let reserved: ResourceVector = json(row, "reserved")?;
    Ok(EnvironmentView {
        id: id(row, "id")?,
        job_id: id(row, "job_id")?,
        attempt_id: id(row, "attempt_id")?,
        project_id: id(row, "project_id")?,
        machine_id: id(row, "machine_id")?,
        machine_name: text(row, "machine_name")?,
        snapshot_id: snapshot_id(row, "snapshot_id")?,
        state: enumeration(row, "state")?,
        endpoints: json(row, "endpoints")?,
        lease: json(row, "lease")?,
        effective_deadline: time(row, "effective_deadline")?,
        // Counted against the machine for as long as the attempt holds it.
        reserved: if flag(row, "held")? {
            reserved
        } else {
            ResourceVector::ZERO
        },
        ended_reason: opt_text(row, "ended_reason")?,
    })
}

fn load(c: &Ctx<'_>, environment: &EnvironmentId) -> Result<EnvironmentView> {
    opt_row(
        c.conn,
        &format!("{ENVIRONMENT_VIEW} WHERE e.id = ?1"),
        params![environment.as_str()],
        view_from,
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("environment {environment}")))
}

fn for_attempt(c: &Ctx<'_>, attempt: &AttemptId) -> Result<Option<EnvironmentView>> {
    opt_row(
        c.conn,
        &format!("{ENVIRONMENT_VIEW} WHERE e.attempt_id = ?1"),
        params![attempt.as_str()],
        view_from,
    )
}

fn is_live(state: EnvironmentState) -> bool {
    matches!(state, EnvironmentState::Starting | EnvironmentState::Ready)
}

fn environment_event(
    c: &Ctx<'_>,
    environment: &EnvironmentView,
    kind: &str,
    data: serde_json::Value,
) -> Result<()> {
    emit(
        c,
        EntityKind::Environment,
        environment.id.as_str(),
        Some(&environment.job_id),
        kind,
        data,
    )?;
    Ok(())
}

fn store_lease(c: &Ctx<'_>, environment: &EnvironmentId, lease: &Lease) -> Result<()> {
    exec(
        c.conn,
        "UPDATE environments SET lease = ?1, effective_deadline = ?2 WHERE id = ?3",
        params![
            to_json(lease)?,
            ms(lease.effective_deadline()),
            environment.as_str()
        ],
    )?;
    Ok(())
}

/// The attempt reported its services ready. A job whose specification asks
/// for an environment gets one, with a lease that starts now.
pub(crate) fn ready(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    job: &JobRow,
    endpoints: &[Endpoint],
) -> Result<()> {
    let Some(lease_spec) = jobs::load_spec(c, &job.id)?.environment else {
        return Ok(());
    };
    if let Some(existing) = for_attempt(c, &attempt.id)? {
        exec(
            c.conn,
            "UPDATE environments SET endpoints = ?1 WHERE id = ?2",
            params![to_json(endpoints)?, existing.id.as_str()],
        )?;
        return Ok(());
    }
    let environment = EnvironmentId::generate();
    let lease = Lease::start(lease_spec, c.now);
    exec(
        c.conn,
        "INSERT INTO environments
             (id, job_id, attempt_id, project_id, machine_id, snapshot_id, state, endpoints,
              lease, effective_deadline, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'ready', ?7, ?8, ?9, ?10)",
        params![
            environment.as_str(),
            job.id.as_str(),
            attempt.id.as_str(),
            job.project_id.as_str(),
            attempt.machine_id.as_str(),
            job.snapshot_id.digest().as_str(),
            to_json(endpoints)?,
            to_json(&lease)?,
            ms(lease.effective_deadline()),
            ms(c.now)
        ],
    )?;
    exec(
        c.conn,
        "UPDATE jobs SET environment_id = ?1 WHERE id = ?2",
        params![environment.as_str(), job.id.as_str()],
    )?;
    emit(
        c,
        EntityKind::Environment,
        environment.as_str(),
        Some(&job.id),
        "environment.ready",
        json!({
            "attempt_id": attempt.id,
            "machine_id": attempt.machine_id,
            "endpoints": endpoints,
            "effective_deadline": lease.effective_deadline(),
        }),
    )?;
    Ok(())
}

/// Move a live environment to `Finishing`. Returns whether it changed.
fn begin_finish(c: &Ctx<'_>, environment: &EnvironmentView, reason: &str) -> Result<bool> {
    if !is_live(environment.state) {
        return Ok(false);
    }
    exec(
        c.conn,
        "UPDATE environments SET state = 'finishing', finish_requested_at = ?1,
                ended_reason = COALESCE(ended_reason, ?2)
         WHERE id = ?3",
        params![ms(c.now), reason, environment.id.as_str()],
    )?;
    environment_event(
        c,
        environment,
        "environment.finishing",
        json!({ "reason": reason }),
    )?;
    Ok(true)
}

fn end(c: &Ctx<'_>, environment: &EnvironmentView, reason: &str) -> Result<bool> {
    if environment.state == EnvironmentState::Ended {
        return Ok(false);
    }
    exec(
        c.conn,
        "UPDATE environments SET state = 'ended', ended_at = ?1,
                ended_reason = COALESCE(ended_reason, ?2)
         WHERE id = ?3",
        params![ms(c.now), reason, environment.id.as_str()],
    )?;
    environment_event(
        c,
        environment,
        "environment.ended",
        json!({ "reason": reason }),
    )?;
    Ok(true)
}

/// The attempt's steps exited: its environment, if any, is finishing.
pub(crate) fn finishing_for_attempt(c: &Ctx<'_>, attempt: &AttemptId, reason: &str) -> Result<()> {
    if let Some(environment) = for_attempt(c, attempt)? {
        begin_finish(c, &environment, reason)?;
    }
    Ok(())
}

/// The attempt's reservation was released: its environment is over.
pub(crate) fn end_for_attempt(c: &Ctx<'_>, attempt: &AttemptId, reason: &str) -> Result<()> {
    if let Some(environment) = for_attempt(c, attempt)? {
        end(c, &environment, &format!("attempt released ({reason})"))?;
    }
    Ok(())
}

/// The attempt was quarantined: so is whatever its environment left behind.
pub(crate) fn quarantine_for_attempt(c: &Ctx<'_>, attempt: &AttemptId, reason: &str) -> Result<()> {
    if let Some(environment) = for_attempt(c, attempt)?
        && !matches!(
            environment.state,
            EnvironmentState::Ended | EnvironmentState::Quarantined
        )
    {
        exec(
            c.conn,
            "UPDATE environments SET state = 'quarantined',
                    ended_reason = COALESCE(ended_reason, ?1)
             WHERE id = ?2",
            params![reason, environment.id.as_str()],
        )?;
        environment_event(
            c,
            &environment,
            "environment.quarantined",
            json!({ "reason": reason }),
        )?;
    }
    Ok(())
}

fn require_live(environment: &EnvironmentView) -> Result<()> {
    if is_live(environment.state) {
        Ok(())
    } else {
        Err(LedgerError::InvalidState(format!(
            "environment {} is {}",
            environment.id,
            enum_text(&environment.state)?
        )))
    }
}

impl Ledger {
    pub fn environment(&self, environment: &EnvironmentId) -> Result<EnvironmentView> {
        self.read(|c| load(c, environment))
    }

    /// The environment of a job's current attempt, if it has one.
    pub fn job_environment(&self, job: &JobId) -> Result<Option<EnvironmentView>> {
        self.read(|c| {
            jobs::load_row(c, job)?;
            opt_row(
                c.conn,
                &format!(
                    "{ENVIRONMENT_VIEW} JOIN jobs j ON j.id = e.job_id
                     WHERE e.job_id = ?1 AND j.environment_id = e.id"
                ),
                params![job.as_str()],
                view_from,
            )
        })
    }

    /// Environments, oldest first. `include_ended = false` leaves out the
    /// ones that have ended.
    pub fn environments(&self, include_ended: bool) -> Result<Vec<EnvironmentView>> {
        self.read(|c| {
            all_rows(
                c.conn,
                &format!(
                    "{ENVIRONMENT_VIEW} WHERE (?1 OR e.ended_at IS NULL)
                     ORDER BY e.created_at, e.id"
                ),
                params![include_ended],
                view_from,
            )
        })
    }

    /// Renew an environment's lease from now. Fails with `InvalidState`
    /// when the environment is not live or its lease has already ended;
    /// an ended lease is never revived. The returned view carries the new
    /// deadline, which never passes the lease's hard deadline.
    pub fn renew_environment(&self, environment: &EnvironmentId) -> Result<EnvironmentView> {
        self.write(|c| {
            let mut view = load(c, environment)?;
            require_live(&view)?;
            view.lease.renew(c.now).map_err(|e| {
                LedgerError::InvalidState(format!(
                    "environment {environment} cannot be renewed: {e:?}"
                ))
            })?;
            store_lease(c, environment, &view.lease)?;
            view.effective_deadline = view.lease.effective_deadline();
            environment_event(
                c,
                &view,
                "environment.renewed",
                json!({ "effective_deadline": view.effective_deadline }),
            )?;
            Ok(view)
        })
    }

    /// Record real use of an environment: resets its idle timer without
    /// extending the lease. Has no effect once the lease has ended.
    pub fn touch_environment(&self, environment: &EnvironmentId) -> Result<EnvironmentView> {
        self.write(|c| {
            let mut view = load(c, environment)?;
            if is_live(view.state) {
                view.lease.touch(c.now);
                store_lease(c, environment, &view.lease)?;
                view.effective_deadline = view.lease.effective_deadline();
            }
            Ok(view)
        })
    }

    /// Ask for an environment to be finished. A live environment becomes
    /// `Finishing`; the result names the attempt to send
    /// `FinishEnvironment` to. Its resources stay reserved until that
    /// attempt's release is recorded. Repeating the call changes nothing.
    pub fn request_environment_finish(
        &self,
        environment: &EnvironmentId,
        reason: &str,
    ) -> Result<EnvironmentFinish> {
        self.write(|c| {
            let before = load(c, environment)?;
            begin_finish(c, &before, reason)?;
            let view = load(c, environment)?;
            let attempt = attempts::load(c, &view.attempt_id)?;
            let target: Option<AttemptTarget> = attempt.held.then(|| attempt.target());
            Ok(EnvironmentFinish {
                environment: view,
                target,
            })
        })
    }

    /// Record that an environment has ended. Releasing its attempt does
    /// this automatically; the call is for ending the record explicitly.
    pub fn mark_environment_ended(
        &self,
        environment: &EnvironmentId,
        reason: &str,
    ) -> Result<EnvironmentView> {
        self.write(|c| {
            let before = load(c, environment)?;
            end(c, &before, reason)?;
            load(c, environment)
        })
    }

    /// Live environments whose lease has ended as of `now`, with the
    /// reason, soonest deadline first. Nothing is changed; finish them with
    /// [`Ledger::request_environment_finish`].
    pub fn expired_environments(&self, now: UnixMillis) -> Result<Vec<ExpiredEnvironment>> {
        self.read(|c| {
            let due = all_rows(
                c.conn,
                &format!(
                    "{ENVIRONMENT_VIEW}
                     WHERE e.ended_at IS NULL AND e.state IN ('starting', 'ready')
                       AND e.effective_deadline <= ?1
                     ORDER BY e.effective_deadline, e.id"
                ),
                params![ms(now)],
                view_from,
            )?;
            Ok(due
                .into_iter()
                .filter_map(|environment| {
                    environment
                        .lease
                        .ended(now)
                        .map(|reason| ExpiredEnvironment {
                            environment,
                            reason,
                        })
                })
                .collect())
        })
    }
}

/// Number of environments that have not ended.
pub(crate) fn live_count(c: &Ctx<'_>) -> Result<u64> {
    crate::db::count(
        c.conn,
        "SELECT COUNT(*) FROM environments WHERE ended_at IS NULL",
        params![],
    )
}
