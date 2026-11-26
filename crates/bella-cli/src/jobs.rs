//! Developer and agent commands: submit, plan, jobs, job, wait, logs,
//! cancel, artifacts, explain and environments.

use crate::cli::{
    ArtifactsArgs, EnvCmd, JobIdArg, JobsArgs, LogsArgs, PlanArgs, PriorityArg, SubmitArgs,
    WaitArgs, WorkArgs,
};
use crate::ctx::{Ctx, interrupted, runtime};
use crate::error::{CliError, CliResult, Exit, wire_name};
use crate::output::{Out, job_detail, job_line, outcome_text, print_raw, utc};
use crate::work::{self, WorkError, WorkRequest};
use bella_client::{BlockingClient, Client, ClientError};
use bella_core::api::{
    ArtifactView, EnvironmentView, JobView, ListJobsQuery, LogEntry, LogQuery, WaitQuery,
};
use bella_core::job::{CommandSpec, JobPhase, Outcome, Priority};
use bella_core::platform::PlacementChoice;
use bella_core::resources::ResourceVector;
use bella_core::{AttemptId, EnvironmentId, JobId};
use bella_recipe::Overrides;
use bella_recipe::units::{format_cpus, parse_bytes, parse_cpu, parse_duration_secs};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

/// How often a followed log is polled when it has nothing new.
const LOG_POLL: Duration = Duration::from_millis(300);
/// Bytes of a failed step's log shown after the job ends.
const FAILURE_TAIL_BYTES: u64 = 2048;
/// Lines of it shown.
const FAILURE_TAIL_LINES: usize = 20;

pub fn parse_job_id(text: &str) -> CliResult<JobId> {
    text.trim()
        .parse()
        .map_err(|error| CliError::local(format!("`{text}` is not a job id: {error}")))
}

fn parse_duration(flag: &str, text: &str) -> CliResult<Duration> {
    parse_duration_secs(text)
        .map(Duration::from_secs)
        .map_err(|error| CliError::local(format!("--{flag}: {error}")))
}

fn priority(arg: PriorityArg) -> Priority {
    match arg {
        PriorityArg::Maintenance => Priority::Maintenance,
        PriorityArg::Background => Priority::Background,
        PriorityArg::Normal => Priority::Normal,
        PriorityArg::Interactive => Priority::Interactive,
        PriorityArg::Urgent => Priority::Urgent,
    }
}

/// The work request a `submit` or `plan` command line describes.
pub fn work_request(args: &WorkArgs) -> CliResult<WorkRequest> {
    let workspace = match &args.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|error| {
            CliError::local(format!("cannot read the current directory: {error}"))
        })?,
    };
    let command = if args.command.is_empty() {
        None
    } else {
        Some(CommandSpec::Argv {
            argv: args.command.clone(),
        })
    };
    let placement = match (&args.platform, &args.machine) {
        (Some(text), _) => {
            let platform = bella_recipe::parse_platform(text)
                .map_err(|error| CliError::local(format!("--platform: {error}")))?;
            Some(if args.require {
                PlacementChoice::Require { platform }
            } else {
                PlacementChoice::Prefer { platform }
            })
        }
        (None, Some(machine)) => Some(PlacementChoice::Machine {
            machine: machine.clone(),
        }),
        (None, None) => None,
    };
    let mut env = BTreeMap::new();
    for pair in &args.env {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| CliError::local(format!("--env `{pair}`: write it as KEY=VALUE")))?;
        if key.is_empty() {
            return Err(CliError::local(format!(
                "--env `{pair}`: the name is empty"
            )));
        }
        env.insert(key.to_string(), value.to_string());
    }
    let timeout_secs = args
        .timeout
        .as_deref()
        .map(|text| parse_duration("timeout", text).map(|d| d.as_secs()))
        .transpose()?;
    let resources = if args.cpu.is_some() || args.memory.is_some() {
        let mut vector = ResourceVector::default();
        if let Some(cpu) = &args.cpu {
            vector.cpu_milli =
                parse_cpu(cpu).map_err(|error| CliError::local(format!("--cpu: {error}")))?;
        }
        if let Some(memory) = &args.memory {
            vector.memory_bytes = parse_bytes(memory)
                .map_err(|error| CliError::local(format!("--memory: {error}")))?;
        }
        Some(vector)
    } else {
        None
    };
    Ok(WorkRequest {
        workspace,
        recipe: args.recipe.clone(),
        command,
        project: args.project.clone(),
        overrides: Overrides {
            priority: args.priority.map(priority),
            placement,
            env,
            timeout_secs,
            lan_only: args.lan_only,
            ..Overrides::default()
        },
        resources,
    })
}

