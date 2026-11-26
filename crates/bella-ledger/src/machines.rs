//! Machines: enrollment, inventory and policy revisions, connection state,
//! heartbeat observations and lifecycle.

use crate::attempts;
use crate::db::{
    Ctx, all_rows, count, enum_text, enumeration, exec, flag, id, int, json, ms, opt_id, opt_json,
    opt_row, opt_time, opt_uint, text, to_json, uint,
};
use crate::events::emit;
use crate::identity::{consume_pairing_code, ensure_hash_unused, require_role};
use crate::{
    EnrolledMachine, HeartbeatOutcome, Ledger, LedgerError, MachineStatus, NewMachine, Result,
};
use bella_core::UnixMillis;
use bella_core::api::{ActiveAttemptView, EntityKind, MachineView, Role};
use bella_core::ids::{BootId, MachineId, PrincipalId};
use bella_core::job::CleanupState;
use bella_core::machine::{
    AvailabilityFlag, Inventory, LocalControl, MachineLifecycle, MachinePolicy, Telemetry,
};
use bella_core::protocol::Heartbeat;
use bella_core::resources::ResourceVector;
use rusqlite::{Row, params};
use serde_json::json;

pub(crate) struct MachineRow {
    pub id: MachineId,
    pub name: String,
    pub lifecycle: MachineLifecycle,
    pub inventory: Inventory,
    pub policy: MachinePolicy,
    pub boot_id: Option<BootId>,
    pub connected: bool,
    pub connection_generation: u64,
    pub reconciled_epoch: Option<u64>,
    pub heartbeat_seq: u64,
    pub last_seen_at: Option<UnixMillis>,
    pub telemetry: Option<Telemetry>,
    pub effective_budget: Option<ResourceVector>,
    pub admitting: bool,
    pub worker_flags: Vec<AvailabilityFlag>,
    pub controller_flags: Vec<AvailabilityFlag>,
    pub local_control: LocalControl,
}

const MACHINE_COLUMNS: &str = "id, name, lifecycle, inventory, policy, boot_id, connected, \
     connection_generation, reconciled_epoch, heartbeat_seq, last_seen_at, telemetry, \
     effective_budget, admitting, worker_flags, controller_flags, local_control";

fn machine_from(row: &Row<'_>) -> Result<MachineRow> {
    Ok(MachineRow {
        id: id(row, "id")?,
        name: text(row, "name")?,
        lifecycle: enumeration(row, "lifecycle")?,
        inventory: json(row, "inventory")?,
        policy: json(row, "policy")?,
        boot_id: opt_id(row, "boot_id")?,
        connected: flag(row, "connected")?,
        connection_generation: uint(row, "connection_generation")?,
        reconciled_epoch: opt_uint(row, "reconciled_epoch")?,
        heartbeat_seq: uint(row, "heartbeat_seq")?,
        last_seen_at: opt_time(row, "last_seen_at")?,
        telemetry: opt_json(row, "telemetry")?,
        effective_budget: opt_json(row, "effective_budget")?,
        admitting: flag(row, "admitting")?,
        worker_flags: json(row, "worker_flags")?,
        controller_flags: json(row, "controller_flags")?,
        local_control: json(row, "local_control")?,
    })
}

pub(crate) fn load(c: &Ctx<'_>, machine: &MachineId) -> Result<MachineRow> {
    opt_row(
        c.conn,
        &format!("SELECT {MACHINE_COLUMNS} FROM machines WHERE id = ?1"),
        params![machine.as_str()],
        machine_from,
    )?
    .ok_or_else(|| LedgerError::NotFound(format!("machine {machine}")))
}

impl MachineRow {
    pub(crate) fn is_live(&self) -> bool {
        !matches!(
            self.lifecycle,
            MachineLifecycle::Retired | MachineLifecycle::Revoked
        )
    }
}

/// Sum of the machine's held reservations.
pub(crate) fn reserved(held: &[crate::AttemptRecord]) -> ResourceVector {
    held.iter().fold(ResourceVector::ZERO, |sum, attempt| {
        sum.saturating_add(&attempt.reserved)
    })
}

