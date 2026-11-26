//! `list_fleet`: a compact view of every machine.

use super::render::{bytes, cpus, name, tag, yes_no};
use super::{Call, Output, ToolResult};
use bella_core::api::MachineView;
use serde_json::{Map, Value, json};

/// Most machines listed in one result.
const MAX_MACHINES: usize = 40;
const MAX_FLAGS: usize = 12;
const MAX_TOKENS: usize = 8;

pub(super) fn input() -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": {}})
}

pub(super) fn output() -> Value {
    let amounts = |unit: &str| {
        json!({
            "type": "object",
            "description": format!("{unit}. `free` is what managed work could still be given right now."),
            "required": ["ceiling", "reserved", "free"],
            "properties": {
                "ceiling": {"type": "number", "description": "Upper bound for managed work on this machine."},
                "reserved": {"type": "number", "description": "Held by reservations, including uncertain and quarantined ones."},
                "free": {"type": "number"}
            }
        })
    };
    json!({
        "type": "object",
        "required": ["controller_version", "reconciling", "queued", "running", "environments", "machine_count", "machines", "machines_omitted"],
        "properties": {
            "controller_version": {"type": "string"},
            "reconciling": {"type": "boolean", "description": "The controller is reconciling after a start and is not dispatching yet."},
            "queued": {"type": "integer"},
            "running": {"type": "integer"},
            "environments": {"type": "integer"},
            "machine_count": {"type": "integer"},
            "machines": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "id", "platform", "os_version", "profile", "lifecycle", "connected", "accepts_builds", "flags", "cpus", "memory_bytes", "enforcement", "active_attempts"],
                    "properties": {
                        "name": {"type": "string"},
                        "id": {"type": "string"},
                        "platform": {"type": "string", "description": "For example `macos-aarch64`."},
                        "os_version": {"type": "string"},
                        "profile": {"type": "string"},
                        "lifecycle": {"type": "string"},
                        "connected": {"type": "boolean"},
                        "accepts_builds": {"type": "boolean"},
                        "flags": {"type": "array", "items": {"type": "string"}, "description": "Conditions that currently limit the machine, such as draining, on_battery or stale."},
                        "cpus": amounts("CPUs"),
                        "memory_bytes": amounts("Bytes of memory"),
                        "tokens_free": {"type": "object", "description": "Free countable resources such as simulators, by name."},
                        "enforcement": {
                            "type": "object",
                            "description": "How strongly limits are enforced here: none, advisory, bounded_guest or hard.",
                            "required": ["cpu", "memory", "process_tree", "verified"],
                            "properties": {
                                "cpu": {"type": "string"},
                                "memory": {"type": "string"},
                                "process_tree": {"type": "string"},
                                "verified": {"type": "boolean", "description": "The controls were exercised by a self-test, not just detected."}
                            }
                        },
                        "active_attempts": {"type": "integer"}
                    }
                }
            },
            "machines_omitted": {"type": "integer"}
        }
    })
}

