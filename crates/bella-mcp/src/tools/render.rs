//! Shared formatting for tool results: enum names, sizes, timestamps and the
//! compact job summary several tools return.

use super::Config;
use crate::sanitize::inline;
use bella_core::UnixMillis;
use bella_core::api::{AttemptView, JobView};
use bella_core::job::{Outcome, QueueReason};
use bella_core::protocol::StepState;
use serde::Serialize;
use serde_json::{Map, Value, json};

/// Longest controller message or detail string passed on to the agent.
pub(crate) const MAX_MESSAGE_CHARS: usize = 400;
/// Longest machine, recipe, project or step name passed on to the agent.
pub(crate) const MAX_NAME_CHARS: usize = 96;

/// The wire name of a unit enum variant, e.g. `waiting_for_capacity`.
pub(crate) fn tag<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => name,
        _ => "unknown".to_string(),
    }
}

pub(crate) fn name(text: &str) -> String {
    inline(text, MAX_NAME_CHARS)
}

pub(crate) fn message(text: &str) -> String {
    inline(text, MAX_MESSAGE_CHARS)
}

/// `12.0 GiB`, `512 MiB`, `17 B`.
pub(crate) fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}

/// Milli-CPUs as a count of CPUs.
pub(crate) fn cpus(milli: u64) -> f64 {
    milli as f64 / 1000.0
}