fn status(c: &Ctx<'_>, row: MachineRow) -> Result<MachineStatus> {
    let held = attempts::held_on_machine(c, &row.id)?;
    let mut flags: Vec<AvailabilityFlag> = row
        .worker_flags
        .iter()
        .chain(&row.controller_flags)
        .copied()
        .collect();
    if held.iter().any(|a| a.cleanup == CleanupState::Quarantined) {
        flags.push(AvailabilityFlag::Quarantined);
    }
    flags.sort();
    flags.dedup();

    let mut active = Vec::with_capacity(held.len());
    for attempt in &held {
        let (project_name, recipe) = opt_row(
            c.conn,
            "SELECT p.name, j.recipe FROM jobs j JOIN projects p ON p.id = j.project_id
             WHERE j.id = ?1",
            params![attempt.job_id.as_str()],
            |r| Ok((text(r, 0)?, text(r, 1)?)),
        )?
        .ok_or_else(|| LedgerError::Corrupt(format!("attempt {} has no job", attempt.id)))?;
        active.push(ActiveAttemptView {
            attempt_id: attempt.id.clone(),
            job_id: attempt.job_id.clone(),
            project_name,
            recipe,
            phase: attempt.phase,
            reserved: attempt.reserved.clone(),
            uncertain: attempt.uncertain,
        });
    }

    let view = MachineView {
        id: row.id,
        name: row.name,
        lifecycle: row.lifecycle,
        connected: row.connected,
        flags,
        managed_ceiling: row.policy.managed_ceiling(&row.inventory.hardware),
        enforcement: row.inventory.native_enforcement(),
        policy: row.policy,
        inventory: row.inventory,
        telemetry: row.telemetry,
        last_seen_at: row.last_seen_at,
        local_control: row.local_control,
        effective_budget: row.effective_budget,
        reserved: reserved(&held),
        attempts: active,
    };
    Ok(MachineStatus {
        view,
        admitting: row.admitting,
        reconciled: row.reconciled_epoch == Some(c.epoch),
        boot_id: row.boot_id,
        connection_generation: row.connection_generation,
    })
}

fn statuses(c: &Ctx<'_>) -> Result<Vec<MachineStatus>> {
    let rows = all_rows(
        c.conn,
        &format!("SELECT {MACHINE_COLUMNS} FROM machines ORDER BY enrolled_at, id"),
        params![],
        machine_from,
    )?;
    rows.into_iter().map(|row| status(c, row)).collect()
}

/// A display name no other live machine uses, derived from `wanted`.
fn unique_name(c: &Ctx<'_>, wanted: &str, except: Option<&MachineId>) -> Result<String> {
    let base = if wanted.trim().is_empty() {
        "machine"
    } else {
        wanted.trim()
    };
    let mut candidate = base.to_string();
    let mut suffix = 1u32;
    loop {
        let taken = count(
            c.conn,
            "SELECT COUNT(*) FROM machines
             WHERE name = ?1 AND lifecycle NOT IN ('retired', 'revoked') AND id != ?2",
            params![candidate, except.map(MachineId::as_str).unwrap_or("")],
        )?;
        if taken == 0 {
            return Ok(candidate);
        }
        suffix += 1;
        candidate = format!("{base}-{suffix}");
    }
}

pub(crate) fn enroll(c: &Ctx<'_>, new: &NewMachine<'_>) -> Result<EnrolledMachine> {
    ensure_hash_unused(c, new.token_hash)?;
    let machine_id = MachineId::generate();
    let name = unique_name(c, new.name, None)?;
    let policy = MachinePolicy::defaults_for(new.profile);
    let mut inventory = new.inventory.clone();
    inventory.revision = inventory.revision.max(1);
    exec(
        c.conn,
        "INSERT INTO machines
             (id, name, token_hash, lifecycle, inventory, policy, boot_id, local_control, enrolled_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            machine_id.as_str(),
            name,
            new.token_hash,
            enum_text(&MachineLifecycle::Ready)?,
            to_json(&inventory)?,
            to_json(&policy)?,
            new.boot_id.as_str(),
            to_json(&LocalControl::default())?,
            ms(c.now)
        ],
    )?;
    emit(
        c,
        EntityKind::Machine,
        machine_id.as_str(),
        None,
        "machine.enrolled",
        json!({ "name": name, "profile": new.profile, "platform": inventory.os.platform }),
    )?;
    Ok(EnrolledMachine {
        fleet_id: c.fleet_id.clone(),
        machine_id,
        name,
        policy,
    })
}

