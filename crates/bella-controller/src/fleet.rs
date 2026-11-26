//! Building the scheduler's input from the ledger and the hub.

use crate::state::{JobMeta, Shared, lock};
use bella_core::api::ProjectPolicy;
use bella_core::ids::{JobId, MachineId, ProjectId};
use bella_core::machine::{Inventory, MachineLifecycle, MachineProfile};
use bella_core::platform::{CapabilityState, Os};
use bella_core::resources::ResourceVector;
use bella_ledger::{Ledger, LedgerError, MachineStatus};
use bella_scheduler::{FleetState, HeldAttempt, MachineState, ProjectState, QueuedJob};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// A capability a worker reports when its work volume tells names apart
/// that differ only by case. Without it the operating system's usual
/// default is assumed.
pub const CAP_CASE_SENSITIVE_FS: &str = "fs-case-sensitive";
/// A capability a worker reports when it can create symbolic links on its
/// work volume.
pub const CAP_SYMLINKS: &str = "fs-symlinks";

fn declared(inventory: &Inventory, name: &str) -> Option<bool> {
    inventory
        .capabilities
        .iter()
        .find(|capability| capability.name == name)
        .map(|capability| capability.state == CapabilityState::Installed)
}

/// Whether the machine's work volume is case sensitive. The inventory has
/// no field for it, so a declared capability decides and otherwise the
/// operating system's default does: only Linux is assumed case sensitive.
pub(crate) fn case_sensitive_fs(inventory: &Inventory) -> bool {
    declared(inventory, CAP_CASE_SENSITIVE_FS)
        .unwrap_or(inventory.os.platform.os == Os::Linux)
}

/// Whether the worker can create symbolic links: declared, or assumed on
/// everything but Windows, where it depends on a privilege.
pub(crate) fn supports_symlinks(inventory: &Inventory) -> bool {
    declared(inventory, CAP_SYMLINKS).unwrap_or(inventory.os.platform.os != Os::Windows)
}

/// The scheduler's view of one machine. A machine counts as connected and
/// reconciled only while the hub holds a connection for it that is past
/// `Welcome` and `Reconcile`.
fn machine_state(shared: &Shared, status: MachineStatus) -> MachineState {
    let now = shared.now();
    let conn = shared.hub.ready(&status.view.id);
    let view = status.view;
    let on_lan = view.policy.profile != MachineProfile::EphemeralCloud
        && conn.as_ref().is_some_and(|conn| conn.peer_is_local());
    MachineState {
        platform: view.inventory.os.platform,
        os_version: view.inventory.os.os_version.clone(),
        case_sensitive_fs: case_sensitive_fs(&view.inventory),
        supports_symlinks: supports_symlinks(&view.inventory),
        capabilities: view.inventory.capabilities,
        managed_ceiling: view.managed_ceiling,
        effective_budget: view.effective_budget,
        reserved: view.reserved,
        held_attempts: u32::try_from(view.attempts.len()).unwrap_or(u32::MAX),
        connected: view.connected && conn.is_some(),
        admitting: status.admitting,
        flags: view.flags,
        // Age since the controller last heard from the machine, on the
        // controller's clock: the two machines' clocks are never compared.
        telemetry_age_ms: view
            .telemetry
            .as_ref()
            .and(view.last_seen_at)
            .map(|seen| u64::try_from(now.since(seen).as_millis()).unwrap_or(u64::MAX)),
        reconciled: status.reconciled && conn.is_some(),
        on_lan,
        enforcement: view.enforcement,
        policy: view.policy,
        name: view.name,
        id: view.id,
    }
}

fn job_meta(shared: &Shared, ledger: &Ledger, job: &JobId) -> Result<JobMeta, LedgerError> {
    if let Some(meta) = lock(&shared.job_meta).get(job) {
        return Ok(meta.clone());
    }
    let view = ledger.job(job)?;
    let meta = JobMeta {
        project: view.project_id,
        session: view.session.map(|session| session.id),
        priority: view.priority,
    };
    lock(&shared.job_meta).insert(job.clone(), meta.clone());
    Ok(meta)
}