fn work_error(error: WorkError) -> CliError {
    match error {
        WorkError::Client(error) => CliError::client(error),
        other => CliError::local(other.describe()),
    }
}

/// `<controller url>/#/jobs/<id>`.
pub fn dashboard_link(client: &BlockingClient, id: &JobId) -> String {
    format!("{}/#/jobs/{id}", client.client().url())
}

pub fn plan(ctx: &Ctx, args: &PlanArgs) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    let request = work_request(&args.work)?;
    let (prepared, response) = work::plan(&client, &request).map_err(work_error)?;
    for warning in &prepared.warnings {
        ctx.out.note(warning);
    }
    let document = json!({
        "project": prepared.project,
        "recipe": prepared.spec.recipe,
        "recipe_file": prepared.recipe_file,
        "envelope": prepared.spec.envelope,
        "warnings": prepared.warnings,
        "plan": response,
    });
    ctx.out.emit(&document, || {
        let peak = &prepared.spec.envelope.peak;
        let mut lines = vec![format!(
            "{} / {}: reserves {}, {} memory",
            prepared.project,
            prepared.spec.recipe,
            format_cpus(peak.cpu_milli),
            crate::output::human_bytes(peak.memory_bytes)
        )];
        lines.push(if response.feasible {
            "feasible: at least one machine could run it".into()
        } else {
            "not feasible: no enrolled machine can run it as specified".into()
        });
        for placement in &response.placements {
            let state = match (&placement.blocker, placement.eligible_now) {
                (_, true) => "could start now".to_string(),
                (Some(blocker), false) => format!(
                    "{}{}: {}",
                    wire_name(&blocker.code),
                    if blocker.permanent {
                        " (permanent)"
                    } else {
                        ""
                    },
                    blocker.message
                ),
                (None, false) => "not now".to_string(),
            };
            lines.push(format!("  {:<24} {state}", placement.machine_name));
        }
        lines.extend(
            response
                .issues
                .iter()
                .map(|issue| format!("issue: {issue}")),
        );
        lines.join("\n")
    });
    Ok(Exit::Success)
}

pub fn submit(ctx: &Ctx, args: &SubmitArgs) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    let request = work_request(&args.work)?;
    let wait_timeout = args
        .wait_timeout
        .as_deref()
        .map(|text| parse_duration("wait-timeout", text))
        .transpose()?;
    let key = args
        .idempotency_key
        .clone()
        .unwrap_or_else(bella_client::new_idempotency_key);
    let submitted = work::submit(&client, &request, &key, None).map_err(work_error)?;

    for warning in &submitted.warnings {
        ctx.out.note(warning);
    }
    for secret in &submitted.capture.excluded_secrets {
        ctx.out.note(format!(
            "left out of the snapshot as a likely secret: {secret}"
        ));
    }
    if ctx.verbose > 0 {
        let stats = &submitted.capture.stats;
        ctx.out.note(format!(
            "captured {} files ({}), {} symlinks; uploaded {} objects ({}); consistency {}",
            stats.files,
            crate::output::human_bytes(stats.bytes),
            stats.symlinks,
            submitted.uploaded_objects,
            crate::output::human_bytes(submitted.uploaded_bytes),
            wire_name(&submitted.capture.meta.consistency)
        ));
        ctx.out.note(
            "left out by rule: the .git directory, git-ignored files (unless declared in \
             include_ignored), and files matching these patterns:",
        );
        let policy = &submitted.capture_policy;
        for pattern in &policy.exclude {
            ctx.out.note(format!("  exclude {pattern}"));
        }
        for pattern in &policy.secret_patterns {
            ctx.out.note(format!("  secret  {pattern}"));
        }
        for pattern in &policy.allow_secrets {
            ctx.out.note(format!("  allowed {pattern}"));
        }
    }

    let job = submitted.job;
    let link = dashboard_link(&client, &job.id);
    if !ctx.out.json {
        crate::output::print_line(&format!("job {}", job.id));
        crate::output::print_line(&format!("dashboard {link}"));
        ctx.out.note(format!(
            "idempotency key {key} (submit again with --idempotency-key {key} to get this job back)"
        ));
    }

    let mut document = json!({
        "job": job,
        "snapshot_id": submitted.snapshot.id,
        "idempotency_key": key,
        "dashboard": link,
        "excluded_secrets": submitted.capture.excluded_secrets,
        "warnings": submitted.warnings,
    });
    if !(args.wait || args.follow) {
        if ctx.out.json {
            ctx.out.print_json(&document);
        }
        return Ok(Exit::Success);
    }

    let client = ctx.client()?;
    let end = runtime()?.block_on(wait_with_tail(
        ctx.out,
        &client,
        &job.id,
        wait_timeout,
        args.follow,
    ))?;
    finish_wait(ctx, end, Some(&mut document))
}