/// Add or remove one controller-set condition. Returns whether it changed.
fn set_condition(
    c: &Ctx<'_>,
    machine: &MachineId,
    condition: AvailabilityFlag,
    on: bool,
) -> Result<bool> {
    let row = load(c, machine)?;
    let mut flags = row.controller_flags;
    let present = flags.contains(&condition);
    if present == on {
        return Ok(false);
    }
    if on {
        flags.push(condition);
        flags.sort();
    } else {
        flags.retain(|f| *f != condition);
    }
    exec(
        c.conn,
        "UPDATE machines SET controller_flags = ?1 WHERE id = ?2",
        params![to_json(&flags)?, machine.as_str()],
    )?;
    let name = enum_text(&condition)?;
    let kind = if on {
        format!("machine.{name}")
    } else {
        format!("machine.{name}_cleared")
    };
    emit(
        c,
        EntityKind::Machine,
        machine.as_str(),
        None,
        &kind,
        serde_json::Value::Null,
    )?;
    Ok(true)
}

/// End a machine's membership and stop it from authenticating.
fn end_lifecycle(
    c: &Ctx<'_>,
    machine: &MachineId,
    to: MachineLifecycle,
    kind: &str,
) -> Result<bool> {
    let row = load(c, machine)?;
    if row.lifecycle == to {
        return Ok(false);
    }
    exec(
        c.conn,
        "UPDATE machines SET lifecycle = ?1, token_hash = NULL, connected = 0,
                reconciled_epoch = NULL, admitting = 0, retired_at = COALESCE(retired_at, ?2)
         WHERE id = ?3",
        params![enum_text(&to)?, ms(c.now), machine.as_str()],
    )?;
    emit(
        c,
        EntityKind::Machine,
        machine.as_str(),
        None,
        kind,
        serde_json::Value::Null,
    )?;
    Ok(true)
}

impl Ledger {
    /// Enroll a machine: a new [`MachineId`], the starting policy for its
    /// profile and lifecycle `Ready`. The machine is offered nothing until
    /// it connects and reconciles.
    pub fn enroll_machine(&self, new: &NewMachine<'_>) -> Result<EnrolledMachine> {
        self.write(|c| enroll(c, new))
    }

    /// Consume a pairing code and enroll the machine with the code's
    /// profile in one transaction: a code can never be spent without
    /// producing a machine, nor enroll two.
    pub fn enroll_with_pairing_code(
        &self,
        code_hash: &str,
        name: &str,
        token_hash: &str,
        boot_id: &BootId,
        inventory: &Inventory,
    ) -> Result<EnrolledMachine> {
        self.write(|c| {
            let profile = consume_pairing_code(c, code_hash)?;
            let new = NewMachine {
                name,
                token_hash,
                boot_id,
                inventory,
                profile,
            };
            enroll(c, &new)
        })
    }

    /// Store a machine's inventory. The stored revision increases by one
    /// when the content differs from what is stored and stays the same
    /// otherwise; the revision in `inventory` is ignored. Returns the
    /// stored revision.
    pub fn update_inventory(&self, machine: &MachineId, inventory: &Inventory) -> Result<u64> {
        self.write(|c| {
            let row = load(c, machine)?;
            let mut incoming = inventory.clone();
            incoming.revision = row.inventory.revision;
            if incoming == row.inventory {
                return Ok(row.inventory.revision);
            }
            incoming.revision = row.inventory.revision + 1;
            exec(
                c.conn,
                "UPDATE machines SET inventory = ?1 WHERE id = ?2",
                params![to_json(&incoming)?, machine.as_str()],
            )?;
            emit(
                c,
                EntityKind::Machine,
                machine.as_str(),
                None,
                "machine.inventory_changed",
                json!({ "revision": incoming.revision }),
            )?;
            Ok(incoming.revision)
        })
    }

    /// Replace a machine's policy. `actor` must be an administrator; the
    /// check runs in the same transaction as the change. The ledger assigns
    /// `revision = previous + 1` and ignores the revision in `policy`. A
    /// policy equal to the stored one changes nothing.
    pub fn replace_machine_policy(
        &self,
        actor: &PrincipalId,
        machine: &MachineId,
        policy: &MachinePolicy,
    ) -> Result<MachinePolicy> {
        self.write(|c| {
            let actor = require_role(c, actor, Role::Admin)?;
            let row = load(c, machine)?;
            let mut next = policy.clone();
            next.revision = row.policy.revision;
            if next == row.policy {
                return Ok(row.policy);
            }
            next.revision = row.policy.revision + 1;
            exec(
                c.conn,
                "UPDATE machines SET policy = ?1 WHERE id = ?2",
                params![to_json(&next)?, machine.as_str()],
            )?;
            emit(
                c,
                EntityKind::Machine,
                machine.as_str(),
                None,
                "machine.policy_changed",
                json!({ "revision": next.revision, "by": actor.id }),
            )?;
            Ok(next)
        })
    }