fn machine(view: &MachineView) -> (Value, String) {
    let label = name(&view.name);
    let platform = view.inventory.os.platform.to_string();
    let capacity = match &view.effective_budget {
        Some(budget) => view.managed_ceiling.min(budget),
        None => view.managed_ceiling.clone(),
    };
    let free = capacity.saturating_sub(&view.reserved);
    let flags: Vec<String> = view.flags.iter().take(MAX_FLAGS).map(tag).collect();
    let enforcement = &view.enforcement;

    let mut entry = Map::new();
    entry.insert("name".into(), json!(label));
    entry.insert("id".into(), json!(view.id.as_str()));
    entry.insert("platform".into(), json!(platform));
    entry.insert(
        "os_version".into(),
        json!(name(&view.inventory.os.os_version)),
    );
    entry.insert("profile".into(), json!(tag(&view.policy.profile)));
    entry.insert("lifecycle".into(), json!(tag(&view.lifecycle)));
    entry.insert("connected".into(), json!(view.connected));
    entry.insert("accepts_builds".into(), json!(view.policy.accept_builds));
    entry.insert("flags".into(), json!(flags));
    entry.insert(
        "cpus".into(),
        json!({
            "ceiling": cpus(view.managed_ceiling.cpu_milli),
            "reserved": cpus(view.reserved.cpu_milli),
            "free": cpus(free.cpu_milli),
        }),
    );
    entry.insert(
        "memory_bytes".into(),
        json!({
            "ceiling": view.managed_ceiling.memory_bytes,
            "reserved": view.reserved.memory_bytes,
            "free": free.memory_bytes,
        }),
    );
    if !free.tokens.is_empty() {
        let tokens: Map<String, Value> = free
            .tokens
            .iter()
            .take(MAX_TOKENS)
            .map(|(token, count)| (name(token), json!(count)))
            .collect();
        entry.insert("tokens_free".into(), Value::Object(tokens));
    }
    entry.insert(
        "enforcement".into(),
        json!({
            "cpu": tag(&enforcement.cpu),
            "memory": tag(&enforcement.memory),
            "process_tree": tag(&enforcement.process_tree),
            "verified": enforcement.verified,
        }),
    );
    entry.insert("active_attempts".into(), json!(view.attempts.len()));

    let line = format!(
        "- {label} [{platform} {}] {}{}, {}; cpu free {:.1} of {:.1} ({:.1} reserved); memory free {} of {} ({} reserved); enforcement cpu={} memory={} tree={}{}; flags: {}; active attempts: {}",
        name(&view.inventory.os.os_version),
        tag(&view.lifecycle),
        if view.connected { ", connected" } else { ", NOT connected" },
        if view.policy.accept_builds { "accepts builds" } else { "does not accept builds" },
        cpus(free.cpu_milli),
        cpus(view.managed_ceiling.cpu_milli),
        cpus(view.reserved.cpu_milli),
        bytes(free.memory_bytes),
        bytes(view.managed_ceiling.memory_bytes),
        bytes(view.reserved.memory_bytes),
        tag(&enforcement.cpu),
        tag(&enforcement.memory),
        tag(&enforcement.process_tree),
        if enforcement.verified { " (verified)" } else { " (not verified)" },
        if flags.is_empty() { "none".to_string() } else { flags.join(", ") },
        view.attempts.len(),
    );
    (Value::Object(entry), line)
}

pub(super) fn list(call: &Call<'_>, _arguments: &Map<String, Value>) -> ToolResult {
    let overview = call.fleet.fleet()?;
    let count = overview.machines.len();
    let mut lines = vec![format!(
        "fleet: {count} machine(s); queued {}, running {}, environments {}; controller {}; reconciling: {}",
        overview.queued,
        overview.running,
        overview.environments,
        name(&overview.controller_version),
        yes_no(overview.reconciling)
    )];
    let mut machines = Vec::new();
    for view in overview.machines.iter().take(MAX_MACHINES) {
        let (entry, line) = machine(view);
        machines.push(entry);
        lines.push(line);
    }
    let omitted = count.saturating_sub(MAX_MACHINES);
    if omitted > 0 {
        lines.push(format!("({omitted} more machine(s) not shown)"));
    }
    if overview.reconciling {
        lines.push(
            "The controller is reconciling after a restart and is not starting new work yet."
                .into(),
        );
    }

    let mut fields = Map::new();
    fields.insert(
        "controller_version".into(),
        json!(name(&overview.controller_version)),
    );
    fields.insert("reconciling".into(), json!(overview.reconciling));
    fields.insert("queued".into(), json!(overview.queued));
    fields.insert("running".into(), json!(overview.running));
    fields.insert("environments".into(), json!(overview.environments));
    fields.insert("machine_count".into(), json!(count));
    fields.insert("machines".into(), Value::Array(machines));
    fields.insert("machines_omitted".into(), json!(omitted));
    Ok(Output::new(lines, fields))
}
