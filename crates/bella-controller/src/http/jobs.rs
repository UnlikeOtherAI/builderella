//! Jobs: plan, submit, list, read, wait, cancel, logs, artifacts, explain.

use super::snapshots::ensure_complete;
use super::{ApiJson, ApiPath, ApiQuery, parse_id, validate_project_name};
use crate::auth::{Developer, Viewer};
use crate::error::{ApiResult, Failure};
use crate::fleet::{fleet_state, machine_names, unborn_project};
use crate::state::App;
use crate::sweeps::send_cancel;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use bella_core::ErrorCode;
use bella_core::api::{
    ArtifactView, ExplainResponse, IDEMPOTENCY_HEADER, JobView, ListJobsQuery, LogPage, LogQuery,
    PlacementOption, PlanRequest, PlanResponse, ProjectPolicy, SubmitJobRequest, WaitQuery,
    WaitResponse,
};
use bella_core::ids::JobId;
use bella_core::job::{
    CloudMode, JobPhase, JobSpec, Priority, QueueReason, QueueReasonCode, StepKind,
};
use bella_core::protocol::ControllerCommand;
use bella_core::store::ObjectStore;
use bella_ledger::{CancelOutcome, LedgerError};
use bella_scheduler::QueuedJob;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tracing::info;

/// Most steps one job may have.
const MAX_STEPS: usize = 256;

fn valid_step_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Reject a specification the fleet could not run as written. Recipes are
/// resolved and validated on the client; this is the same structural check
/// on the side that has to live with the result.
pub(crate) fn validate_spec(spec: &JobSpec) -> ApiResult<()> {
    let bad = |message: String| Err(Failure::bad_request(message));
    if spec.steps.is_empty() {
        return bad("a job needs at least one step".into());
    }
    if spec.steps.len() > MAX_STEPS {
        return bad(format!("a job has at most {MAX_STEPS} steps"));
    }
    if !spec.envelope.is_well_formed() {
        return bad("the resource envelope must satisfy initial <= peak <= maximum".into());
    }
    if spec.max_attempts == 0 {
        return bad("max_attempts counts the first attempt and must be at least 1".into());
    }
    if spec.timeout_secs == 0 {
        return bad("timeout_secs must be at least 1".into());
    }
    let mut names = BTreeSet::new();
    for step in &spec.steps {
        if !valid_step_name(&step.name) {
            return bad(format!(
                "step name `{}` must be 1 to 64 characters of a-z, 0-9, `_` and `-`",
                step.name.escape_default()
            ));
        }
        if !names.insert(step.name.as_str()) {
            return bad(format!("step `{}` is declared twice", step.name));
        }
    }
    let mut dependants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    for step in &spec.steps {
        let mut seen = BTreeSet::new();
        for dependency in &step.depends_on {
            if !names.contains(dependency.as_str()) {
                return bad(format!(
                    "step `{}` depends on `{}`, which is not a step of this job",
                    step.name,
                    dependency.escape_default()
                ));
            }
            if seen.insert(dependency.as_str()) {
                dependants
                    .entry(dependency.as_str())
                    .or_default()
                    .push(step.name.as_str());
            }
        }
        waiting.insert(step.name.as_str(), seen.len());
        if step.kind == StepKind::Service && step.readiness.is_none() {
            return bad(format!(
                "service step `{}` must declare how it becomes ready",
                step.name
            ));
        }
    }
    // Every step must be reachable in dependency order: no cycles.
    let mut ready: Vec<&str> = waiting
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(name, _)| *name)
        .collect();
    let mut ordered = 0;
    while let Some(name) = ready.pop() {
        ordered += 1;
        for dependant in dependants.get(name).map(Vec::as_slice).unwrap_or_default() {
            if let Some(count) = waiting.get_mut(dependant) {
                *count -= 1;
                if *count == 0 {
                    ready.push(*dependant);
                }
            }
        }
    }
    if ordered != spec.steps.len() {
        return bad("the steps' dependencies form a cycle".into());
    }
    Ok(())
}

