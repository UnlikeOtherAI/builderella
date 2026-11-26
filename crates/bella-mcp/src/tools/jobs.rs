//! `plan_job`, `submit_job`, `get_job`, `wait_job`, `cancel_job` and
//! `explain_queue`.

use super::render::{
    self, JOB_SUMMARY_REQUIRED, add_note, job_summary, job_summary_properties, message, name, tag,
};
use super::work::{self, Work};
use super::{
    Call, Failure, Output, ToolResult, count_arg, flag_arg, job_id_arg, job_id_schema, text_arg,
};
use crate::ops::{PlanArgs, SubmitArgs};
use crate::sanitize::inline;
use bella_core::CommandId;
use bella_core::api::{JobView, WaitQuery};
use bella_core::job::MachineBlocker;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

const MAX_PLACEMENTS: usize = 25;
const MAX_ISSUES: usize = 10;
const MAX_BLOCKERS: usize = 20;
const MAX_SUGGESTIONS: usize = 10;
const MAX_EXCLUDED: usize = 50;
const MAX_WARNINGS: usize = 10;
const MAX_STEPS: usize = 50;
const MAX_ATTEMPTS_SHOWN: usize = 10;

fn object_schema(properties: Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn output_schema(properties: Map<String, Value>, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

fn blocker(blocker: &MachineBlocker) -> Value {
    json!({
        "machine": name(&blocker.machine),
        "code": tag(&blocker.code),
        "message": message(&blocker.message),
        "permanent": blocker.permanent,
    })
}

fn blocker_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "machine": {"type": "string"},
            "code": {"type": "string"},
            "message": {"type": "string"},
            "permanent": {"type": "boolean", "description": "true when waiting cannot fix it."}
        },
        "required": ["machine", "code", "message", "permanent"]
    })
}

// ---------------------------------------------------------------- plan_job

pub(super) fn plan_input() -> Value {
    object_schema(work::properties(), &[])
}

pub(super) fn plan_output() -> Value {
    let properties = json!({
        "feasible": {"type": "boolean", "description": "At least one enrolled machine could ever run this."},
        "eligible_now": {"type": "integer", "description": "Machines that could start it right now."},
        "machines_considered": {"type": "integer"},
        "placements": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "machine": {"type": "string"},
                    "machine_id": {"type": "string"},
                    "eligible_now": {"type": "boolean"},
                    "blocker": blocker_schema()
                },
                "required": ["machine", "machine_id", "eligible_now"]
            }
        },
        "placements_omitted": {"type": "integer"},
        "issues": {"type": "array", "items": {"type": "string"}}
    });
    output_schema(
        as_map(properties),
        &[
            "feasible",
            "eligible_now",
            "machines_considered",
            "placements",
            "placements_omitted",
            "issues",
        ],
    )
}