/// How a wait ended.
pub enum WaitEnd {
    Settled(JobView),
    TimedOut(JobView),
    Interrupted(JobId),
}

/// Wait until the job is finished and released, optionally printing its log
/// as it arrives. A timeout or Ctrl-C ends the wait only.
pub async fn wait_for(
    out: Out,
    client: &Client,
    id: &JobId,
    timeout: Option<Duration>,
    follow: bool,
) -> CliResult<WaitEnd> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let waiting = settle(client, id, deadline);
    let following = async {
        if follow {
            follow_logs(out, client, id, None, None).await
        } else {
            std::future::pending::<Result<(), ClientError>>().await
        }
    };
    tokio::pin!(waiting);
    tokio::pin!(following);
    let mut log_done = !follow;
    loop {
        tokio::select! {
            () = interrupted() => return Ok(WaitEnd::Interrupted(id.clone())),
            result = &mut waiting => {
                let end = result?;
                if follow && !log_done && matches!(end, WaitEnd::Settled(_)) {
                    // The job is finished: the rest of its log is already
                    // stored. Read it to the end, but not forever.
                    let rest = tokio::time::timeout(Duration::from_secs(10), &mut following);
                    if let Ok(Err(error)) = rest.await {
                        tracing::warn!(%error, "could not read the rest of the log");
                    }
                }
                return Ok(end);
            }
            result = &mut following, if !log_done => {
                log_done = true;
                if let Err(error) = result {
                    out.note(format!("warning: stopped following the log: {error}"));
                }
            }
        }
    }
}

/// Long-poll until the job's phase is terminal or the deadline passes.
async fn settle(
    client: &Client,
    id: &JobId,
    deadline: Option<Instant>,
) -> Result<WaitEnd, CliError> {
    let mut after = None;
    loop {
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let timeout_ms = match remaining {
            Some(remaining) if remaining.is_zero() => {
                let job = client.job(id).await?;
                return Ok(if job.phase.is_terminal() {
                    WaitEnd::Settled(job)
                } else {
                    WaitEnd::TimedOut(job)
                });
            }
            Some(remaining) => remaining.min(bella_client::MAX_WAIT).as_millis() as u64,
            None => bella_client::MAX_WAIT.as_millis() as u64,
        };
        let response = client
            .wait_job(
                id,
                &WaitQuery {
                    timeout_ms: Some(timeout_ms.max(1)),
                    after_event: after,
                },
            )
            .await?;
        let job = response.job;
        if job.phase.is_terminal() {
            return Ok(WaitEnd::Settled(job));
        }
        // Once the outcome is known the plain wait returns at once; from
        // then on wake on the job's next event until it is released.
        if job.outcome.is_some() {
            after = Some(job.last_event);
        }
    }
}

