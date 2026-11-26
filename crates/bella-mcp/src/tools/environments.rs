//! `get_environment`, `renew_environment` and `finish_environment`.

use super::render::{bytes, cpus, message, name, rfc3339, tag};
use super::{Call, Output, ToolResult, environment_id_arg};
use bella_core::api::{EnvironmentState, EnvironmentView};
use serde_json::{Map, Value, json};

const MAX_ENDPOINTS: usize = 32;

pub(super) fn input() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["environment_id"],
        "properties": {
            "environment_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": "Environment id as reported by get_job, for example `env_0190f3a2b4c57d8e9f00112233445566`."
            }
        }
    })
}

pub(super) fn output() -> Value {
    json!({
        "type": "object",
        "required": [
            "environment_id", "job_id", "state", "machine", "snapshot_id", "endpoints",
            "effective_deadline", "effective_deadline_ms", "lease_until", "hard_deadline",
            "idle_timeout_seconds", "reserved", "resources_released"
        ],
        "properties": {
            "environment_id": {"type": "string"},
            "job_id": {"type": "string"},
            "state": {"type": "string", "description": "starting, ready, finishing, ended or quarantined."},
            "machine": {"type": "string"},
            "snapshot_id": {"type": "string", "description": "The snapshot being served. Later edits never change it."},
            "endpoints": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["step", "host", "port"],
                    "properties": {
                        "step": {"type": "string"},
                        "host": {"type": "string"},
                        "port": {"type": "integer"}
                    }
                }
            },
            "effective_deadline": {"type": "string", "description": "UTC time at which the environment ends if nothing else happens."},
            "effective_deadline_ms": {"type": "integer", "description": "The same moment in milliseconds since the Unix epoch."},
            "lease_until": {"type": "string", "description": "End of the current renewable lease, UTC."},
            "hard_deadline": {"type": "string", "description": "UTC time no renewal can pass."},
            "idle_timeout_seconds": {"type": "integer"},
            "reserved": {
                "type": "object",
                "description": "Resources held on the machine while the environment lives.",
                "required": ["cpus", "memory_bytes"],
                "properties": {"cpus": {"type": "number"}, "memory_bytes": {"type": "integer"}}
            },
            "resources_released": {"type": "boolean"},
            "ended_reason": {"type": "string"},
            "note": {"type": "string"}
        }
    })
}

fn render(view: &EnvironmentView, headline: &str, note: Option<&str>) -> Output {
    let state = tag(&view.state);
    let machine = name(&view.machine_name);
    let released = view.state == EnvironmentState::Ended;
    let deadline = rfc3339(view.effective_deadline);
    let lease_until = rfc3339(view.lease.lease_until);
    let hard_deadline = rfc3339(view.lease.hard_deadline());

    let mut fields = Map::new();
    let mut lines = vec![
        headline.to_string(),
        format!(
            "environment {}: {state} on {machine} (job {})",
            view.id, view.job_id
        ),
    ];
    fields.insert("environment_id".into(), json!(view.id.as_str()));
    fields.insert("job_id".into(), json!(view.job_id.as_str()));
    fields.insert("state".into(), json!(state));
    fields.insert("machine".into(), json!(machine));
    fields.insert("snapshot_id".into(), json!(view.snapshot_id.to_string()));
    lines.push(format!("snapshot served: {}", view.snapshot_id));

    let mut endpoints = Vec::new();
    for endpoint in view.endpoints.iter().take(MAX_ENDPOINTS) {
        let step = name(&endpoint.step);
        let host = name(&endpoint.host);
        lines.push(format!("endpoint `{step}`: {host}:{}", endpoint.port));
        endpoints.push(json!({"step": step, "host": host, "port": endpoint.port}));
    }
    if view.endpoints.len() > MAX_ENDPOINTS {
        lines.push(format!(
            "({} more endpoint(s) not shown)",
            view.endpoints.len() - MAX_ENDPOINTS
        ));
    }
    fields.insert("endpoints".into(), Value::Array(endpoints));

    lines.push(format!(
        "ends at {deadline} unless renewed or used (lease until {lease_until}, idle timeout {} s, hard limit {hard_deadline})",
        view.lease.spec.idle_timeout_secs
    ));
    fields.insert("effective_deadline".into(), json!(deadline));
    fields.insert(
        "effective_deadline_ms".into(),
        json!(view.effective_deadline.0),
    );
    fields.insert("lease_until".into(), json!(lease_until));
    fields.insert("hard_deadline".into(), json!(hard_deadline));
    fields.insert(
        "idle_timeout_seconds".into(),
        json!(view.lease.spec.idle_timeout_secs),
    );

    lines.push(format!(
        "reserved: {:.1} CPU, {} memory; resources released: {}",
        cpus(view.reserved.cpu_milli),
        bytes(view.reserved.memory_bytes),
        super::render::yes_no(released)
    ));
    fields.insert(
        "reserved".into(),
        json!({
            "cpus": cpus(view.reserved.cpu_milli),
            "memory_bytes": view.reserved.memory_bytes,
        }),
    );
    fields.insert("resources_released".into(), json!(released));
    if let Some(reason) = &view.ended_reason {
        lines.push(format!("ended: {}", message(reason)));
        fields.insert("ended_reason".into(), json!(message(reason)));
    }
    if let Some(note) = note {
        lines.push(format!("note: {note}"));
        fields.insert("note".into(), json!(note));
    }
    Output::new(lines, fields)
}

pub(super) fn get(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = environment_id_arg(arguments)?;
    let view = call.fleet.environment(&id)?;
    let note = match view.state {
        EnvironmentState::Quarantined => Some(
            "Cleanup of this environment could not be verified, so its resources stay held. Tell the user.",
        ),
        EnvironmentState::Finishing => {
            Some("Cleanup is underway; the resources are not released yet.")
        }
        _ => None,
    };
    Ok(render(&view, "environment state", note))
}

pub(super) fn renew(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = environment_id_arg(arguments)?;
    let view = call.fleet.renew_environment(&id)?;
    Ok(render(
        &view,
        "lease renewed",
        Some(
            "The deadline shown is the one that now applies. It cannot pass the hard limit, however often the lease is renewed.",
        ),
    ))
}

pub(super) fn finish(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = environment_id_arg(arguments)?;
    let view = call.fleet.finish_environment(&id)?;
    let note = if view.state == EnvironmentState::Ended {
        "The environment has ended and its resources are released."
    } else {
        "Finishing was requested, not completed: the environment's resources stay reserved until cleanup is confirmed. Check get_environment before assuming the capacity is free."
    };
    Ok(render(&view, "finish requested", Some(note)))
}