/// The more restrictive of what a job asks for and what its project allows.
/// `Hybrid` permits both manual and automatic cloud capacity; a mode a
/// project does not permit is dropped from the job.
pub(crate) fn cap_cloud(requested: CloudMode, allowed: CloudMode) -> CloudMode {
    let parts = |mode: CloudMode| match mode {
        CloudMode::ExistingOnly => (false, false),
        CloudMode::Manual => (true, false),
        CloudMode::AutoOverflow => (false, true),
        CloudMode::Hybrid => (true, true),
    };
    let ((manual_a, auto_a), (manual_b, auto_b)) = (parts(requested), parts(allowed));
    match (manual_a && manual_b, auto_a && auto_b) {
        (false, false) => CloudMode::ExistingOnly,
        (true, false) => CloudMode::Manual,
        (false, true) => CloudMode::AutoOverflow,
        (true, true) => CloudMode::Hybrid,
    }
}

/// Apply a project's limits to a submitted specification. A job can ask for
/// less than its project allows, never for more.
pub(crate) fn apply_project_policy(
    spec: &mut JobSpec,
    policy: &ProjectPolicy,
    project: &str,
) -> ApiResult<()> {
    if spec.priority == Priority::Urgent && !policy.allow_urgent {
        return Err(Failure::new(
            ErrorCode::PolicyDenied,
            format!("project `{project}` does not allow urgent jobs"),
        ));
    }
    if policy.source_lan_only {
        spec.source_lan_only = true;
    }
    spec.cloud = cap_cloud(spec.cloud, policy.cloud);
    Ok(())
}

fn project_policy(app: &App, name: &str) -> ApiResult<ProjectPolicy> {
    match app.ledger.project_by_name(name) {
        Ok(project) => Ok(project.policy),
        // The first job creates the project with the default policy.
        Err(LedgerError::NotFound(_)) => Ok(ProjectPolicy::default()),
        Err(error) => Err(error.into()),
    }
}

/// `POST /jobs`. With an `idempotency-key` header the same request returns
/// the same job.
pub(crate) async fn submit(
    State(app): State<App>,
    Developer(principal): Developer,
    headers: HeaderMap,
    ApiJson(mut request): ApiJson<SubmitJobRequest>,
) -> ApiResult<Json<JobView>> {
    let key = match headers.get(IDEMPOTENCY_HEADER) {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| {
                    Failure::bad_request("the idempotency-key header must be visible ASCII")
                })?
                .to_string(),
        ),
    };
    validate_project_name(&request.project)?;
    validate_spec(&request.spec)?;
    // Held so that a garbage collection cannot remove the snapshot's
    // objects between the check below and the job that makes them live.
    let _no_collection = app.gc_gate.read().await;
    let submitter = principal.id.clone();
    let view = app
        .blocking(move |app| {
            let policy = project_policy(app, &request.project)?;
            apply_project_policy(&mut request.spec, &policy, &request.project)?;
            ensure_complete(app, &request.snapshot_id)?;
            Ok(app
                .ledger
                .submit_job(&submitter, key.as_deref(), &request)?)
        })
        .await?;
    app.changed();
    info!(
        job_id = %view.id,
        project_id = %view.project_id,
        snapshot_id = %view.snapshot_id,
        principal_id = %principal.id,
        "job submitted"
    );
    Ok(Json(view))
}

/// `GET /jobs`.
pub(crate) async fn list(
    State(app): State<App>,
    _viewer: Viewer,
    ApiQuery(query): ApiQuery<ListJobsQuery>,
) -> ApiResult<Json<Vec<JobView>>> {
    Ok(Json(app.db(move |ledger| ledger.list_jobs(&query)).await?))
}