/// Print log entries from `cursor` on until the job's output is complete.
async fn follow_logs(
    out: Out,
    client: &Client,
    id: &JobId,
    step: Option<String>,
    cursor: Option<String>,
) -> Result<(), ClientError> {
    let mut cursor = cursor;
    loop {
        let page = client
            .job_logs(
                id,
                &LogQuery {
                    step: step.clone(),
                    cursor: cursor.clone(),
                    ..LogQuery::default()
                },
            )
            .await?;
        print_entries(out, &page.entries);
        let empty = page.entries.is_empty();
        cursor = Some(page.next_cursor);
        if page.complete && empty {
            return Ok(());
        }
        if empty {
            tokio::time::sleep(LOG_POLL).await;
        }
    }
}

/// Log text goes to standard output, or to standard error under `--json`
/// so that standard output stays one JSON document.
fn print_entries(out: Out, entries: &[LogEntry]) {
    for entry in entries {
        if out.json {
            eprint!("{}", entry.text);
        } else {
            print_raw(&entry.text);
        }
    }
}

/// Report how a wait ended and choose the exit status.
fn finish_wait(
    ctx: &Ctx,
    (end, tail): (WaitEnd, Option<FailureTail>),
    document: Option<&mut serde_json::Value>,
) -> CliResult<Exit> {
    let (job, exit) = match end {
        WaitEnd::Interrupted(id) => {
            ctx.out.note(format!(
                "stopped waiting; job {id} keeps running. Cancel it with `bella cancel {id}`."
            ));
            if ctx.out.json {
                let mut value = json!({ "interrupted": true, "job_id": id });
                if let Some(document) = document {
                    document["interrupted"] = json!(true);
                    value = document.clone();
                }
                ctx.out.print_json(&value);
            }
            return Ok(Exit::Interrupted);
        }
        WaitEnd::TimedOut(job) => {
            ctx.out.note(format!(
                "the wait timed out; job {} keeps running ({}). Wait again with `bella wait {}`.",
                job.id,
                wire_name(&job.phase),
                job.id
            ));
            (job, Exit::WaitTimeout)
        }
        WaitEnd::Settled(job) => {
            let succeeded = job.outcome.as_ref().is_some_and(Outcome::is_success);
            (
                job,
                if succeeded {
                    Exit::Success
                } else {
                    Exit::JobFailed
                },
            )
        }
    };

    if exit == Exit::JobFailed
        && let Some(tail) = tail
    {
        ctx.out.note(format!(
            "--- last output of step {} (untrusted build output) ---",
            tail.step
        ));
        for line in &tail.lines {
            ctx.out.note(line);
        }
        ctx.out.note("---");
    }
    if ctx.out.json {
        let value = match document {
            Some(document) => {
                document["job"] = json!(job);
                document["timed_out"] = json!(exit == Exit::WaitTimeout);
                document.clone()
            }
            None => json!({ "job": job, "timed_out": exit == Exit::WaitTimeout }),
        };
        ctx.out.print_json(&value);
    } else if exit != Exit::WaitTimeout {
        let outcome = job
            .outcome
            .as_ref()
            .map(outcome_text)
            .unwrap_or_else(|| wire_name(&job.phase));
        crate::output::print_line(&format!("job {} {outcome}", job.id));
    }
    Ok(exit)
}

/// Wait as [`wait_for`] does and, when the job failed in a step, fetch the
/// last lines of that step's output.
pub async fn wait_with_tail(
    out: Out,
    client: &Client,
    id: &JobId,
    timeout: Option<Duration>,
    follow: bool,
) -> CliResult<(WaitEnd, Option<FailureTail>)> {
    let end = wait_for(out, client, id, timeout, follow).await?;
    let tail = match &end {
        WaitEnd::Settled(job) => failure_tail(client, job).await,
        _ => None,
    };
    Ok((end, tail))
}

/// The end of a failed step's output.
pub struct FailureTail {
    pub step: String,
    pub lines: Vec<String>,
}