pub(super) fn plan(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let Work {
        workspace,
        recipe,
        command,
        priority,
        placement,
        env,
        resources,
    } = work::parse(call, arguments)?;
    let response = call.fleet.plan(PlanArgs {
        workspace,
        recipe,
        command,
        priority,
        placement,
        env,
        resources,
    })?;

    let considered = response.placements.len();
    let eligible = response
        .placements
        .iter()
        .filter(|option| option.eligible_now)
        .count();
    // Machines that can start now are the useful ones; show them first.
    let mut ordered: Vec<_> = response.placements.iter().collect();
    ordered.sort_by_key(|option| !option.eligible_now);

    let mut lines = vec![format!(
        "feasible: {}; {eligible} of {considered} machine(s) could start it now",
        render::yes_no(response.feasible)
    )];
    let mut placements = Vec::new();
    for option in ordered.iter().take(MAX_PLACEMENTS) {
        let machine = name(&option.machine_name);
        let mut entry = Map::new();
        entry.insert("machine".into(), json!(machine));
        entry.insert("machine_id".into(), json!(option.machine_id.as_str()));
        entry.insert("eligible_now".into(), json!(option.eligible_now));
        match &option.blocker {
            Some(reason) => {
                lines.push(format!(
                    "- {machine}: blocked {} ({}): {}",
                    if reason.permanent {
                        "permanently"
                    } else {
                        "for now"
                    },
                    tag(&reason.code),
                    message(&reason.message)
                ));
                entry.insert("blocker".into(), blocker(reason));
            }
            None => lines.push(format!(
                "- {machine}: {}",
                if option.eligible_now {
                    "can start now"
                } else {
                    "not eligible right now"
                }
            )),
        }
        placements.push(Value::Object(entry));
    }
    let omitted = considered.saturating_sub(MAX_PLACEMENTS);
    if omitted > 0 {
        lines.push(format!("({omitted} more machine(s) not shown)"));
    }
    let issues: Vec<String> = response
        .issues
        .iter()
        .take(MAX_ISSUES)
        .map(|issue| message(issue))
        .collect();
    for issue in &issues {
        lines.push(format!("issue: {issue}"));
    }
    lines.push("Nothing was uploaded, queued or reserved.".into());

    let mut fields = Map::new();
    fields.insert("feasible".into(), json!(response.feasible));
    fields.insert("eligible_now".into(), json!(eligible));
    fields.insert("machines_considered".into(), json!(considered));
    fields.insert("placements".into(), Value::Array(placements));
    fields.insert("placements_omitted".into(), json!(omitted));
    fields.insert("issues".into(), json!(issues));
    Ok(Output::new(lines, fields))
}

// -------------------------------------------------------------- submit_job

pub(super) fn submit_input() -> Value {
    let mut properties = work::properties();
    properties.insert(
        "idempotency_key".into(),
        json!({
            "type": "string",
            "minLength": 8,
            "maxLength": 128,
            "description": "Optional. Repeating a submission of unchanged files with the same key returns the existing job instead of creating another, which makes a retry after a lost reply safe. Letters, digits, `.`, `_`, `:` and `-` only. A fresh key is generated for each call when omitted."
        }),
    );
    object_schema(properties, &[])
}

pub(super) fn submit_output() -> Value {
    let properties = json!({
        "job_id": {"type": "string"},
        "phase": {"type": "string"},
        "queue_reason": {
            "type": "object",
            "properties": {"code": {"type": "string"}, "message": {"type": "string"}},
            "required": ["code", "message"]
        },
        "snapshot_id": {"type": "string", "description": "The immutable snapshot that was captured for this job."},
        "consistency": {"type": "string", "description": "best_effort, filesystem_snapshot or coordinated."},
        "excluded_secrets": {"type": "array", "items": {"type": "string"}, "description": "Paths left out of the snapshot because they match a secret pattern."},
        "excluded_secrets_omitted": {"type": "integer"},
        "warnings": {"type": "array", "items": {"type": "string"}},
        "idempotency_key": {"type": "string"},
        "last_event": {"type": "integer"},
        "dashboard_url": {"type": "string"},
        "note": {"type": "string"}
    });
    output_schema(
        as_map(properties),
        &[
            "job_id",
            "phase",
            "snapshot_id",
            "consistency",
            "excluded_secrets",
            "excluded_secrets_omitted",
            "warnings",
            "idempotency_key",
            "last_event",
            "note",
        ],
    )
}

const SUBMIT_NOTE: &str = "The job exists but has not finished; this call did not wait for it. Use wait_job to follow it and read_logs for its output.";

