//! Fleet overview, machines and the queue.

use super::{ApiJson, ApiPath, CONTROLLER_VERSION, parse_id};
use crate::auth::{Admin, Viewer};
use crate::error::{ApiResult, Failure};
use crate::state::App;
use axum::Json;
use axum::extract::State;
use bella_core::api::{FleetOverview, MachineView, QueueView, UpdateMachineRequest};
use bella_core::ids::MachineId;
use bella_core::machine::{AvailabilityFlag, MachinePolicy};
use bella_core::protocol::ControllerCommand;
use tracing::info;

/// `GET /fleet`.
pub(crate) async fn overview(
    State(app): State<App>,
    _viewer: Viewer,
) -> ApiResult<Json<FleetOverview>> {
    let reconciling = app.is_reconciling();
    Ok(Json(
        app.db(move |ledger| ledger.fleet_overview(CONTROLLER_VERSION, reconciling))
            .await?,
    ))
}

/// `GET /machines/{id}`.
pub(crate) async fn machine(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<MachineView>> {
    let id: MachineId = parse_id(&id)?;
    Ok(Json(app.db(move |ledger| ledger.machine_view(&id)).await?))
}

fn validate_policy(policy: &MachinePolicy) -> ApiResult<()> {
    let percentages = [
        ("foreground_cpu_percent", policy.foreground_cpu_percent),
        (
            "foreground_memory_percent",
            policy.foreground_memory_percent,
        ),
        ("disk_free_floor_percent", policy.disk_free_floor_percent),
        (
            "min_battery_percent",
            policy.min_battery_percent.unwrap_or(0),
        ),
    ];
    for (field, value) in percentages {
        if value > 100 {
            return Err(Failure::bad_request(format!(
                "{field} is a percentage and cannot exceed 100"
            )));
        }
    }
    if policy.max_concurrent_attempts == Some(0) {
        return Err(Failure::bad_request(
            "max_concurrent_attempts of 0 would accept nothing; set accept_builds to false instead",
        ));
    }
    Ok(())
}

/// `PATCH /machines/{id}`: rename, replace the policy, drain or retire.
/// Administrators only. The fields are applied in that order, each in its
/// own ledger transaction; an error stops at the field that failed and
/// leaves the earlier ones applied.
pub(crate) async fn update_machine(
    State(app): State<App>,
    Admin(admin): Admin,
    ApiPath(id): ApiPath,
    ApiJson(request): ApiJson<UpdateMachineRequest>,
) -> ApiResult<Json<MachineView>> {
    let id: MachineId = parse_id(&id)?;
    let UpdateMachineRequest {
        name,
        policy,
        drain,
        retire,
    } = request;
    if let Some(name) = &name
        && (name.trim().is_empty()
            || name.chars().count() > 128
            || name.chars().any(char::is_control))
    {
        return Err(Failure::bad_request(
            "a machine's name has 1 to 128 characters and no control characters",
        ));
    }
    if let Some(policy) = &policy {
        validate_policy(policy)?;
    }
    let retire = retire == Some(true);

    let (machine, actor) = (id.clone(), admin.id.clone());
    let (view, new_policy) = app
        .db(move |ledger| {
            // Refuse an unknown machine before changing anything.
            ledger.machine_view(&machine)?;
            if let Some(name) = name {
                ledger.rename_machine(&machine, name.trim())?;
            }
            let new_policy = match policy {
                Some(policy) => Some(ledger.replace_machine_policy(&actor, &machine, &policy)?),
                None => None,
            };
            if let Some(drain) = drain {
                ledger.set_machine_condition(&machine, AvailabilityFlag::Draining, drain)?;
            }
            if retire {
                ledger.retire_machine(&machine)?;
            }
            Ok((ledger.machine_view(&machine)?, new_policy))
        })
        .await?;

    // The ledger is what scheduling obeys; the commands only let the worker
    // follow sooner than its next reconnect.
    let now = app.now();
    if let Some(policy) = new_policy {
        let _ = app.hub.send(
            &id,
            ControllerCommand::UpdatePolicy { policy },
            app.epoch,
            now,
        );
    }
    if let Some(draining) = drain {
        let _ = app.hub.send(
            &id,
            ControllerCommand::SetDrain { draining },
            app.epoch,
            now,
        );
    }
    if retire {
        app.hub
            .close(&id, "the machine was retired by an administrator");
    }
    app.changed();
    info!(
        machine_id = %id,
        changed_by = %admin.id,
        policy_revision = view.policy.revision,
        drain = ?drain,
        retire,
        "machine updated"
    );
    Ok(Json(view))
}

/// `GET /queue`.
pub(crate) async fn queue(State(app): State<App>, _viewer: Viewer) -> ApiResult<Json<QueueView>> {
    Ok(Json(app.db(|ledger| ledger.queue_view()).await?))
}
