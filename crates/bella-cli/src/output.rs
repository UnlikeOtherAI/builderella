//! Printing: one JSON document with `--json`, plain text otherwise.

use crate::error::{CliError, wire_name};
use bella_core::UnixMillis;
use bella_core::api::JobView;
use bella_core::job::Outcome;
use bella_recipe::units::format_cpus;
use serde::Serialize;
use std::io::Write;

/// Where results go. Text and JSON go to standard output; diagnostics never
/// do.
#[derive(Clone, Copy, Debug)]
pub struct Out {
    pub json: bool,
}

impl Out {
    /// Print `value` as JSON, or `text` when JSON was not asked for.
    pub fn emit<T: Serialize>(&self, value: &T, text: impl FnOnce() -> String) {
        if self.json {
            self.print_json(value);
        } else {
            let text = text();
            if !text.is_empty() {
                print_line(&text);
            }
        }
    }

    pub fn print_json<T: Serialize>(&self, value: &T) {
        match serde_json::to_string_pretty(value) {
            Ok(text) => print_line(&text),
            Err(error) => eprintln!("bella: could not encode the result as JSON: {error}"),
        }
    }

    /// A line of progress or advice: on standard error, so it never mixes
    /// with a JSON document.
    pub fn note(&self, text: impl AsRef<str>) {
        eprintln!("{}", text.as_ref());
    }

    /// Report a failure the way the output mode asks for.
    pub fn error(&self, error: &CliError) {
        eprintln!("bella: {}", error.message);
        if self.json {
            self.print_json(&error.to_json());
        }
    }
}

/// Write one line to standard output, ignoring a closed pipe.
pub fn print_line(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{text}");
    let _ = stdout.flush();
}

/// Write text without a newline (log output that carries its own).
pub fn print_raw(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// `succeeded`, `failed in step build (exit 2)`, ...
pub fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Succeeded => "succeeded".into(),
        Outcome::Failed { step, exit_code } => {
            let mut text = "failed".to_string();
            if let Some(step) = step {
                text.push_str(&format!(" in step {step}"));
            }
            if let Some(code) = exit_code {
                text.push_str(&format!(" (exit {code})"));
            }
            text
        }
        Outcome::Cancelled => "cancelled".into(),
        Outcome::TimedOut { step } => match step {
            Some(step) => format!("timed out in step {step}"),
            None => "timed out".into(),
        },
        Outcome::ResourceExhausted { detail } => format!("resource exhausted: {detail}"),
        Outcome::InfraInterrupted { detail } => {
            format!("interrupted by an infrastructure failure: {detail}")
        }
        Outcome::Rejected { detail } => format!("rejected: {detail}"),
    }
}

/// Age of a timestamp, coarse: `5s ago`, `3m ago`, `2h ago`.
pub fn ago(at: UnixMillis) -> String {
    let now = UnixMillis::now().0;
    let secs = now.saturating_sub(at.0) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=172_799 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// A timestamp as UTC `YYYY-MM-DD HH:MM:SS`.
pub fn utc(at: UnixMillis) -> String {
    let secs = at.0 / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// One line per job for listings.
pub fn job_line(job: &JobView) -> String {
    let state = match &job.outcome {
        Some(outcome) if job.phase.is_terminal() => outcome_text(outcome),
        Some(outcome) => format!("{} ({})", outcome_text(outcome), wire_name(&job.phase)),
        None => wire_name(&job.phase),
    };
    format!(
        "{}  {:<16} {:<12} {:<28} {}",
        job.id,
        truncate(&job.project_name, 16),
        truncate(&job.recipe, 12),
        state,
        ago(job.created_at)
    )
}

/// The whole picture of one job.
pub fn job_detail(job: &JobView) -> String {
    let mut lines = vec![
        format!("job       {}", job.id),
        format!("project   {}", job.project_name),
        format!("recipe    {}", job.recipe),
        format!("snapshot  {}", job.snapshot_id),
        format!("priority  {}", wire_name(&job.priority)),
        format!("phase     {}", wire_name(&job.phase)),
    ];
    if let Some(outcome) = &job.outcome {
        lines.push(format!("outcome   {}", outcome_text(outcome)));
    }
    lines.push(format!("cleanup   {}", wire_name(&job.cleanup)));
    if job.phase.is_terminal() {
        lines.push("resources released".into());
    }
    if job.cancel_requested {
        lines.push("cancel    requested".into());
    }
    if let Some(reason) = &job.queue_reason {
        lines.push(format!(
            "waiting   {}: {}",
            wire_name(&reason.code),
            reason.message
        ));
    }
    let peak = &job.envelope.peak;
    lines.push(format!(
        "reserves  {}, {} memory",
        format_cpus(peak.cpu_milli),
        human_bytes(peak.memory_bytes)
    ));
    lines.push(format!("created   {}", utc(job.created_at)));
    if let Some(environment) = &job.environment_id {
        lines.push(format!("env       {environment}"));
    }
    for attempt in &job.attempts {
        let mut line = format!(
            "attempt {} on {}: {}",
            attempt.generation,
            attempt.machine_name,
            wire_name(&attempt.phase)
        );
        if let Some(outcome) = &attempt.outcome {
            line.push_str(&format!(", {}", outcome_text(outcome)));
        }
        line.push_str(&format!(", cleanup {}", wire_name(&attempt.cleanup)));
        if attempt.uncertain {
            line.push_str(", machine out of contact");
        }
        if attempt.released_at.is_some() {
            line.push_str(", released");
        }
        lines.push(line);
        for step in &attempt.steps {
            let mut line = format!("  step {:<20} {}", step.name, wire_name(&step.state));
            if let Some(code) = step.exit_code {
                line.push_str(&format!(" (exit {code})"));
            }
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Bytes for people: exact when the value is a whole unit (`8GiB`),
/// otherwise one decimal in binary units (`28.8GiB`).
pub fn human_bytes(bytes: u64) -> String {
    let exact = bella_recipe::units::format_bytes(bytes);
    if !exact.ends_with('B') || exact.trim_end_matches('B').parse::<u64>().is_err() || bytes < 1024
    {
        return exact;
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", units[unit])
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
        assert_eq!(
            utc(UnixMillis(86_400_000 + 3_661_000)),
            "1970-01-02 01:01:01 UTC"
        );
    }

    #[test]
    fn outcomes_read_as_sentences() {
        let failed = Outcome::Failed {
            step: Some("build".into()),
            exit_code: Some(2),
        };
        assert_eq!(outcome_text(&failed), "failed in step build (exit 2)");
        assert_eq!(outcome_text(&Outcome::Cancelled), "cancelled");
    }

    #[test]
    fn bytes_read_naturally() {
        assert_eq!(human_bytes(8 << 30), "8GiB");
        assert_eq!(human_bytes(30_923_764_538), "28.8GiB");
        assert_eq!(human_bytes(7), "7B");
    }
}