    pub fn machine_policy(&self, machine: &MachineId) -> Result<MachinePolicy> {
        self.read(|c| Ok(load(c, machine)?.policy))
    }

    /// Change a machine's display name. A name used by another live machine
    /// is a conflict.
    pub fn rename_machine(&self, machine: &MachineId, name: &str) -> Result<()> {
        self.write(|c| {
            let row = load(c, machine)?;
            let name = name.trim();
            if name.is_empty() {
                return Err(LedgerError::Invalid("empty machine name".into()));
            }
            if row.name == name {
                return Ok(());
            }
            if unique_name(c, name, Some(machine))? != name {
                return Err(LedgerError::Conflict(format!(
                    "another machine is already named `{name}`"
                )));
            }
            exec(
                c.conn,
                "UPDATE machines SET name = ?1 WHERE id = ?2",
                params![name, machine.as_str()],
            )?;
            emit(
                c,
                EntityKind::Machine,
                machine.as_str(),
                None,
                "machine.renamed",
                json!({ "from": row.name, "to": name }),
            )?;
            Ok(())
        })
    }

    /// Record that the machine opened control connection
    /// `connection_generation`. A connection older than the newest one seen
    /// is refused. The machine still has to reconcile before it is offered
    /// work.
    pub fn machine_connected(&self, machine: &MachineId, connection_generation: u64) -> Result<()> {
        self.write(|c| {
            let row = load(c, machine)?;
            if !row.is_live() {
                return Err(LedgerError::Unauthorized(format!(
                    "machine {machine} is no longer enrolled"
                )));
            }
            if connection_generation < row.connection_generation {
                return Err(LedgerError::StaleConnection {
                    machine: machine.clone(),
                    got: connection_generation,
                    current: row.connection_generation,
                });
            }
            exec(
                c.conn,
                "UPDATE machines SET connected = 1, connection_generation = ?1, heartbeat_seq = 0,
                        reconciled_epoch = NULL, last_seen_at = ?2
                 WHERE id = ?3",
                params![int(connection_generation)?, ms(c.now), machine.as_str()],
            )?;
            emit(
                c,
                EntityKind::Machine,
                machine.as_str(),
                None,
                "machine.connected",
                json!({ "connection_generation": connection_generation }),
            )?;
            Ok(())
        })
    }

    /// Record that connection `connection_generation` closed. Returns
    /// `false`, changing nothing, when a newer connection has replaced it.
    /// A disconnect releases nothing; the machine must reconcile again
    /// before it is offered work.
    pub fn machine_disconnected(
        &self,
        machine: &MachineId,
        connection_generation: u64,
    ) -> Result<bool> {
        self.write(|c| {
            let row = load(c, machine)?;
            if !row.connected || row.connection_generation != connection_generation {
                return Ok(false);
            }
            exec(
                c.conn,
                "UPDATE machines SET connected = 0, reconciled_epoch = NULL, admitting = 0
                 WHERE id = ?1",
                params![machine.as_str()],
            )?;
            emit(
                c,
                EntityKind::Machine,
                machine.as_str(),
                None,
                "machine.disconnected",
                json!({ "connection_generation": connection_generation }),
            )?;
            Ok(true)
        })
    }