/// `GET /jobs/{id}`, with the frozen specification.
pub(crate) async fn show(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<JobView>> {
    let id: JobId = parse_id(&id)?;
    Ok(Json(app.db(move |ledger| ledger.job(&id)).await?))
}

/// `GET /jobs/{id}/wait`: hold the request until the job has an event newer
/// than `after_event`, or, without it, until its outcome is known. Held for
/// at most the configured maximum (60 s). A timeout cancels nothing.
pub(crate) async fn wait(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
    ApiQuery(query): ApiQuery<WaitQuery>,
) -> ApiResult<Json<WaitResponse>> {
    let id: JobId = parse_id(&id)?;
    let limit = query
        .timeout_ms
        .map_or(app.tuning.max_wait, Duration::from_millis)
        .min(app.tuning.max_wait);
    let deadline = tokio::time::Instant::now() + limit;
    // Subscribed before the job is read, so no event falls between the two.
    let mut events = app.events.subscribe();
    loop {
        let wanted = id.clone();
        let job = app.db(move |ledger| ledger.job(&wanted)).await?;
        let done = match query.after_event {
            Some(after) => job.last_event > after,
            None => job.outcome.is_some() || job.phase.is_terminal(),
        };
        if done {
            return Ok(Json(WaitResponse {
                job,
                timed_out: false,
            }));
        }
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    return Ok(Json(WaitResponse { job, timed_out: true }));
                }
                () = app.shutdown.cancelled() => {
                    return Ok(Json(WaitResponse { job, timed_out: true }));
                }
                received = events.recv() => match received {
                    Ok(event) if event.job_id.as_ref() == Some(&id) => break,
                    Ok(_) => {}
                    // Too far behind to know what was missed: look again.
                    Err(RecvError::Lagged(_)) => break,
                    Err(RecvError::Closed) => {
                        return Ok(Json(WaitResponse { job, timed_out: true }));
                    }
                },
            }
        }
    }
}