pub(super) fn submit(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let idempotency_key = match text_arg(arguments, "idempotency_key") {
        Some(key) => {
            let valid = key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
            if !valid {
                return Err(Failure::Invalid(
                    "`idempotency_key` may contain only letters, digits, `.`, `_`, `:` and `-`"
                        .into(),
                ));
            }
            key.to_string()
        }
        None => new_idempotency_key(),
    };
    let Work {
        workspace,
        recipe,
        command,
        priority,
        placement,
        env,
        resources,
    } = work::parse(call, arguments)?;
    let outcome = call.fleet.submit(SubmitArgs {
        workspace,
        recipe,
        command,
        priority,
        placement,
        env,
        resources,
        idempotency_key: idempotency_key.clone(),
        session: call.config.session.clone(),
    })?;

    let job = &outcome.job;
    let id = job.id.as_str();
    let snapshot = outcome.snapshot_id.to_string();
    let consistency = tag(&outcome.consistency);
    let mut fields = Map::new();
    let mut lines = vec![format!("submitted job {id} (phase: {})", tag(&job.phase))];
    fields.insert("job_id".into(), json!(id));
    fields.insert("phase".into(), json!(tag(&job.phase)));
    if let Some(reason) = &job.queue_reason {
        fields.insert("queue_reason".into(), render::queue_reason(reason));
        lines.push(format!(
            "queue: {}: {}",
            tag(&reason.code),
            message(&reason.message)
        ));
    }
    fields.insert("snapshot_id".into(), json!(snapshot));
    fields.insert("consistency".into(), json!(consistency));
    lines.push(format!("snapshot: {snapshot} (consistency: {consistency})"));

    let excluded: Vec<String> = outcome
        .excluded_secrets
        .iter()
        .take(MAX_EXCLUDED)
        .map(|path| inline(path, 200))
        .collect();
    let excluded_omitted = outcome.excluded_secrets.len().saturating_sub(MAX_EXCLUDED);
    if !outcome.excluded_secrets.is_empty() {
        lines.push(format!(
            "left out as secrets ({}): {}{}",
            outcome.excluded_secrets.len(),
            excluded.join(", "),
            if excluded_omitted > 0 { ", ..." } else { "" }
        ));
    }
    fields.insert("excluded_secrets".into(), json!(excluded));
    fields.insert("excluded_secrets_omitted".into(), json!(excluded_omitted));

    let warnings: Vec<String> = outcome
        .warnings
        .iter()
        .take(MAX_WARNINGS)
        .map(|warning| message(warning))
        .collect();
    for warning in &warnings {
        lines.push(format!("warning: {warning}"));
    }
    fields.insert("warnings".into(), json!(warnings));

    fields.insert("idempotency_key".into(), json!(idempotency_key));
    lines.push(format!("idempotency_key: {idempotency_key}"));
    fields.insert("last_event".into(), json!(job.last_event));
    if let Some(link) = render::dashboard_link(call.config, id) {
        lines.push(format!("dashboard: {link}"));
        fields.insert("dashboard_url".into(), json!(link));
    }
    lines.push(SUBMIT_NOTE.into());
    fields.insert("note".into(), json!(SUBMIT_NOTE));
    Ok(Output::new(lines, fields))
}

/// A key that is unique per call: a time-ordered random identifier from
/// `bella-core`, re-labelled so it cannot be mistaken for an entity id.
fn new_idempotency_key() -> String {
    let id = CommandId::generate();
    let unique = id.as_str().rsplit('_').next().unwrap_or(id.as_str());
    format!("mcp-{unique}")
}

// ----------------------------------------------------------------- get_job

pub(super) fn get_input() -> Value {
    let properties = json!({
        "job_id": job_id_schema(),
        "verbose": {
            "type": "boolean",
            "description": "Also list the current attempt's steps and earlier attempts. Default false."
        }
    });
    object_schema(as_map(properties), &["job_id"])
}

fn job_output_schema(extra: Value, extra_required: &[&str]) -> Value {
    let mut properties = job_summary_properties();
    properties.extend(as_map(extra));
    let mut required: Vec<&str> = JOB_SUMMARY_REQUIRED.to_vec();
    required.extend_from_slice(extra_required);
    output_schema(properties, &required)
}