    /// Store what a heartbeat observed: last-seen time, telemetry,
    /// effective budget, admission, flags, local controls and the usage of
    /// the attempts it lists. A heartbeat from a connection older than the
    /// current one is refused. The stored boot id is not changed here;
    /// only [`Ledger::reconcile`] accepts a new boot.
    pub fn record_heartbeat(&self, heartbeat: &Heartbeat) -> Result<HeartbeatOutcome> {
        self.write(|c| {
            let machine = &heartbeat.machine_id;
            let row = load(c, machine)?;
            if !row.is_live() {
                return Err(LedgerError::Unauthorized(format!(
                    "machine {machine} is no longer enrolled"
                )));
            }
            if heartbeat.connection_generation < row.connection_generation {
                return Err(LedgerError::StaleConnection {
                    machine: machine.clone(),
                    got: heartbeat.connection_generation,
                    current: row.connection_generation,
                });
            }
            let new_connection = heartbeat.connection_generation > row.connection_generation;
            let boot_changed = row.boot_id.as_ref() != Some(&heartbeat.boot_id);
            let regressed = !new_connection && heartbeat.seq <= row.heartbeat_seq;
            let mut flags = heartbeat.flags.clone();
            flags.sort();
            flags.dedup();

            exec(
                c.conn,
                "UPDATE machines SET connection_generation = ?1, heartbeat_seq = ?2,
                        last_seen_at = ?3, telemetry = ?4, effective_budget = ?5, admitting = ?6,
                        worker_flags = ?7, local_control = ?8
                 WHERE id = ?9",
                params![
                    int(heartbeat.connection_generation)?,
                    int(heartbeat.seq)?,
                    ms(c.now),
                    to_json(&heartbeat.telemetry)?,
                    to_json(&heartbeat.effective_budget)?,
                    heartbeat.admitting,
                    to_json(&flags)?,
                    to_json(&heartbeat.local_control)?,
                    machine.as_str()
                ],
            )?;
            for report in &heartbeat.attempts {
                if let Some(usage) = &report.usage {
                    exec(
                        c.conn,
                        "UPDATE attempts SET usage = ?1
                         WHERE id = ?2 AND machine_id = ?3 AND generation = ?4 AND held = 1",
                        params![
                            to_json(usage)?,
                            report.attempt.attempt_id.as_str(),
                            machine.as_str(),
                            report.attempt.generation
                        ],
                    )?;
                }
            }
            if flags != row.worker_flags {
                emit(
                    c,
                    EntityKind::Machine,
                    machine.as_str(),
                    None,
                    "machine.flags_changed",
                    json!({ "flags": flags }),
                )?;
            }
            if heartbeat.local_control != row.local_control {
                emit(
                    c,
                    EntityKind::Machine,
                    machine.as_str(),
                    None,
                    "machine.local_control_changed",
                    json!({ "local_control": heartbeat.local_control }),
                )?;
            }
            Ok(HeartbeatOutcome {
                needs_reconcile: boot_changed
                    || regressed
                    || new_connection
                    || row.reconciled_epoch != Some(c.epoch),
                policy_stale: heartbeat.policy_revision != row.policy.revision,
                policy_revision: row.policy.revision,
            })
        })
    }

    /// Set or clear a condition the controller observes or decides:
    /// `Stale`, `Unreachable`, `Draining`, `Upgrading` and so on. Conditions
    /// the worker reports arrive through heartbeats and are kept
    /// separately; a machine's view shows the union. Returns whether
    /// anything changed. Setting a condition releases nothing.
    pub fn set_machine_condition(
        &self,
        machine: &MachineId,
        condition: AvailabilityFlag,
        on: bool,
    ) -> Result<bool> {
        self.write(|c| set_condition(c, machine, condition, on))
    }

    /// Retire a machine: it stops authenticating and is offered nothing.
    /// Reservations it holds stay held until they are released by proof,
    /// reconciliation or an operator. Returns `false` if already retired.
    pub fn retire_machine(&self, machine: &MachineId) -> Result<bool> {
        self.write(|c| end_lifecycle(c, machine, MachineLifecycle::Retired, "machine.retired"))
    }

    /// Revoke a machine's credentials because they may be compromised.
    /// Like [`Ledger::retire_machine`], with lifecycle `Revoked`.
    pub fn revoke_machine(&self, machine: &MachineId) -> Result<bool> {
        self.write(|c| end_lifecycle(c, machine, MachineLifecycle::Revoked, "machine.revoked"))
    }

    /// One machine as the API shows it. `reserved` is the sum of its held
    /// reservations, including uncertain and quarantined ones.
    pub fn machine_view(&self, machine: &MachineId) -> Result<MachineView> {
        Ok(self.machine_status(machine)?.view)
    }

    /// Every machine, in enrollment order.
    pub fn machine_views(&self) -> Result<Vec<MachineView>> {
        Ok(self
            .machine_statuses()?
            .into_iter()
            .map(|status| status.view)
            .collect())
    }

    /// A machine's view plus what the scheduler needs beyond it.
    pub fn machine_status(&self, machine: &MachineId) -> Result<MachineStatus> {
        self.read(|c| status(c, load(c, machine)?))
    }

    /// Every machine's status, in enrollment order.
    pub fn machine_statuses(&self) -> Result<Vec<MachineStatus>> {
        self.read(statuses)
    }
}

pub(crate) fn views(c: &Ctx<'_>) -> Result<Vec<MachineView>> {
    Ok(statuses(c)?.into_iter().map(|status| status.view).collect())
}