/// `POST /jobs/{id}/cancel`. A queued or merely offered job ends at once; a
/// job that may be running is asked to stop and ends when its worker proves
/// the cleanup.
pub(crate) async fn cancel(
    State(app): State<App>,
    Developer(principal): Developer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<JobView>> {
    let id: JobId = parse_id(&id)?;
    let wanted = id.clone();
    let outcome = app.db(move |ledger| ledger.request_cancel(&wanted)).await?;
    let reason = "the job was cancelled";
    match &outcome {
        CancelOutcome::Cancelled {
            revoked: Some(target),
        } => {
            let revoke = ControllerCommand::Revoke {
                attempt: target.attempt.clone(),
                reason: reason.into(),
            };
            // A worker that is away drops the offer when it reconciles.
            let _ = app
                .hub
                .send(&target.machine_id, revoke, app.epoch, app.now());
        }
        CancelOutcome::Requested { target } => send_cancel(&app, target, reason).await,
        CancelOutcome::Cancelled { revoked: None } | CancelOutcome::AlreadyFinished => {}
    }
    app.changed();
    info!(job_id = %id, principal_id = %principal.id, ?outcome, "cancel requested");
    Ok(Json(app.db(move |ledger| ledger.job(&id)).await?))
}

/// `GET /jobs/{id}/logs`: one bounded page. The text is build output and is
/// passed through as data; nothing in it is interpreted.
pub(crate) async fn logs(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
    ApiQuery(query): ApiQuery<LogQuery>,
) -> ApiResult<Json<LogPage>> {
    let id: JobId = parse_id(&id)?;
    Ok(Json(
        app.db(move |ledger| ledger.read_logs(&id, &query)).await?,
    ))
}

/// `GET /jobs/{id}/artifacts`: the current attempt's outputs whose objects
/// are actually stored. An artifact that was announced but whose object
/// never arrived is not listed, so a listed artifact can always be fetched.
pub(crate) async fn artifacts(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<Vec<ArtifactView>>> {
    let id: JobId = parse_id(&id)?;
    let published = app
        .blocking(move |app| {
            let mut published = Vec::new();
            for mut artifact in app.ledger.job_artifacts(&id)? {
                if let Some(size) = app.store.size(&artifact.digest)? {
                    artifact.size = size;
                    published.push(artifact);
                }
            }
            Ok(published)
        })
        .await?;
    Ok(Json(published))
}

fn suggestions(code: QueueReasonCode) -> Vec<String> {
    use QueueReasonCode as Code;
    let texts: &[&str] = match code {
        Code::Schedulable | Code::ControllerReconciling | Code::OfferRejected => &[],
        Code::WaitingForCapacity
        | Code::FairShare
        | Code::ReservedForOlderJob
        | Code::MachineConcurrencyLimit => &[
            "wait: compatible machines are busy and the job keeps its place",
            "lower the job's resource request or parallelism so it fits beside other work",
            "finish an environment that is holding capacity",
        ],
        Code::MachineTooSmall => &[
            "lower the job's resource request or parallelism",
            "ask an administrator to enroll or authorise a larger machine",
        ],
        Code::NoCompatibleMachine
        | Code::WrongPlatform
        | Code::MissingCapability
        | Code::PlacementConstraint => &[
            "enroll a machine with the required platform and tools",
            "relax the job's platform, pool or capability requirements",
        ],
        Code::ProjectConcurrencyLimit | Code::EnvironmentQuota => &[
            "wait for the project's running work, or finish one of its environments",
            "ask an administrator to raise the project's limit",
        ],
        Code::ProjectNotAllowed | Code::BuildsDisabled | Code::SourceLanOnly => {
            &["ask an administrator to allow this project on a suitable machine"]
        }
        Code::CloudManualOnly => &["ask an administrator to authorise cloud capacity for this job"],
        Code::ForegroundProtection
        | Code::MemoryPressure
        | Code::DiskLow
        | Code::StaleTelemetry
        | Code::MachineUnreachable
        | Code::MachinePaused
        | Code::MachineDraining
        | Code::OnBattery
        | Code::Overheated
        | Code::MachineUpgrading => &[
            "wait: the condition holding the machine back is temporary",
            "enroll or free another compatible machine",
        ],
        Code::PreviousAttemptUnresolved | Code::CleanupPending | Code::Quarantined => &[
            "wait for the earlier attempt's cleanup to be confirmed",
            "ask an administrator to resolve a quarantined attempt",
        ],
        Code::SnapshotUnavailable => &["upload and register the snapshot again"],
    };
    texts.iter().map(|text| (*text).to_string()).collect()
}

/// Why a job that is not waiting in the queue is where it is.
fn not_queued(job: &JobView) -> QueueReason {
    let place = job.current_attempt().map_or_else(String::new, |attempt| {
        format!(
            " (attempt {} on {})",
            attempt.generation, attempt.machine_name
        )
    });
    match job.phase {
        JobPhase::Finished => QueueReason::new(
            QueueReasonCode::Schedulable,
            "the job has finished and is not waiting for anything",
        ),
        JobPhase::Cleaning => QueueReason::new(
            QueueReasonCode::CleanupPending,
            format!("the job's processes are being stopped and cleaned up{place}"),
        ),
        JobPhase::Quarantined => QueueReason::new(
            QueueReasonCode::Quarantined,
            format!("cleanup could not be verified, so the capacity stays held{place}"),
        ),
        JobPhase::Offered => QueueReason::new(
            QueueReasonCode::Schedulable,
            format!("the job has been offered to a machine and is about to start{place}"),
        ),
        _ => QueueReason::new(
            QueueReasonCode::Schedulable,
            format!("the job is not queued: it is running{place}"),
        ),
    }
}

/// `GET /jobs/{id}/explain`: why the job is waiting, from a fresh
/// scheduling pass, with its position and what would help.
pub(crate) async fn explain(
    State(app): State<App>,
    _viewer: Viewer,
    ApiPath(id): ApiPath,
) -> ApiResult<Json<ExplainResponse>> {
    let id: JobId = parse_id(&id)?;
    let response = app
        .blocking(move |app| {
            let job = app.ledger.job(&id)?;
            if job.phase != JobPhase::Queued {
                return Ok(ExplainResponse {
                    reason: not_queued(&job),
                    job_id: job.id,
                    phase: job.phase,
                    position: None,
                    suggestions: Vec::new(),
                });
            }
            let state = fleet_state(app, &app.ledger)?;
            let position = state
                .queue
                .iter()
                .position(|queued| queued.id == id)
                .and_then(|index| u32::try_from(index + 1).ok());
            let reason = bella_scheduler::explain(&state, &id)
                .or(job.queue_reason)
                .unwrap_or_else(|| {
                    QueueReason::new(
                        QueueReasonCode::WaitingForCapacity,
                        "the job is queued behind more jobs than one scheduling pass considers",
                    )
                });
            Ok(ExplainResponse {
                suggestions: suggestions(reason.code),
                reason,
                job_id: job.id,
                phase: job.phase,
                position,
            })
        })
        .await?;
    Ok(Json(response))
}

/// `POST /plan`: where a job could run, without submitting it.
pub(crate) async fn plan(
    State(app): State<App>,
    _viewer: Viewer,
    ApiJson(request): ApiJson<PlanRequest>,
) -> ApiResult<Json<PlanResponse>> {
    validate_project_name(&request.project)?;
    let response = app
        .blocking(move |app| {
            let PlanRequest {
                project,
                mut spec,
                origin_platform,
            } = request;
            let mut issues = Vec::new();
            if let Err(invalid) = validate_spec(&spec) {
                issues.push(invalid.message().to_string());
            }
            let mut state = fleet_state(app, &app.ledger)?;
            let known = state
                .projects
                .iter()
                .find(|candidate| candidate.name == project)
                .cloned();
            let project = match known {
                Some(project) => project,
                None => {
                    let project = unborn_project(&project);
                    state.projects.push(project.clone());
                    project
                }
            };
            if let Err(denied) = apply_project_policy(&mut spec, &project.policy, &project.name) {
                issues.push(denied.message().to_string());
            }
            let job = QueuedJob {
                id: JobId::generate(),
                project: project.id,
                session: None,
                spec: Arc::new(spec),
                origin_platform,
                queued_at: state.now,
                attempts_made: 0,
                snapshot_windows_incompatible: None,
                snapshot_case_sensitive_only: false,
                snapshot_has_symlinks: false,
                has_unresolved_attempt: false,
                recently_rejected_by: Vec::new(),
            };
            let names = machine_names(&state);
            let placements: Vec<PlacementOption> = bella_scheduler::evaluate(&state, &job)
                .into_iter()
                .map(|(machine_id, verdict)| PlacementOption {
                    machine_name: names.get(&machine_id).cloned().unwrap_or_default(),
                    machine_id,
                    eligible_now: verdict.is_ok(),
                    blocker: verdict.err(),
                })
                .collect();
            let feasible = placements.iter().any(|placement| {
                placement.eligible_now
                    || placement
                        .blocker
                        .as_ref()
                        .is_some_and(|blocker| !blocker.permanent)
            });
            if placements.is_empty() {
                issues.push("no machine is enrolled in this fleet".to_string());
            }
            if let Some(reason) = bella_scheduler::job_blocker(&state, &job) {
                issues.push(reason.message);
            }
            Ok(PlanResponse {
                feasible,
                placements,
                issues,
            })
        })
        .await?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::Digest;
    use bella_core::job::{CommandSpec, Readiness, ReadinessProbe, StepSpec};
    use bella_core::resources::{ResourceEnvelope, ResourceVector};

    fn step(name: &str, depends_on: &[&str]) -> StepSpec {
        StepSpec {
            name: name.into(),
            kind: StepKind::Task,
            command: CommandSpec::Argv {
                argv: vec!["true".into()],
            },
            cwd: None,
            env: Default::default(),
            depends_on: depends_on.iter().map(|s| (*s).to_string()).collect(),
            readiness: None,
            timeout_secs: None,
            resources: None,
            ports: Vec::new(),
        }
    }

    fn spec(steps: Vec<StepSpec>) -> JobSpec {
        JobSpec {
            recipe: "build".into(),
            recipe_digest: Digest::of_bytes(b"recipe"),
            steps,
            envelope: ResourceEnvelope::fixed(ResourceVector::new(1000, 1 << 30, 0)),
            compatibility: Default::default(),
            placement: Default::default(),
            priority: Default::default(),
            retry_class: Default::default(),
            max_attempts: 1,
            timeout_secs: 60,
            cleanup_timeout_secs: 60,
            cloud: Default::default(),
            source_lan_only: false,
            outputs: Default::default(),
            environment: None,
            env: Default::default(),
            disconnected_continuation_secs: 0,
            parallelism: None,
        }
    }

    #[test]
    fn well_formed_specs_pass() {
        let ok = spec(vec![
            step("build", &[]),
            step("test", &["build"]),
            step("lint", &["build", "build"]),
        ]);
        assert!(validate_spec(&ok).is_ok());
    }

    #[test]
    fn malformed_specs_are_rejected() {
        let mut envelope = spec(vec![step("build", &[])]);
        envelope.envelope.initial = ResourceVector::new(9000, 1 << 30, 0);
        let mut service = step("db", &[]);
        service.kind = StepKind::Service;
        let mut ready_service = service.clone();
        ready_service.readiness = Some(Readiness {
            probe: ReadinessProbe::Tcp { port: 5432 },
            timeout_secs: 5,
            interval_ms: 100,
        });
        assert!(validate_spec(&spec(vec![ready_service])).is_ok());
        let mut zero_timeout = spec(vec![step("build", &[])]);
        zero_timeout.timeout_secs = 0;
        let cases = [
            spec(vec![]),
            envelope,
            spec(vec![step("Build", &[])]),
            spec(vec![step("a", &[]), step("a", &[])]),
            spec(vec![step("a", &["missing"])]),
            spec(vec![step("a", &["b"]), step("b", &["a"])]),
            spec(vec![step("a", &["a"])]),
            spec(vec![service]),
            zero_timeout,
        ];
        for (index, case) in cases.iter().enumerate() {
            let failure = validate_spec(case).expect_err(&format!("case {index}"));
            assert_eq!(failure.code(), ErrorCode::BadRequest, "case {index}");
        }
    }

    #[test]
    fn cloud_mode_is_capped_by_the_project() {
        use CloudMode::*;
        assert_eq!(cap_cloud(Hybrid, ExistingOnly), ExistingOnly);
        assert_eq!(cap_cloud(Hybrid, Manual), Manual);
        assert_eq!(cap_cloud(Hybrid, AutoOverflow), AutoOverflow);
        assert_eq!(cap_cloud(Hybrid, Hybrid), Hybrid);
        assert_eq!(cap_cloud(AutoOverflow, Manual), ExistingOnly);
        assert_eq!(cap_cloud(Manual, Hybrid), Manual);
        assert_eq!(cap_cloud(ExistingOnly, Hybrid), ExistingOnly);
    }

    #[test]
    fn project_policy_limits_a_submitted_spec() {
        let policy = ProjectPolicy {
            source_lan_only: true,
            cloud: CloudMode::Manual,
            ..ProjectPolicy::default()
        };
        let mut job = spec(vec![step("build", &[])]);
        job.cloud = CloudMode::Hybrid;
        apply_project_policy(&mut job, &policy, "app").unwrap();
        assert!(job.source_lan_only);
        assert_eq!(job.cloud, CloudMode::Manual);

        job.priority = Priority::Urgent;
        let denied = apply_project_policy(&mut job, &policy, "app").unwrap_err();
        assert_eq!(denied.code(), ErrorCode::PolicyDenied);
        let urgent = ProjectPolicy {
            allow_urgent: true,
            ..policy
        };
        assert!(apply_project_policy(&mut job, &urgent, "app").is_ok());
    }
}