pub(super) fn get_output() -> Value {
    job_output_schema(
        json!({
            "steps": {
                "type": "array",
                "description": "With `verbose`: steps of the current attempt.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "kind": {"type": "string"},
                        "state": {"type": "string"},
                        "exit_code": {"type": "integer"}
                    },
                    "required": ["name", "kind", "state"]
                }
            },
            "attempt_history": {
                "type": "array",
                "description": "With `verbose`: the most recent attempts, oldest first.",
                "items": {
                    "type": "object",
                    "properties": {
                        "generation": {"type": "integer"},
                        "machine": {"type": "string"},
                        "phase": {"type": "string"},
                        "outcome": {"type": "string"},
                        "cleanup": {"type": "string"},
                        "uncertain": {"type": "boolean"}
                    },
                    "required": ["generation", "machine", "phase", "cleanup", "uncertain"]
                }
            }
        }),
        &[],
    )
}

pub(super) fn get(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = job_id_arg(arguments)?;
    let job = call.fleet.job(&id)?;
    let (mut fields, mut lines) = job_summary(&job, call.config);
    if flag_arg(arguments, "verbose") == Some(true) {
        add_detail(&job, &mut fields, &mut lines);
    }
    Ok(Output::new(lines, fields))
}

fn add_detail(job: &JobView, fields: &mut Map<String, Value>, lines: &mut Vec<String>) {
    if let Some(current) = job.current_attempt() {
        let mut steps = Vec::new();
        for step in current.steps.iter().take(MAX_STEPS) {
            let mut entry = Map::new();
            entry.insert("name".into(), json!(name(&step.name)));
            entry.insert("kind".into(), json!(tag(&step.kind)));
            entry.insert("state".into(), json!(tag(&step.state)));
            let mut line = format!(
                "step `{}` ({}): {}",
                name(&step.name),
                tag(&step.kind),
                tag(&step.state)
            );
            if let Some(code) = step.exit_code {
                entry.insert("exit_code".into(), json!(code));
                line.push_str(&format!(", exit code {code}"));
            }
            lines.push(line);
            steps.push(Value::Object(entry));
        }
        if current.steps.len() > MAX_STEPS {
            lines.push(format!(
                "({} more step(s) not shown)",
                current.steps.len() - MAX_STEPS
            ));
        }
        fields.insert("steps".into(), Value::Array(steps));
    }

    let mut attempts: Vec<_> = job.attempts.iter().collect();
    attempts.sort_by_key(|attempt| attempt.generation);
    let skipped = attempts.len().saturating_sub(MAX_ATTEMPTS_SHOWN);
    let mut history = Vec::new();
    for attempt in attempts.into_iter().skip(skipped) {
        let mut entry = Map::new();
        entry.insert("generation".into(), json!(attempt.generation));
        entry.insert("machine".into(), json!(name(&attempt.machine_name)));
        entry.insert("phase".into(), json!(tag(&attempt.phase)));
        let outcome = attempt.outcome.as_ref().map(outcome_type);
        if let Some(outcome) = &outcome {
            entry.insert("outcome".into(), json!(outcome));
        }
        entry.insert("cleanup".into(), json!(tag(&attempt.cleanup)));
        entry.insert("uncertain".into(), json!(attempt.uncertain));
        lines.push(format!(
            "attempt {} on {}: {}, outcome {}, cleanup {}{}",
            attempt.generation,
            name(&attempt.machine_name),
            tag(&attempt.phase),
            outcome.as_deref().unwrap_or("not known"),
            tag(&attempt.cleanup),
            if attempt.uncertain { ", uncertain" } else { "" }
        ));
        history.push(Value::Object(entry));
    }
    fields.insert("attempt_history".into(), Value::Array(history));
}