async fn failure_tail(client: &Client, job: &JobView) -> Option<FailureTail> {
    let step = match &job.outcome {
        Some(Outcome::Failed { step, .. } | Outcome::TimedOut { step }) => step.clone(),
        _ => None,
    }?;
    let page = client
        .job_logs(
            &job.id,
            &LogQuery {
                step: Some(step.clone()),
                tail: Some(true),
                limit_bytes: Some(FAILURE_TAIL_BYTES),
                ..LogQuery::default()
            },
        )
        .await
        .ok()?;
    let text: String = page
        .entries
        .iter()
        .map(|entry| entry.text.as_str())
        .collect();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.len().saturating_sub(FAILURE_TAIL_LINES);
    Some(FailureTail {
        step,
        lines: lines[start..].to_vec(),
    })
}

pub fn wait(ctx: &Ctx, args: &WaitArgs) -> CliResult<Exit> {
    let id = parse_job_id(&args.id)?;
    let timeout = args
        .timeout
        .as_deref()
        .map(|text| parse_duration("timeout", text))
        .transpose()?;
    let client = ctx.client()?;
    let end = runtime()?.block_on(wait_with_tail(ctx.out, &client, &id, timeout, false))?;
    finish_wait(ctx, end, None)
}

pub fn logs(ctx: &Ctx, args: &LogsArgs) -> CliResult<Exit> {
    let id = parse_job_id(&args.id)?;
    let tail = args
        .tail
        .as_deref()
        .map(|text| parse_bytes(text).map_err(|error| CliError::local(format!("--tail: {error}"))))
        .transpose()?;
    let client = ctx.client()?;
    let out = ctx.out;
    runtime()?.block_on(async {
        let mut collected = Vec::new();
        let mut cursor = None;
        let mut last: Option<bella_core::api::LogPage>;
        // One tail page, or every page from the start.
        loop {
            let page = client
                .job_logs(
                    &id,
                    &LogQuery {
                        step: args.step.clone(),
                        cursor: cursor.clone(),
                        tail: tail.map(|_| true),
                        limit_bytes: tail,
                    },
                )
                .await?;
            let empty = page.entries.is_empty();
            cursor = Some(page.next_cursor.clone());
            if out.json && !args.follow {
                collected.extend(page.entries.iter().cloned());
            } else {
                print_entries(out, &page.entries);
            }
            last = Some(page);
            if empty || tail.is_some() {
                break;
            }
        }
        if args.follow {
            tokio::select! {
                result = follow_logs(out, &client, &id, args.step.clone(), cursor) => result?,
                () = interrupted() => return Ok(Exit::Interrupted),
            }
            if out.json {
                out.print_json(&json!({ "job_id": id, "complete": true }));
            }
        } else if out.json {
            let (truncated, complete, next_cursor) = last
                .map(|page| (page.truncated, page.complete, page.next_cursor))
                .unwrap_or_default();
            out.print_json(&json!({
                "entries": collected,
                "truncated": truncated,
                "complete": complete,
                "next_cursor": next_cursor,
            }));
        }
        Ok::<_, CliError>(Exit::Success)
    })
}

pub fn jobs(ctx: &Ctx, args: &JobsArgs) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    let state = args.state.as_deref().map(str::trim);
    let phase = match state {
        None | Some("active") | Some("finished") | Some("all") => None,
        Some(other) => Some(
            serde_json::from_value::<JobPhase>(json!(other.replace('-', "_"))).map_err(|_| {
                CliError::local(format!(
                    "--state `{other}`: use active, finished, all or a phase such as queued, \
                     executing, cleaning"
                ))
            })?,
        ),
    };
    let jobs = client.jobs(&ListJobsQuery {
        project: args.project.clone(),
        session: None,
        active: (state == Some("active")).then_some(true),
        limit: Some(args.limit),
    })?;
    let jobs: Vec<JobView> = jobs
        .into_iter()
        .filter(|job| match (state, phase) {
            (Some("finished"), _) => job.phase.is_terminal(),
            (_, Some(phase)) => job.phase == phase,
            _ => true,
        })
        .collect();
    ctx.out.emit(&jobs, || {
        if jobs.is_empty() {
            return "no jobs".into();
        }
        jobs.iter().map(job_line).collect::<Vec<_>>().join("\n")
    });
    Ok(Exit::Success)
}