/// UTC timestamp in RFC 3339 form, to the second.
pub(crate) fn rfc3339(at: UnixMillis) -> String {
    let seconds = at.0 / 1000;
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let in_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Link to a job in the dashboard. A `{job}` placeholder in the configured
/// URL is replaced; otherwise `/jobs/<id>` is appended.
pub(crate) fn dashboard_link(config: &Config, job_id: &str) -> Option<String> {
    let base = config.dashboard_url.as_deref()?.trim();
    if base.is_empty() {
        return None;
    }
    Some(if base.contains("{job}") {
        base.replace("{job}", job_id)
    } else {
        format!("{}/jobs/{job_id}", base.trim_end_matches('/'))
    })
}

/// `{code, message}` for a queue reason.
pub(crate) fn queue_reason(reason: &QueueReason) -> Value {
    json!({"code": tag(&reason.code), "message": message(&reason.message)})
}

/// The outcome as a small object and as a phrase.
fn outcome(outcome: &Outcome) -> (Value, String) {
    match outcome {
        Outcome::Succeeded => (json!({"type": "succeeded"}), "succeeded".into()),
        Outcome::Failed { step, exit_code } => {
            let mut object = Map::new();
            object.insert("type".into(), json!("failed"));
            let mut phrase = String::from("failed");
            if let Some(step) = step {
                object.insert("step".into(), json!(name(step)));
                phrase.push_str(&format!(" at step `{}`", name(step)));
            }
            if let Some(code) = exit_code {
                object.insert("exit_code".into(), json!(code));
                phrase.push_str(&format!(" with exit code {code}"));
            }
            (Value::Object(object), phrase)
        }
        Outcome::Cancelled => (json!({"type": "cancelled"}), "cancelled".into()),
        Outcome::TimedOut { step } => {
            let mut object = Map::new();
            object.insert("type".into(), json!("timed_out"));
            let mut phrase = String::from("timed out");
            if let Some(step) = step {
                object.insert("step".into(), json!(name(step)));
                phrase.push_str(&format!(" at step `{}`", name(step)));
            }
            (Value::Object(object), phrase)
        }
        Outcome::ResourceExhausted { detail } => (
            json!({"type": "resource_exhausted", "detail": message(detail)}),
            format!("hit a resource limit: {}", message(detail)),
        ),
        Outcome::InfraInterrupted { detail } => (
            json!({"type": "infra_interrupted", "detail": message(detail)}),
            format!(
                "interrupted by an infrastructure failure: {}",
                message(detail)
            ),
        ),
        Outcome::Rejected { detail } => (
            json!({"type": "rejected", "detail": message(detail)}),
            format!("rejected: {}", message(detail)),
        ),
    }
}

/// The step that failed and its exit code, from the outcome when it names
/// them, otherwise from the current attempt's step states.
fn failing_step(job: &JobView, current: Option<&AttemptView>) -> (Option<String>, Option<i32>) {
    let from_outcome = match &job.outcome {
        Some(Outcome::Failed { step, exit_code }) => (step.clone(), *exit_code),
        Some(Outcome::TimedOut { step }) => (step.clone(), None),
        _ => (None, None),
    };
    if from_outcome.0.is_some() {
        return from_outcome;
    }
    let failed = current.and_then(|attempt| {
        attempt
            .steps
            .iter()
            .find(|step| matches!(step.state, StepState::Failed | StepState::TimedOut))
    });
    match failed {
        Some(step) => (Some(step.name.clone()), from_outcome.1.or(step.exit_code)),
        None => from_outcome,
    }
}

pub(crate) const INFRA_NOTE: &str = "This is an infrastructure failure, not a result of the code under test. Report it to the user; do not fall back to running an unbounded build or test locally.";
pub(crate) const UNCERTAIN_NOTE: &str = "The worker is out of contact, so the attempt may still be running and its resources stay reserved.";

/// The concise view of a job: structured fields and the matching text lines.
/// It reports the snapshot the job ran on and never claims that the
/// workspace still matches it.
pub(crate) fn job_summary(job: &JobView, config: &Config) -> (Map<String, Value>, Vec<String>) {
    let mut fields = Map::new();
    let mut lines = Vec::new();
    let id = job.id.as_str();
    let current = job.current_attempt();

    fields.insert("job_id".into(), json!(id));
    fields.insert("recipe".into(), json!(name(&job.recipe)));
    fields.insert("project".into(), json!(name(&job.project_name)));
    fields.insert("priority".into(), json!(tag(&job.priority)));
    lines.push(format!(
        "job {id}: recipe `{}`, project `{}`, priority {}",
        name(&job.recipe),
        name(&job.project_name),
        tag(&job.priority)
    ));

    let phase = tag(&job.phase);
    fields.insert("phase".into(), json!(phase));
    fields.insert("settled".into(), json!(job.is_settled()));
    let mut phase_line = format!("phase: {phase}");
    if let Some(attempt) = current {
        let machine = name(&attempt.machine_name);
        fields.insert("machine".into(), json!(machine));
        phase_line.push_str(&format!(" on {machine}"));
    }
    fields.insert("attempts".into(), json!(job.attempts.len()));
    phase_line.push_str(&format!(" (attempts: {})", job.attempts.len()));
    lines.push(phase_line);

    let mut notes: Vec<&str> = Vec::new();
    match &job.outcome {
        Some(result) => {
            let (object, phrase) = outcome(result);
            fields.insert("outcome".into(), object);
            lines.push(format!("outcome: {phrase}"));
            if matches!(result, Outcome::InfraInterrupted { .. }) {
                fields.insert("infrastructure_failure".into(), json!(true));
                notes.push(INFRA_NOTE);
            }
        }
        None => lines.push("outcome: not known yet".into()),
    }
    let (step, exit_code) = failing_step(job, current);
    if let Some(step) = &step {
        fields.insert("failing_step".into(), json!(name(step)));
    }
    if let Some(code) = exit_code {
        fields.insert("exit_code".into(), json!(code));
    }
    let outcome_names_step = matches!(
        &job.outcome,
        Some(Outcome::Failed { step: Some(_), .. } | Outcome::TimedOut { step: Some(_) })
    );
    if !outcome_names_step && let Some(step) = &step {
        let code = exit_code.map_or(String::new(), |code| format!(" (exit code {code})"));
        lines.push(format!("failing step: `{}`{code}", name(step)));
    }

    let uncertain = current.is_some_and(|attempt| attempt.uncertain);
    fields.insert("uncertain".into(), json!(uncertain));
    if uncertain {
        notes.push(UNCERTAIN_NOTE);
    }

    let cleanup = tag(&job.cleanup);
    fields.insert("cleanup".into(), json!(cleanup));
    fields.insert("resources_released".into(), json!(job.is_settled()));
    fields.insert("cancel_requested".into(), json!(job.cancel_requested));
    lines.push(format!(
        "cleanup: {cleanup}; resources released: {}; cancel requested: {}",
        yes_no(job.is_settled()),
        yes_no(job.cancel_requested)
    ));

    if let Some(reason) = &job.queue_reason {
        fields.insert("queue_reason".into(), queue_reason(reason));
        lines.push(format!(
            "queue: {}: {}",
            tag(&reason.code),
            message(&reason.message)
        ));
    }

    let snapshot = job.snapshot_id.to_string();
    fields.insert("snapshot_id".into(), json!(snapshot));
    fields.insert("workspace_compared".into(), json!(false));
    lines.push(format!(
        "snapshot: {snapshot} (the result applies to this snapshot; it was not compared with the current workspace)"
    ));

    if let Some(environment) = &job.environment_id {
        fields.insert("environment_id".into(), json!(environment.as_str()));
        lines.push(format!("environment: {environment}"));
    }
    fields.insert("last_event".into(), json!(job.last_event));
    lines.push(format!("last_event: {}", job.last_event));
    if let Some(link) = dashboard_link(config, id) {
        lines.push(format!("dashboard: {link}"));
        fields.insert("dashboard_url".into(), json!(link));
    }
    if !notes.is_empty() {
        let note = notes.join(" ");
        lines.push(format!("note: {note}"));
        fields.insert("note".into(), json!(note));
    }
    (fields, lines)
}

/// Add to the `note` field and the text lines.
pub(crate) fn add_note(fields: &mut Map<String, Value>, lines: &mut Vec<String>, note: &str) {
    let combined = match fields.get("note").and_then(Value::as_str) {
        Some(existing) => {
            // Replace the line written for the existing note.
            lines.retain(|line| !line.starts_with("note: "));
            format!("{note} {existing}")
        }
        None => note.to_string(),
    };
    lines.push(format!("note: {combined}"));
    fields.insert("note".into(), json!(combined));
}

pub(crate) fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// JSON Schema properties of [`job_summary`], for output schemas.
pub(crate) fn job_summary_properties() -> Map<String, Value> {
    let properties = json!({
        "job_id": {"type": "string"},
        "recipe": {"type": "string"},
        "project": {"type": "string"},
        "priority": {"type": "string"},
        "phase": {"type": "string", "description": "capturing, queued, offered, preparing, executing, collecting, cleaning, finished or quarantined."},
        "settled": {"type": "boolean", "description": "Finished, with its resources released."},
        "machine": {"type": "string", "description": "Machine of the current attempt."},
        "attempts": {"type": "integer"},
        "outcome": {
            "type": "object",
            "description": "Present once the result is known.",
            "properties": {
                "type": {"type": "string", "description": "succeeded, failed, cancelled, timed_out, resource_exhausted, infra_interrupted or rejected."},
                "step": {"type": "string"},
                "exit_code": {"type": "integer"},
                "detail": {"type": "string"}
            },
            "required": ["type"]
        },
        "infrastructure_failure": {"type": "boolean"},
        "failing_step": {"type": "string"},
        "exit_code": {"type": "integer"},
        "uncertain": {"type": "boolean", "description": "The worker is out of contact; the attempt may still be running."},
        "cleanup": {"type": "string", "description": "not_started, requested, worker_acknowledged, processes_stopped, cleanup_complete, released or quarantined."},
        "resources_released": {"type": "boolean"},
        "cancel_requested": {"type": "boolean"},
        "queue_reason": {
            "type": "object",
            "properties": {"code": {"type": "string"}, "message": {"type": "string"}},
            "required": ["code", "message"]
        },
        "snapshot_id": {"type": "string", "description": "The immutable snapshot the job ran on."},
        "workspace_compared": {"type": "boolean", "description": "Always false: the bridge does not compare the snapshot with the current workspace."},
        "environment_id": {"type": "string"},
        "last_event": {"type": "integer", "description": "Cursor for wait_job's after_event."},
        "dashboard_url": {"type": "string"},
        "note": {"type": "string"}
    });
    match properties {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Fields of [`job_summary`] that are always present.
pub(crate) const JOB_SUMMARY_REQUIRED: [&str; 14] = [
    "job_id",
    "recipe",
    "project",
    "priority",
    "phase",
    "settled",
    "attempts",
    "uncertain",
    "cleanup",
    "resources_released",
    "cancel_requested",
    "snapshot_id",
    "workspace_compared",
    "last_event",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_rendered_in_utc() {
        assert_eq!(rfc3339(UnixMillis(0)), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UnixMillis(1_700_000_000_000)),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(rfc3339(UnixMillis(951_782_400_000)), "2000-02-29T00:00:00Z");
        assert_eq!(
            rfc3339(UnixMillis(1_735_689_599_999)),
            "2024-12-31T23:59:59Z"
        );
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(bytes(17), "17 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(12 * 1024 * 1024 * 1024), "12.0 GiB");
        assert_eq!(bytes(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn enum_names_match_the_wire() {
        assert_eq!(tag(&bella_core::job::JobPhase::Executing), "executing");
        assert_eq!(
            tag(&bella_core::job::QueueReasonCode::WaitingForCapacity),
            "waiting_for_capacity"
        );
    }
}