fn outcome_type(outcome: &bella_core::job::Outcome) -> String {
    serde_json::to_value(outcome)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

// ---------------------------------------------------------------- wait_job

pub(super) fn wait_input() -> Value {
    let properties = json!({
        "job_id": job_id_schema(),
        "timeout_seconds": {
            "type": "integer",
            "minimum": 0,
            "maximum": 86400,
            "description": "Longest time to block. Capped by the server (50 seconds by default); omit to wait up to that cap. 0 returns the current state at once."
        },
        "after_event": {
            "type": "integer",
            "minimum": 0,
            "description": "A `last_event` value from an earlier result: return as soon as the job has a newer event. Without it, return only when the outcome is known."
        }
    });
    object_schema(as_map(properties), &["job_id"])
}

pub(super) fn wait_output() -> Value {
    job_output_schema(
        json!({
            "timed_out": {
                "type": "boolean",
                "description": "true when the wait ended without what was waited for. The job was not cancelled."
            },
            "waited_seconds": {"type": "integer", "description": "The limit that applied to this wait."}
        }),
        &["timed_out", "waited_seconds"],
    )
}

pub(super) fn wait(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = job_id_arg(arguments)?;
    let cap = call.config.max_wait;
    let limit = match count_arg(arguments, "timeout_seconds") {
        Some(seconds) => Duration::from_secs(seconds).min(cap),
        None => cap,
    };
    let after_event = count_arg(arguments, "after_event");
    let deadline = Instant::now() + limit;

    // Long-poll in slices so that a withdrawn request stops promptly. A
    // slice that ends is only that: nothing here ever cancels the job.
    let response = loop {
        if call.abandoned() {
            return Err(Failure::Abandoned);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(call.config.wait_slice);
        let started = Instant::now();
        let response = call.fleet.wait(
            &id,
            WaitQuery {
                timeout_ms: Some(slice.as_millis().min(u128::from(u64::MAX)) as u64),
                after_event,
            },
        )?;
        if !response.timed_out || remaining <= slice {
            break response;
        }
        // An implementation that answers early must not turn this into a
        // busy loop.
        let unused = slice.saturating_sub(started.elapsed());
        if unused > Duration::from_millis(5) {
            std::thread::sleep(unused.min(Duration::from_millis(50)));
        }
    };
    if call.abandoned() {
        return Err(Failure::Abandoned);
    }

    let (mut fields, mut lines) = job_summary(&response.job, call.config);
    fields.insert("timed_out".into(), json!(response.timed_out));
    fields.insert("waited_seconds".into(), json!(limit.as_secs()));
    if response.timed_out {
        lines.insert(
            1,
            format!("timed_out: yes (waited up to {} s)", limit.as_secs()),
        );
        add_note(
            &mut fields,
            &mut lines,
            &format!(
                "The wait timed out; the job was NOT cancelled and keeps running. Call wait_job again (with after_event {} to wake at the next state change).",
                response.job.last_event
            ),
        );
    } else {
        lines.insert(1, "timed_out: no".into());
        if response.job.outcome.is_some() && !response.job.is_settled() {
            add_note(
                &mut fields,
                &mut lines,
                "The outcome is known, but cleanup has not finished, so the job's resources are not released yet.",
            );
        }
    }
    Ok(Output::new(lines, fields))
}

// -------------------------------------------------------------- cancel_job

pub(super) fn cancel_input() -> Value {
    object_schema(as_map(json!({"job_id": job_id_schema()})), &["job_id"])
}

pub(super) fn cancel_output() -> Value {
    job_output_schema(
        json!({
            "cancel_accepted": {
                "type": "boolean",
                "description": "The fleet accepted the request. It does not mean the job has stopped."
            }
        }),
        &["cancel_accepted"],
    )
}

pub(super) fn cancel(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = job_id_arg(arguments)?;
    let job = call.fleet.cancel(&id)?;
    let (mut fields, mut lines) = job_summary(&job, call.config);
    fields.insert("cancel_accepted".into(), json!(true));
    lines.insert(1, "cancel request accepted".into());
    let note = if job.is_settled() {
        "The job is finished and its resources are released."
    } else {
        "Cancellation was requested, not completed: processes may still be running and the job's resources stay reserved until cleanup is confirmed. Do not assume the capacity is free. Follow it with wait_job or get_job until resources_released is true."
    };
    add_note(&mut fields, &mut lines, note);
    Ok(Output::new(lines, fields))
}

// ----------------------------------------------------------- explain_queue

pub(super) fn explain_input() -> Value {
    object_schema(as_map(json!({"job_id": job_id_schema()})), &["job_id"])
}

pub(super) fn explain_output() -> Value {
    let properties = json!({
        "job_id": {"type": "string"},
        "phase": {"type": "string"},
        "reason": {
            "type": "object",
            "properties": {"code": {"type": "string"}, "message": {"type": "string"}},
            "required": ["code", "message"]
        },
        "position": {"type": "integer", "description": "Position among queued jobs, starting at 1."},
        "hard_blockers": {
            "type": "array",
            "description": "Machines that can never run the job as specified.",
            "items": blocker_schema()
        },
        "temporary_blockers": {
            "type": "array",
            "description": "Machines that cannot take the job right now.",
            "items": blocker_schema()
        },
        "blockers_omitted": {"type": "integer"},
        "suggestions": {"type": "array", "items": {"type": "string"}},
        "note": {"type": "string"}
    });
    output_schema(
        as_map(properties),
        &[
            "job_id",
            "phase",
            "reason",
            "hard_blockers",
            "temporary_blockers",
            "blockers_omitted",
            "suggestions",
            "note",
        ],
    )
}

const EXPLAIN_NOTE: &str = "Suggestions are not applied automatically. Waiting needs no action; anything that changes policy or capacity is the user's decision.";

pub(super) fn explain(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = job_id_arg(arguments)?;
    let response = call.fleet.explain(&id)?;
    let reason = &response.reason;

    let mut fields = Map::new();
    let mut lines = vec![format!(
        "job {}: phase {}",
        response.job_id,
        tag(&response.phase)
    )];
    fields.insert("job_id".into(), json!(response.job_id.as_str()));
    fields.insert("phase".into(), json!(tag(&response.phase)));
    fields.insert("reason".into(), render::queue_reason(reason));
    lines.push(format!(
        "reason: {}: {}",
        tag(&reason.code),
        message(&reason.message)
    ));
    if let Some(position) = response.position {
        fields.insert("position".into(), json!(position));
        lines.push(format!("position in queue: {position}"));
    }

    let mut hard = Vec::new();
    let mut temporary = Vec::new();
    let mut omitted = 0usize;
    for entry in &reason.blockers {
        let list = if entry.permanent {
            &mut hard
        } else {
            &mut temporary
        };
        if list.len() < MAX_BLOCKERS {
            list.push(entry);
        } else {
            omitted += 1;
        }
    }
    for (label, list) in [
        ("cannot run it as specified", &hard),
        ("blocked for now", &temporary),
    ] {
        for entry in list {
            lines.push(format!(
                "- {} {label} ({}): {}",
                name(&entry.machine),
                tag(&entry.code),
                message(&entry.message)
            ));
        }
    }
    if omitted > 0 {
        lines.push(format!("({omitted} more blocker(s) not shown)"));
    }
    fields.insert(
        "hard_blockers".into(),
        Value::Array(hard.iter().map(|entry| blocker(entry)).collect()),
    );
    fields.insert(
        "temporary_blockers".into(),
        Value::Array(temporary.iter().map(|entry| blocker(entry)).collect()),
    );
    fields.insert("blockers_omitted".into(), json!(omitted));

    let suggestions: Vec<String> = response
        .suggestions
        .iter()
        .take(MAX_SUGGESTIONS)
        .map(|suggestion| message(suggestion))
        .collect();
    for suggestion in &suggestions {
        lines.push(format!("suggestion: {suggestion}"));
    }
    fields.insert("suggestions".into(), json!(suggestions));
    lines.push(format!("note: {EXPLAIN_NOTE}"));
    fields.insert("note".into(), json!(EXPLAIN_NOTE));
    Ok(Output::new(lines, fields))
}

fn as_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_unique_and_header_safe() {
        let first = new_idempotency_key();
        let second = new_idempotency_key();
        assert_ne!(first, second);
        assert!(first.starts_with("mcp-") && first.len() == 36, "{first}");
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