pub fn job(ctx: &Ctx, args: &JobIdArg) -> CliResult<Exit> {
    let id = parse_job_id(&args.id)?;
    let client = ctx.blocking()?;
    let job = client.job(&id)?;
    let link = dashboard_link(&client, &id);
    ctx.out
        .emit(&job, || format!("{}\ndashboard {link}", job_detail(&job)));
    Ok(Exit::Success)
}

pub fn cancel(ctx: &Ctx, args: &JobIdArg) -> CliResult<Exit> {
    let id = parse_job_id(&args.id)?;
    let client = ctx.blocking()?;
    let job = client.cancel_job(&id)?;
    ctx.out.emit(&job, || {
        if job.phase.is_terminal() {
            format!(
                "job {} is finished ({}); its resources are released",
                job.id,
                job.outcome
                    .as_ref()
                    .map(outcome_text)
                    .unwrap_or_else(|| "cancelled".into())
            )
        } else {
            format!(
                "cancellation of job {} requested; cleanup is {} and its resources stay reserved \
                 until the worker confirms it (follow with `bella wait {}`)",
                job.id,
                wire_name(&job.cleanup),
                job.id
            )
        }
    });
    Ok(Exit::Success)
}

pub fn explain(ctx: &Ctx, args: &JobIdArg) -> CliResult<Exit> {
    let id = parse_job_id(&args.id)?;
    let client = ctx.blocking()?;
    let explained = client.explain_job(&id)?;
    ctx.out.emit(&explained, || {
        let mut lines = vec![format!(
            "job {} is {}: {} ({})",
            explained.job_id,
            wire_name(&explained.phase),
            explained.reason.message,
            wire_name(&explained.reason.code)
        )];
        if let Some(position) = explained.position {
            lines.push(format!("position {position} in the queue"));
        }
        for blocker in &explained.reason.blockers {
            lines.push(format!(
                "  {:<24} {}{}: {}",
                blocker.machine,
                wire_name(&blocker.code),
                if blocker.permanent {
                    " (permanent)"
                } else {
                    ""
                },
                blocker.message
            ));
        }
        lines.extend(
            explained
                .suggestions
                .iter()
                .map(|suggestion| format!("suggestion: {suggestion}")),
        );
        lines.join("\n")
    });
    Ok(Exit::Success)
}

/// For every artifact path, the one from the newest attempt.
pub fn newest_artifacts(job: &JobView, artifacts: Vec<ArtifactView>) -> Vec<ArtifactView> {
    let generation = |attempt: &AttemptId| {
        job.attempts
            .iter()
            .find(|view| &view.id == attempt)
            .map_or(0, |view| view.generation)
    };
    let mut newest: BTreeMap<String, ArtifactView> = BTreeMap::new();
    for artifact in artifacts {
        let replace = newest
            .get(&artifact.path)
            .is_none_or(|known| generation(&artifact.attempt_id) > generation(&known.attempt_id));
        if replace {
            newest.insert(artifact.path.clone(), artifact);
        }
    }
    newest.into_values().collect()
}

/// `dest` joined with an artifact path, refusing anything that would leave
/// `dest`.
pub fn artifact_target(dest: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    let plain = relative
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if path.is_empty() || !plain {
        return Err(format!(
            "artifact path `{path}` is not a plain relative path"
        ));
    }
    Ok(dest.join(relative))
}

/// Download one artifact into a new file below `dest`. Never overwrites.
pub fn download_artifact(
    client: &BlockingClient,
    dest: &Path,
    artifact: &ArtifactView,
) -> Result<PathBuf, (bool, String)> {
    let target = artifact_target(dest, &artifact.path).map_err(|message| (false, message))?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            (
                false,
                format!("cannot create {}: {error}", parent.display()),
            )
        })?;
    }
    match client
        .object_store()
        .fetch_raw_to_file(&artifact.digest, &target)
    {
        Ok(_) => Ok(target),
        Err(bella_core::store::StoreError::Io(error))
            if error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            Err((
                true,
                format!(
                    "{} already exists; nothing was overwritten",
                    target.display()
                ),
            ))
        }
        Err(error) => Err((
            false,
            format!("downloading {} failed: {error}", artifact.path),
        )),
    }
}