/// Everything one scheduling decision depends on, read from the ledger.
///
/// The reads are separate transactions, so the state can be slightly out of
/// step with itself. That costs at most a refused offer: the ledger checks
/// capacity again inside the transaction that reserves.
pub(crate) fn fleet_state(shared: &Shared, ledger: &Ledger) -> Result<FleetState, LedgerError> {
    let now = shared.now();
    let statuses = ledger.machine_statuses()?;

    let mut held = Vec::new();
    let mut held_jobs = HashSet::new();
    let mut per_project: HashMap<ProjectId, (u32, ResourceVector)> = HashMap::new();
    for status in &statuses {
        for attempt in &status.view.attempts {
            let meta = job_meta(shared, ledger, &attempt.job_id)?;
            held_jobs.insert(attempt.job_id.clone());
            let entry = per_project
                .entry(meta.project.clone())
                .or_insert((0, ResourceVector::ZERO));
            entry.0 += 1;
            entry.1 = entry.1.saturating_add(&attempt.reserved);
            held.push(HeldAttempt {
                project: meta.project,
                session: meta.session,
                priority: meta.priority,
                reserved: attempt.reserved.clone(),
            });
        }
    }
    lock(&shared.job_meta).retain(|job, _| held_jobs.contains(job));

    let mut environments: HashMap<ProjectId, u32> = HashMap::new();
    for environment in ledger.environments(false)? {
        *environments.entry(environment.project_id).or_default() += 1;
    }

    let projects = ledger
        .projects()?
        .into_iter()
        .map(|project| {
            let (held_attempts, reserved) = per_project
                .remove(&project.id)
                .unwrap_or((0, ResourceVector::ZERO));
            ProjectState {
                environments: environments.get(&project.id).copied().unwrap_or(0),
                held_attempts,
                reserved,
                id: project.id,
                name: project.name,
                policy: project.policy,
            }
        })
        .collect();

    let machines = statuses
        .into_iter()
        .filter(|status| status.view.lifecycle == MachineLifecycle::Ready)
        .map(|status| machine_state(shared, status))
        .collect();

    let queued = ledger.queued_jobs(shared.tuning.max_queue_scan)?;
    let queued_ids: HashSet<&JobId> = queued.iter().map(|job| &job.job_id).collect();
    let rejections = {
        let mut rejections = lock(&shared.rejections);
        rejections.retain(|job, machines| {
            machines.retain(|(_, until)| *until > now);
            !machines.is_empty() && queued_ids.contains(job)
        });
        rejections.clone()
    };
    let queue = queued
        .into_iter()
        .map(|job| QueuedJob {
            recently_rejected_by: rejections.get(&job.job_id).cloned().unwrap_or_default(),
            id: job.job_id,
            project: job.project_id,
            session: job.session_id,
            spec: Arc::new(job.spec),
            origin_platform: job.origin_platform,
            queued_at: job.queued_at,
            attempts_made: job.started_attempts,
            snapshot_windows_incompatible: job.snapshot_meta.windows_incompatible,
            snapshot_case_sensitive_only: job.snapshot_meta.case_sensitive_only,
            snapshot_has_symlinks: job.snapshot_meta.has_symlinks,
            // The ledger returns a job to the queue only once its previous
            // attempt's reservation is released, and checks again when the
            // offer is created.
            has_unresolved_attempt: false,
        })
        .collect();

    Ok(FleetState {
        now,
        reconciling: shared.is_reconciling(),
        machines,
        queue,
        projects,
        held,
        config: shared.tuning.scheduler.clone(),
    })
}

/// Machine display names by id, for views that name machines.
pub(crate) fn machine_names(state: &FleetState) -> BTreeMap<MachineId, String> {
    state
        .machines
        .iter()
        .map(|machine| (machine.id.clone(), machine.name.clone()))
        .collect()
}

/// A project that does not exist yet, as it will be when the first
/// snapshot or job creates it.
pub(crate) fn unborn_project(name: &str) -> ProjectState {
    ProjectState {
        id: ProjectId::derive(format!("unborn project {name}").as_bytes()),
        name: name.to_string(),
        policy: ProjectPolicy::default(),
        held_attempts: 0,
        reserved: ResourceVector::ZERO,
        environments: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::machine::{Hardware, OsInfo};
    use bella_core::platform::{Arch, Capability, Platform};

    fn inventory(os: Os, capabilities: Vec<Capability>) -> Inventory {
        Inventory {
            revision: 1,
            hardware: Hardware {
                logical_cpus: 8,
                physical_cpus: None,
                cpu_model: String::new(),
                memory_bytes: 16 << 30,
                disks: Vec::new(),
                gpus: Vec::new(),
            },
            os: OsInfo {
                platform: Platform {
                    os,
                    arch: Arch::X86_64,
                },
                os_version: "1".into(),
                kernel: String::new(),
                hostname: "host".into(),
            },
            capabilities,
            executors: Vec::new(),
            worker_version: "0.1.0".into(),
        }
    }

    fn capability(name: &str, state: CapabilityState) -> Capability {
        Capability {
            name: name.into(),
            version: None,
            state,
            attributes: Default::default(),
        }
    }

    #[test]
    fn filesystem_facts_default_by_operating_system() {
        assert!(case_sensitive_fs(&inventory(Os::Linux, vec![])));
        assert!(!case_sensitive_fs(&inventory(Os::Macos, vec![])));
        assert!(!case_sensitive_fs(&inventory(Os::Windows, vec![])));
        assert!(supports_symlinks(&inventory(Os::Macos, vec![])));
        assert!(!supports_symlinks(&inventory(Os::Windows, vec![])));
    }

    #[test]
    fn a_declared_capability_overrides_the_default() {
        let mac = inventory(
            Os::Macos,
            vec![capability(CAP_CASE_SENSITIVE_FS, CapabilityState::Installed)],
        );
        assert!(case_sensitive_fs(&mac));
        let windows = inventory(
            Os::Windows,
            vec![capability(CAP_SYMLINKS, CapabilityState::Installed)],
        );
        assert!(supports_symlinks(&windows));
        let linux = inventory(
            Os::Linux,
            vec![capability(CAP_CASE_SENSITIVE_FS, CapabilityState::Missing)],
        );
        assert!(!case_sensitive_fs(&linux));
    }
}
