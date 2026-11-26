//! Read models for the scheduler, the API and the dashboard.

use crate::db::{Ctx, all_rows, count, id, json, text, time, uint32};
use crate::{Ledger, Result, environments, jobs, machines};
use bella_core::UnixMillis;
use bella_core::api::{FleetOverview, ProjectPolicy, ProjectShare, QueueEntry, QueueView};
use bella_core::ids::{JobId, ProjectId};
use bella_core::resources::ResourceVector;
use rusqlite::params;

fn shares(c: &Ctx<'_>) -> Result<Vec<ProjectShare>> {
    let mut shares = all_rows(
        c.conn,
        "SELECT p.id, p.name, p.policy,
                (SELECT COUNT(*) FROM jobs j
                  WHERE j.project_id = p.id AND j.phase IN ('capturing', 'queued')) AS queued,
                (SELECT COUNT(*) FROM jobs j
                  WHERE j.project_id = p.id
                    AND j.phase NOT IN ('capturing', 'queued', 'finished')) AS running
         FROM projects p ORDER BY p.name",
        params![],
        |row| {
            let policy: ProjectPolicy = json(row, "policy")?;
            Ok(ProjectShare {
                project_id: id(row, "id")?,
                name: text(row, "name")?,
                weight: policy.weight,
                queued: uint32(row, "queued")?,
                running: uint32(row, "running")?,
                reserved: ResourceVector::ZERO,
            })
        },
    )?;
    shares.retain(|share| share.queued > 0 || share.running > 0);
    for share in &mut shares {
        let held = all_rows(
            c.conn,
            "SELECT a.reserved FROM attempts a JOIN jobs j ON j.id = a.job_id
             WHERE j.project_id = ?1 AND a.held = 1",
            params![share.project_id.as_str()],
            |row| json::<ResourceVector, _>(row, 0),
        )?;
        share.reserved = held
            .iter()
            .fold(ResourceVector::ZERO, |sum, r| sum.saturating_add(r));
    }
    Ok(shares)
}

fn queue(c: &Ctx<'_>) -> Result<QueueView> {
    let waiting = all_rows(
        c.conn,
        "SELECT id, queued_at FROM jobs WHERE phase = 'queued' ORDER BY seq",
        params![],
        |row| Ok((id::<JobId, _>(row, 0)?, time(row, 1)?)),
    )?;
    let mut entries = Vec::with_capacity(waiting.len());
    for (index, (job, queued_at)) in waiting.iter().enumerate() {
        entries.push(QueueEntry {
            position: u32::try_from(index + 1).unwrap_or(u32::MAX),
            waiting_ms: waiting_ms(c.now, *queued_at),
            job: jobs::view(c, job, false)?,
        });
    }
    Ok(QueueView {
        entries,
        shares: shares(c)?,
    })
}

fn waiting_ms(now: UnixMillis, since: UnixMillis) -> u64 {
    now.0.saturating_sub(since.0)
}

impl Ledger {
    /// Queued jobs in submission order, with how long each has waited
    /// since it last entered the queue, and the share of every project
    /// that has queued or running work: its weight, job counts and the sum
    /// of its held reservations.
    pub fn queue_view(&self) -> Result<QueueView> {
        self.read(queue)
    }

    /// Sum of the reservations a project's attempts currently hold.
    pub fn project_reserved(&self, project: &ProjectId) -> Result<ResourceVector> {
        self.read(|c| {
            let held = all_rows(
                c.conn,
                "SELECT a.reserved FROM attempts a JOIN jobs j ON j.id = a.job_id
                 WHERE j.project_id = ?1 AND a.held = 1",
                params![project.as_str()],
                |row| json::<ResourceVector, _>(row, 0),
            )?;
            Ok(held
                .iter()
                .fold(ResourceVector::ZERO, |sum, r| sum.saturating_add(r)))
        })
    }

    /// Machines with their reservations and the fleet-wide counts: queued
    /// jobs, attempts that hold a start permit and are not yet released,
    /// and environments that have not ended. `controller_version` and
    /// `reconciling` are the caller's to report.
    pub fn fleet_overview(
        &self,
        controller_version: &str,
        reconciling: bool,
    ) -> Result<FleetOverview> {
        self.read(|c| {
            let running = count(
                c.conn,
                "SELECT COUNT(*) FROM attempts
                 WHERE held = 1 AND start_committed_at IS NOT NULL",
                params![],
            )?;
            Ok(FleetOverview {
                fleet_id: c.fleet_id.clone(),
                controller_version: controller_version.to_string(),
                controller_epoch: c.epoch,
                server_time: c.now,
                reconciling,
                machines: machines::views(c)?,
                queued: jobs::counts(c)?.queued,
                running: u32::try_from(running).unwrap_or(u32::MAX),
                environments: u32::try_from(environments::live_count(c)?).unwrap_or(u32::MAX),
            })
        })
    }
}