pub fn artifacts(ctx: &Ctx, args: &ArtifactsArgs) -> CliResult<Exit> {
    let id = parse_job_id(&args.id)?;
    let client = ctx.blocking()?;
    let listed = client.job_artifacts(&id)?;
    let Some(dest) = &args.download else {
        ctx.out.emit(&listed, || {
            if listed.is_empty() {
                return "no artifacts".into();
            }
            listed
                .iter()
                .map(|artifact| {
                    format!(
                        "{:<40} {:>10}  {}  {}",
                        artifact.path,
                        crate::output::human_bytes(artifact.size),
                        artifact.digest,
                        artifact.attempt_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
        return Ok(Exit::Success);
    };

    let job = client.job(&id)?;
    std::fs::create_dir_all(dest)
        .map_err(|error| CliError::local(format!("cannot create {}: {error}", dest.display())))?;
    let mut written = Vec::new();
    let mut failures = Vec::new();
    for artifact in newest_artifacts(&job, listed) {
        match download_artifact(&client, dest, &artifact) {
            Ok(path) => {
                if !ctx.out.json {
                    crate::output::print_line(&path.display().to_string());
                }
                written.push(json!({ "path": artifact.path, "file": path, "digest": artifact.digest, "size": artifact.size }));
            }
            Err((_, message)) => {
                ctx.out.note(format!("error: {message}"));
                failures.push(message);
            }
        }
    }
    if ctx.out.json {
        ctx.out
            .print_json(&json!({ "downloaded": written, "failed": failures }));
    }
    if failures.is_empty() {
        Ok(Exit::Success)
    } else {
        Err(CliError::local(format!(
            "{} of the artifacts could not be downloaded",
            failures.len()
        )))
    }
}

fn parse_env_id(text: &str) -> CliResult<EnvironmentId> {
    text.trim()
        .parse()
        .map_err(|error| CliError::local(format!("`{text}` is not an environment id: {error}")))
}

fn environment_text(env: &EnvironmentView) -> String {
    let mut lines = vec![
        format!("environment {}", env.id),
        format!("job         {}", env.job_id),
        format!("machine     {}", env.machine_name),
        format!("state       {}", wire_name(&env.state)),
        format!("deadline    {}", utc(env.effective_deadline)),
    ];
    for endpoint in &env.endpoints {
        lines.push(format!(
            "endpoint    {} {}:{}",
            endpoint.step, endpoint.host, endpoint.port
        ));
    }
    if let Some(reason) = &env.ended_reason {
        lines.push(format!("ended       {reason}"));
    }
    lines.join("\n")
}

pub fn env(ctx: &Ctx, command: &EnvCmd) -> CliResult<Exit> {
    let client = ctx.blocking()?;
    match command {
        EnvCmd::List => {
            let list = client.environments()?;
            ctx.out.emit(&list, || {
                if list.is_empty() {
                    return "no environments".into();
                }
                list.iter()
                    .map(|env| {
                        format!(
                            "{}  {:<12} {:<20} until {}",
                            env.id,
                            wire_name(&env.state),
                            env.machine_name,
                            utc(env.effective_deadline)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        EnvCmd::Show(arg) => {
            let env = client.environment(&parse_env_id(&arg.id)?)?;
            ctx.out.emit(&env, || environment_text(&env));
        }
        EnvCmd::Renew(arg) => {
            let env = client.renew_environment(&parse_env_id(&arg.id)?)?;
            ctx.out.emit(&env, || environment_text(&env));
        }
        EnvCmd::Finish(arg) => {
            let env = client.finish_environment(&parse_env_id(&arg.id)?)?;
            ctx.out.emit(&env, || {
                format!(
                    "{}\nfinishing was requested; its resources are released once cleanup is \
                     confirmed",
                    environment_text(&env)
                )
            });
        }
    }
    Ok(Exit::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_paths_cannot_leave_the_destination() {
        let dest = Path::new("out");
        assert_eq!(
            artifact_target(dest, "dist/app").unwrap(),
            Path::new("out").join("dist/app")
        );
        assert!(artifact_target(dest, "../etc/passwd").is_err());
        assert!(artifact_target(dest, "/etc/passwd").is_err());
        assert!(artifact_target(dest, "").is_err());
    }
}
