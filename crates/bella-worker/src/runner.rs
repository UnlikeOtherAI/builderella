//! The attempt runner: one thread per started attempt, from `Preparing` to
//! the release proof, plus the cleanup pass shared with recovery and with
//! the retry of quarantined attempts.
//!
//! Every state change is journaled, together with the event that reports
//! it, before the runner acts on it.

use crate::SYSTEM_STEP;
use crate::collect;
use crate::core::{AttemptHandle, Core, StopReason, describe_incompatibilities, scope_limits};
use crate::env;
use crate::readiness;
use crate::spool::{AttemptLogs, LogWatch, Pump};
use bella_contain::{Child, ExitInfo, LimitHits, Membership, ProcessSpec};
use bella_core::AttemptId;
use bella_core::ids::Digest;
use bella_core::job::{CleanupState, CommandSpec, Outcome, ReadinessProbe, StepKind, StepSpec};
use bella_core::machine::MachinePolicy;
use bella_core::protocol::{
    ArtifactKind, AttemptEvent, AttemptRef, Endpoint, LocalAttemptState, LogStream, Offer,
    ReleaseProof, StepState, UsageSummary,
};
use bella_core::snapshot::Manifest;
use bella_core::store::StoreError;
use bella_journal::{AttemptRecord, JournalError, JournalTx};
use bella_materialize::MaterializeError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Most files collected as artifacts and test reports of one attempt.
const MAX_COLLECTED_FILES: usize = 10_000;
/// How long a stopped process may take to be reaped after its scope was
/// killed, and how long output pipes are read after the scope is empty.
const REAP_LIMIT: Duration = Duration::from_secs(2);
/// Longest a single readiness command may run.
const PROBE_COMMAND_LIMIT: Duration = Duration::from_secs(30);

/// Start the runner thread of an attempt whose start permit is fresh.
pub(crate) fn launch(core: &Arc<Core>, handle: Arc<AttemptHandle>) {
    let name = format!("bella-run-{}", short_id(handle.id()));
    let thread_core = core.clone();
    let thread_handle = handle.clone();
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let mut run = Run::new(&thread_core, &thread_handle);
        let outcome = run.execute();
        run.conclude(outcome);
        thread_handle.busy.store(false, Ordering::SeqCst);
    });
    match spawned {
        Ok(thread) => core.adopt_thread(thread),
        Err(error) => {
            // Nothing ran and nothing was created: report and release.
            let mut run = Run::new(core, &handle);
            run.conclude(Outcome::InfraInterrupted {
                detail: format!("the worker could not start a thread for the attempt: {error}"),
            });
            handle.busy.store(false, Ordering::SeqCst);
        }
    }
}

/// Settle an attempt found in the journal at startup: report it as
/// interrupted unless it already has an outcome, then clean it up.
///
/// `proven` is how the absence of processes is known when no scope of the
/// current boot is on record (a reboot, or no scope was ever created).
pub(crate) fn recover(core: &Arc<Core>, record: AttemptRecord, detail: &str, proven: Option<&str>) {
    let attempt = record.attempt.clone();
    let handle = AttemptHandle::new(record.offer.clone(), record.state);
    handle.busy.store(true, Ordering::SeqCst);
    core.insert_handle(handle.clone());
    let logs = core.logs.open(&attempt, &core.journal);
    logs.system(SYSTEM_STEP, &format!("[builderella] {detail}"));

    let journaled = core.tx(|tx| {
        let id = &attempt.attempt_id;
        let current = tx
            .attempt(id)?
            .ok_or_else(|| JournalError::NotFound(id.clone()))?;
        if current.outcome.is_none() {
            let outcome = Outcome::InfraInterrupted {
                detail: detail.to_string(),
            };
            let usage = current.usage.clone().unwrap_or_default();
            if tx.record_outcome(id, &outcome, &usage)? {
                tx.append_event(&attempt, AttemptEvent::Exited { outcome, usage })?;
            }
        }
        crate::core::stop_recorded_steps(tx, &attempt, &current.steps)?;
        if !matches!(
            current.state,
            LocalAttemptState::Cleaning | LocalAttemptState::Quarantined
        ) {
            tx.set_state(id, LocalAttemptState::Cleaning)?;
        }
        Ok(current.state)
    });
    match journaled {
        Ok(LocalAttemptState::Quarantined) => handle.set_state(LocalAttemptState::Quarantined),
        Ok(_) => handle.set_state(LocalAttemptState::Cleaning),
        Err(error) => {
            tracing::error!(attempt = %attempt.attempt_id, %error, "could not journal the recovery of an attempt");
        }
    }
    core.events_pending(&attempt.attempt_id);
    finish_cleanup(core, &handle, &[], proven);
    handle.busy.store(false, Ordering::SeqCst);
}

/// Try the cleanup of a quarantined attempt again, on a thread of its own.
/// The caller has set `handle.busy`.
pub(crate) fn retry_cleanup(core: &Arc<Core>, handle: Arc<AttemptHandle>) {
    let thread_core = core.clone();
    let thread_handle = handle.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("bella-clean-{}", short_id(handle.id())))
        .spawn(move || {
            finish_cleanup(&thread_core, &thread_handle, &[], None);
            thread_handle.busy.store(false, Ordering::SeqCst);
        });
    match spawned {
        Ok(thread) => core.adopt_thread(thread),
        Err(_) => handle.busy.store(false, Ordering::SeqCst),
    }
}

/// The random tail of an attempt id: short enough for a workspace path.
fn short_id(id: &AttemptId) -> &str {
    let text = id.as_str();
    &text[text.len().saturating_sub(12)..]
}

/// A scope name for an attempt: unique per attempt, `[a-z0-9_-]` only.
fn scope_name(id: &AttemptId) -> String {
    id.as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn exit_code(exit: &ExitInfo) -> Option<i32> {
    exit.code.or(exit.signal.map(|signal| 128 + signal))
}

/// One pass over what must be true before an attempt is released: its
/// scope stopped, verified empty and removed, its output pipes drained and
/// its workspace gone. `Err` names exactly what is left.
fn cleanup_pass(
    core: &Arc<Core>,
    handle: &AttemptHandle,
    pumps: &[Pump],
    proven: Option<&str>,
) -> Result<ReleaseProof, String> {
    let record = core
        .journal
        .attempt(handle.id())
        .map_err(|error| format!("the journal could not be read: {error}"))?
        .ok_or_else(|| "the attempt is missing from the journal".to_string())?;
    let policy = core.policy();
    let grace = handle
        .stop_grace()
        .unwrap_or(Duration::from_secs(policy.cleanup_grace_secs));

    let mut guard = handle
        .scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut gone = None;
    if guard.is_none()
        && let Some(identity) = &record.scope
    {
        // A scope of this boot that this process holds no handle to.
        match core.containment.adopt(identity) {
            Ok(Some(scope)) => *guard = Some(scope),
            Ok(None) => gone = Some(format!("scope `{}` no longer exists", identity.name)),
            Err(error) => {
                return Err(format!(
                    "scope `{}` could not be re-adopted: {error}",
                    identity.name
                ));
            }
        }
    }
    let method = match guard.as_mut() {
        Some(scope) => {
            let name = scope.identity().name;
            match bella_contain::stop_scope(scope.as_mut(), grace, core.tuning.kill_wait) {
                Ok(Membership::Empty) => {}
                Ok(Membership::Alive(count)) => {
                    return Err(format!(
                        "{count} process(es) of the attempt are still alive in scope `{name}` after a forced kill"
                    ));
                }
                Ok(Membership::Unknown) => {
                    return Err(format!(
                        "the containment adapter cannot tell whether scope `{name}` is empty"
                    ));
                }
                Err(error) => {
                    return Err(format!("scope `{name}` could not be stopped: {error}"));
                }
            }
            if let Ok(usage) = scope.usage() {
                handle.record_usage(usage);
            }
            match guard.take() {
                Some(scope) => scope
                    .destroy()
                    .map_err(|error| format!("scope `{name}` could not be removed: {error}"))?,
                None => String::new(),
            }
        }
        None => match (gone, proven) {
            (Some(gone), _) => gone,
            (None, Some(proven)) => proven.to_string(),
            (None, None) => "no containment scope was created for the attempt".to_string(),
        },
    };
    drop(guard);

    // With the scope empty every pipe reaches its end.
    let deadline = Instant::now() + REAP_LIMIT;
    for pump in pumps {
        pump.wait_until(deadline);
    }

    if let Some(workspace) = &record.workspace {
        let path = Path::new(workspace);
        bella_materialize::remove_tree(path)
            .map_err(|error| format!("the workspace could not be removed: {error}"))?;
        if path.exists() {
            return Err("the workspace still exists after it was removed".to_string());
        }
    }
    Ok(ReleaseProof {
        scope_empty: true,
        workspace_removed: true,
        method,
    })
}

/// Run a cleanup pass and journal its result: a release with its proof, or
/// a quarantine with the exact thing that is left. A quarantined attempt
/// keeps its reservation; the protection loop calls this again later.
pub(crate) fn finish_cleanup(
    core: &Arc<Core>,
    handle: &Arc<AttemptHandle>,
    pumps: &[Pump],
    proven: Option<&str>,
) {
    let attempt = &handle.attempt;
    let id = handle.id();
    let failure = match cleanup_pass(core, handle, pumps, proven) {
        Ok(proof) => {
            let released = core.tx(|tx| {
                let record = tx
                    .attempt(id)?
                    .ok_or_else(|| JournalError::NotFound(id.clone()))?;
                if record.state == LocalAttemptState::Quarantined {
                    tx.set_state(id, LocalAttemptState::Cleaning)?;
                }
                tx.append_event(
                    attempt,
                    AttemptEvent::Cleanup {
                        state: CleanupState::ProcessesStopped,
                        detail: proof.method.clone(),
                    },
                )?;
                tx.append_event(
                    attempt,
                    AttemptEvent::Cleanup {
                        state: CleanupState::CleanupComplete,
                        detail: "the workspace was removed".to_string(),
                    },
                )?;
                tx.release(attempt, proof.clone())?;
                Ok(())
            });
            match released {
                Ok(()) => {
                    handle.set_quarantine(None);
                    core.released(handle);
                    return;
                }
                Err(error) => format!("the release could not be journaled: {error}"),
            }
        }
        Err(reason) => reason,
    };

    tracing::warn!(attempt = %id, reason = %failure, "cleanup could not be verified; the attempt stays quarantined");
    if handle.quarantine_reason().as_deref() != Some(failure.as_str()) {
        let journaled = core.tx(|tx| {
            let record = tx
                .attempt(id)?
                .ok_or_else(|| JournalError::NotFound(id.clone()))?;
            if record.state == LocalAttemptState::Cleaning {
                tx.set_state(id, LocalAttemptState::Quarantined)?;
            }
            tx.append_event(
                attempt,
                AttemptEvent::Quarantined {
                    reason: failure.clone(),
                },
            )?;
            Ok(())
        });
        if let Err(error) = journaled {
            tracing::error!(attempt = %id, %error, "could not journal a quarantine");
        }
        core.events_pending(id);
    }
    handle.set_quarantine(Some(failure));
    handle.set_state(LocalAttemptState::Quarantined);
    core.heartbeat_wake.notify_one();
}

struct Service {
    name: String,
    child: Box<dyn Child>,
    alive: bool,
}

/// How waiting for a foreground process ended.
enum End {
    Exit(ExitInfo),
    Stop,
    StepTimeout(u64),
    JobTimeout,
    WaitFailed(String),
}

/// The run of one attempt.
struct Run<'a> {
    core: &'a Arc<Core>,
    handle: &'a Arc<AttemptHandle>,
    offer: Arc<Offer>,
    attempt: AttemptRef,
    policy: MachinePolicy,
    logs: Option<Arc<AttemptLogs>>,
    workspace: PathBuf,
    src: PathBuf,
    tmp: PathBuf,
    /// The source set is on disk, so steps can run.
    materialized: bool,
    states: BTreeMap<String, StepState>,
    services: Vec<Service>,
    pumps: Vec<Pump>,
    /// The first failure among the steps.
    failure: Option<Outcome>,
    /// `Started` was emitted.
    started: bool,
    /// When the job's own timeout ends.
    job_deadline: Instant,
    monitor: Option<JoinHandle<()>>,
}

impl<'a> Run<'a> {
    fn new(core: &'a Arc<Core>, handle: &'a Arc<AttemptHandle>) -> Self {
        let offer = handle.offer.clone();
        let workspace = core.work_root.join(short_id(handle.id()));
        Self {
            core,
            handle,
            attempt: handle.attempt.clone(),
            policy: core.policy(),
            logs: None,
            src: workspace.join("src"),
            tmp: workspace.join("tmp"),
            workspace,
            materialized: false,
            states: BTreeMap::new(),
            services: Vec::new(),
            pumps: Vec::new(),
            failure: None,
            started: false,
            job_deadline: Instant::now() + Duration::from_secs(offer.spec.timeout_secs),
            monitor: None,
            offer,
        }
    }

    fn id(&self) -> &AttemptId {
        &self.attempt.attempt_id
    }

    fn stopped(&self) -> bool {
        self.handle.stop_reason().is_some()
    }

    fn note(&self, step: &str, text: &str) {
        if let Some(logs) = &self.logs {
            logs.system(step, &format!("[builderella] {text}"));
        }
    }

    /// Why the journal refuses to let the attempt go on, as a stop reason.
    fn authority_reason(&self) -> StopReason {
        let now = self.core.clock.now();
        let past_deadline = self
            .core
            .journal
            .attempt(self.id())
            .ok()
            .flatten()
            .and_then(|record| record.authority)
            .is_some_and(|authority| now >= authority.hard_deadline);
        if past_deadline {
            StopReason::HardDeadline
        } else {
            StopReason::AuthorityLapsed
        }
    }

    /// Run journal operations as one transaction. A refusal because the
    /// authority ended or a stop was requested, and any journal fault,
    /// becomes a stop request; `Err` then tells the caller to do nothing
    /// further.
    fn journal<T>(
        &self,
        work: impl Fn(&JournalTx<'_>) -> Result<T, JournalError>,
    ) -> Result<T, ()> {
        match self.core.tx(work) {
            Ok(value) => {
                self.core.events_pending(self.id());
                Ok(value)
            }
            Err(JournalError::AuthorityExpired { .. }) => {
                self.handle.request_stop(self.authority_reason(), None);
                Err(())
            }
            Err(JournalError::CancelRequested { .. }) => {
                // The stop request that set the flag names the reason; a
                // flag found without one came from an authority check.
                if self.handle.stop_reason().is_none() {
                    self.handle.request_stop(self.authority_reason(), None);
                }
                Err(())
            }
            Err(error) => {
                tracing::error!(attempt = %self.id(), %error, "journal failure in the attempt runner");
                self.handle
                    .request_stop(StopReason::JournalFailure(error.to_string()), None);
                Err(())
            }
        }
    }

    fn set_state(&self, state: LocalAttemptState, event: AttemptEvent) -> Result<(), ()> {
        let (id, attempt) = (self.id(), &self.attempt);
        self.journal(|tx| {
            tx.set_state(id, state)?;
            tx.append_event(attempt, event.clone())?;
            Ok(())
        })?;
        self.handle.set_state(state);
        self.core.heartbeat_wake.notify_one();
        Ok(())
    }

    fn set_step(&mut self, step: &str, state: StepState, code: Option<i32>) -> Result<(), ()> {
        let (id, attempt) = (self.attempt.attempt_id.clone(), self.attempt.clone());
        self.journal(|tx| {
            tx.set_step_state(&id, step, state, code)?;
            tx.append_event(
                &attempt,
                AttemptEvent::StepChanged {
                    step: step.to_string(),
                    state,
                    exit_code: code,
                },
            )?;
            Ok(())
        })?;
        self.states.insert(step.to_string(), state);
        Ok(())
    }

    fn fail(&mut self, outcome: Outcome) {
        if self.failure.is_none() {
            self.failure = Some(outcome);
        }
    }

    /// The grace to give processes before they are killed.
    fn grace(&self) -> Duration {
        self.handle
            .stop_grace()
            .unwrap_or(Duration::from_secs(self.policy.cleanup_grace_secs))
    }

    // ------------------------------------------------------------- prepare

    /// Everything before the first step. `Err` is the outcome of an attempt
    /// that cannot run its steps.
    fn prepare(&mut self) -> Result<(), Outcome> {
        let stop_outcome = |handle: &AttemptHandle| {
            handle
                .stop_reason()
                .map(|reason| reason.outcome())
                .unwrap_or(Outcome::Cancelled)
        };
        if self
            .set_state(LocalAttemptState::Preparing, AttemptEvent::Preparing)
            .is_err()
        {
            return Err(stop_outcome(self.handle));
        }
        let outputs = &self.offer.spec.outputs;
        match self.core.logs.create(&self.attempt, outputs.max_log_bytes) {
            Ok(logs) => self.logs = Some(logs),
            Err(error) => {
                return Err(Outcome::InfraInterrupted {
                    detail: format!("the log spool could not be created: {error}"),
                });
            }
        }
        if let Err(detail) = self.check_spec() {
            return Err(Outcome::Rejected { detail });
        }

        // Workspace: journaled before it exists.
        if self.workspace.exists() {
            self.workspace = self.core.work_root.join(scope_name(self.id()));
            self.src = self.workspace.join("src");
            self.tmp = self.workspace.join("tmp");
            let _ = bella_materialize::remove_tree(&self.workspace);
        }
        let workspace = self.workspace.to_string_lossy().into_owned();
        let id = self.id().clone();
        if self
            .journal(|tx| tx.set_workspace(&id, &workspace))
            .is_err()
        {
            return Err(stop_outcome(self.handle));
        }
        if let Err(error) = std::fs::create_dir_all(&self.tmp) {
            return Err(Outcome::InfraInterrupted {
                detail: format!("the workspace could not be created: {error}"),
            });
        }

        // Scope: journaled before the first spawn.
        let limits = scope_limits(&self.offer.reservation, &self.policy);
        let scope = match self
            .core
            .containment
            .create(&scope_name(self.id()), &limits)
        {
            Ok(scope) => scope,
            Err(error) => {
                return Err(Outcome::InfraInterrupted {
                    detail: format!("a containment scope could not be created: {error}"),
                });
            }
        };
        let identity = scope.identity();
        *self
            .handle
            .scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scope);
        if self.journal(|tx| tx.set_scope(&id, &identity)).is_err() {
            return Err(stop_outcome(self.handle));
        }
        self.handle.mark_started(self.core.clock.now());
        self.monitor = spawn_monitor(self.core, self.handle, &self.workspace);

        self.materialize()?;
        self.materialized = true;
        if self.stopped() {
            return Err(stop_outcome(self.handle));
        }
        Ok(())
    }

    /// Reject specifications this runner cannot execute as written.
    fn check_spec(&self) -> Result<(), String> {
        let steps = &self.offer.spec.steps;
        for (index, step) in steps.iter().enumerate() {
            if !env::valid_step_name(&step.name) {
                return Err(format!(
                    "step name `{}` is not usable: names are 1 to 64 characters of a-z, 0-9, `_` and `-`",
                    step.name.escape_debug()
                ));
            }
            if step.name == SYSTEM_STEP {
                return Err(format!("step name `{SYSTEM_STEP}` is reserved"));
            }
            if steps[..index].iter().any(|other| other.name == step.name) {
                return Err(format!("step name `{}` is used twice", step.name));
            }
            for dependency in &step.depends_on {
                let earlier = steps[..index]
                    .iter()
                    .find(|other| &other.name == dependency);
                match earlier {
                    Some(other) if other.kind == StepKind::Cleanup => {
                        return Err(format!(
                            "step `{}` depends on the cleanup step `{dependency}`",
                            step.name
                        ));
                    }
                    Some(_) => {}
                    None => {
                        return Err(format!(
                            "step `{}` depends on `{dependency}`, which is not an earlier step",
                            step.name
                        ));
                    }
                }
            }
            env::step_cwd(&self.src, step.cwd.as_deref())
                .map_err(|reason| format!("step `{}`: {reason}", step.name))?;
        }
        Ok(())
    }

    fn materialize(&mut self) -> Result<(), Outcome> {
        let unavailable = |detail: String| Outcome::InfraInterrupted { detail };
        let rejected = |detail: String| Outcome::Rejected { detail };
        let cache = self.core.cache.clone();
        let digest = &self.offer.manifest_digest;

        let bytes = match bella_core::store::get_bytes(cache.as_ref(), digest) {
            Ok(bytes) => bytes,
            Err(_) if cache.discard_if_corrupt(digest) => {
                bella_core::store::get_bytes(cache.as_ref(), digest).map_err(|error| {
                    unavailable(format!("the snapshot manifest could not be read: {error}"))
                })?
            }
            Err(StoreError::NotFound(_)) => {
                return Err(unavailable(format!(
                    "the snapshot manifest {digest} is not in the object store"
                )));
            }
            Err(error) => {
                return Err(unavailable(format!(
                    "the snapshot manifest could not be read: {error}"
                )));
            }
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| rejected(format!("the snapshot manifest is not readable: {error}")))?;
        manifest
            .validate()
            .map_err(|error| rejected(format!("the snapshot manifest is invalid: {error}")))?;

        // Pin before the first object is fetched, so a collection that runs
        // meanwhile cannot take what this attempt is about to need.
        let mut needed: Vec<Digest> = manifest.digests().into_iter().collect();
        needed.push(digest.clone());
        self.core.cache.pin(self.id(), needed);

        let case_sensitive = bella_materialize::is_case_sensitive(&self.workspace).unwrap_or(true);
        if let Err(problems) =
            bella_materialize::check_platform(&manifest, self.core.os(), case_sensitive)
        {
            return Err(rejected(describe_incompatibilities(&problems)));
        }

        let mut options = self.core.tuning.materialize.clone();
        if self.offer.reservation.disk_bytes > 0 {
            options.max_total_bytes = options
                .max_total_bytes
                .min(self.offer.reservation.disk_bytes);
        }
        let mut result =
            bella_materialize::materialize(&manifest, cache.as_ref(), &self.src, &options);
        if let Err(error) = &result
            && let Some(damaged) = damaged_object(error)
            && cache.discard_if_corrupt(damaged)
        {
            // The local copy was damaged; fetch it again once.
            self.note(
                SYSTEM_STEP,
                "a cached source object was damaged and is fetched again",
            );
            result = bella_materialize::materialize(&manifest, cache.as_ref(), &self.src, &options);
        }
        match result {
            Ok(report) => {
                self.note(
                    SYSTEM_STEP,
                    &format!(
                        "materialized {} file(s), {} bytes",
                        report.files, report.bytes
                    ),
                );
                Ok(())
            }
            Err(error) => {
                let detail = format!("the snapshot could not be materialized: {error}");
                if cannot_run_as_specified(&error) {
                    Err(rejected(detail))
                } else {
                    Err(unavailable(detail))
                }
            }
        }
    }

    // --------------------------------------------------------------- steps

    /// Start one process of the attempt inside its scope. The scope's
    /// identity is journaled again afterwards, because an adapter may
    /// record the new process in it.
    fn spawn(
        &mut self,
        step: &StepSpec,
        command: &CommandSpec,
        capture: bool,
        watch: Option<(&str, &Arc<AtomicBool>)>,
    ) -> Result<Box<dyn Child>, String> {
        let argv = env::resolve_command(command)?;
        let cwd = env::step_cwd(&self.src, step.cwd.as_deref())?;
        if !cwd.is_dir() {
            return Err(format!(
                "the working directory `{}` does not exist in the snapshot",
                step.cwd.as_deref().unwrap_or(".")
            ));
        }
        let spec = ProcessSpec {
            argv,
            cwd,
            env: env::step_env(
                &self.core.system_env,
                &self.offer,
                &self.src,
                &self.tmp,
                &step.env,
            ),
        };
        let (spawned, identity) = {
            let mut guard = self
                .handle
                .scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let scope = guard
                .as_mut()
                .ok_or_else(|| "the attempt has no containment scope".to_string())?;
            let spawned = scope.spawn(&spec).map_err(|error| error.to_string())?;
            (spawned, scope.identity())
        };
        let id = self.id().clone();
        let _ = self.journal(|tx| tx.set_scope(&id, &identity));
        if !self.started {
            self.started = true;
            let _ = self.core.emit(
                &self.attempt,
                AttemptEvent::Started {
                    scope: identity.clone(),
                },
            );
        }
        let logs = if capture { self.logs.clone() } else { None };
        let watcher = |found: &(&str, &Arc<AtomicBool>)| LogWatch::new(found.0, found.1.clone());
        self.pumps.push(Pump::spawn(
            logs.clone(),
            &step.name,
            LogStream::Stdout,
            spawned.stdout,
            watch.as_ref().map(watcher),
        ));
        self.pumps.push(Pump::spawn(
            logs,
            &step.name,
            LogStream::Stderr,
            spawned.stderr,
            watch.as_ref().map(watcher),
        ));
        Ok(spawned.child)
    }

    /// Wait for the output pipes of the process started last to reach
    /// their end, for at most the drain grace.
    fn drain_last_pumps(&self) {
        let deadline = Instant::now() + self.core.tuning.log_drain_grace;
        for pump in self.pumps.iter().rev().take(2) {
            pump.wait_until(deadline);
        }
    }

    /// Stop every process of the attempt. Services are gone afterwards.
    fn stop_scope(&mut self, grace: Duration) {
        {
            let mut guard = self
                .handle
                .scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(scope) = guard.as_mut()
                && let Err(error) =
                    bella_contain::stop_scope(scope.as_mut(), grace, self.core.tuning.kill_wait)
            {
                tracing::warn!(attempt = %self.id(), %error, "could not stop the attempt's scope");
            }
        }
        let stopped: Vec<String> = self
            .services
            .iter_mut()
            .filter(|service| service.alive)
            .map(|service| {
                reap(service.child.as_mut());
                service.alive = false;
                service.name.clone()
            })
            .collect();
        for name in stopped {
            let _ = self.set_step(&name, StepState::Stopped, None);
        }
    }

    /// Notice services that ended on their own: that fails the attempt.
    fn poll_services(&mut self) {
        let mut ended = Vec::new();
        for service in self.services.iter_mut().filter(|service| service.alive) {
            if let Ok(Some(exit)) = service.child.try_wait() {
                service.alive = false;
                ended.push((service.name.clone(), exit_code(&exit)));
            }
        }
        for (name, code) in ended {
            self.note(&name, "the service exited on its own");
            let _ = self.set_step(&name, StepState::Failed, code);
            self.fail(Outcome::Failed {
                step: Some(name),
                exit_code: code,
            });
        }
    }

    fn dependencies_met(&self, step: &StepSpec) -> bool {
        step.depends_on.iter().all(|dependency| {
            matches!(
                self.states.get(dependency),
                Some(StepState::Succeeded | StepState::Ready)
            )
        })
    }

    fn run_steps(&mut self) {
        // `Running` has no event of its own: `Started` and the first
        // step's change report it.
        let id = self.id().clone();
        let running = self.journal(|tx| tx.set_state(&id, LocalAttemptState::Running));
        if running.is_ok() {
            self.handle.set_state(LocalAttemptState::Running);
            self.core.heartbeat_wake.notify_one();
            let steps = self.offer.spec.steps.clone();
            for step in steps.iter().filter(|step| step.kind != StepKind::Cleanup) {
                if self.stopped() {
                    break;
                }
                if Instant::now() >= self.job_deadline {
                    self.job_timed_out(None);
                    break;
                }
                self.poll_services();
                if !self.dependencies_met(step) {
                    let _ = self.set_step(&step.name, StepState::Skipped, None);
                    continue;
                }
                if step.kind == StepKind::Service {
                    self.start_service(step);
                } else {
                    self.run_task(step, None);
                }
            }
            self.poll_services();
        }
        self.skip_unreached(false);
    }

    /// Give every step that never ran a final state.
    fn skip_unreached(&mut self, cleanup_too: bool) {
        let steps = self.offer.spec.steps.clone();
        for step in &steps {
            let wanted = cleanup_too || step.kind != StepKind::Cleanup;
            if wanted && !self.states.contains_key(&step.name) {
                let _ = self.set_step(&step.name, StepState::Skipped, None);
            }
        }
    }

    fn job_timed_out(&mut self, running: Option<&str>) {
        let timeout = self.offer.spec.timeout_secs;
        self.note(
            running.unwrap_or(SYSTEM_STEP),
            &format!(
                "the job exceeded its timeout of {timeout} s; stopping every process of the attempt"
            ),
        );
        self.fail(Outcome::TimedOut { step: None });
        // Later steps are not started: the job as a whole is over.
        self.job_deadline = Instant::now();
        self.stop_scope(self.grace());
    }

    /// Wait for a foreground process. `budget` is the end of the cleanup
    /// phase when this is a cleanup step, which ignores stop requests.
    fn wait(&mut self, child: &mut dyn Child, step: &StepSpec, budget: Option<Instant>) -> End {
        let started = Instant::now();
        let poll = self.core.tuning.poll_interval;
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => return End::Exit(exit),
                Ok(None) => {}
                Err(error) => return End::WaitFailed(error.to_string()),
            }
            let now = Instant::now();
            if let Some(limit) = step.timeout_secs
                && now >= started + Duration::from_secs(limit)
            {
                return End::StepTimeout(limit);
            }
            match budget {
                Some(end) => {
                    if now >= end {
                        return End::StepTimeout(self.offer.spec.cleanup_timeout_secs);
                    }
                    std::thread::sleep(poll);
                }
                None => {
                    if self.stopped() {
                        return End::Stop;
                    }
                    if now >= self.job_deadline {
                        return End::JobTimeout;
                    }
                    self.handle.sleep(poll);
                }
            }
        }
    }

    /// Run a task, test or cleanup step to completion.
    fn run_task(&mut self, step: &StepSpec, budget: Option<Instant>) {
        let name = step.name.as_str();
        if self.set_step(name, StepState::Running, None).is_err() {
            return;
        }
        let mut child = match self.spawn(step, &step.command, true, None) {
            Ok(child) => child,
            Err(reason) => {
                self.note(name, &format!("the step could not be started: {reason}"));
                let _ = self.set_step(name, StepState::Failed, None);
                if budget.is_none() {
                    self.fail(Outcome::Failed {
                        step: Some(name.to_string()),
                        exit_code: None,
                    });
                }
                return;
            }
        };
        match self.wait(child.as_mut(), step, budget) {
            End::Exit(exit) => {
                self.drain_last_pumps();
                let code = exit_code(&exit);
                if exit.success() {
                    let _ = self.set_step(name, StepState::Succeeded, code);
                } else {
                    let _ = self.set_step(name, StepState::Failed, code);
                    if budget.is_none() {
                        self.fail(Outcome::Failed {
                            step: Some(name.to_string()),
                            exit_code: code,
                        });
                    }
                }
            }
            End::StepTimeout(limit) => {
                self.note(
                    name,
                    &format!("the step exceeded its timeout of {limit} s; stopping every process of the attempt"),
                );
                self.stop_scope(self.grace());
                reap(child.as_mut());
                self.drain_last_pumps();
                let _ = self.set_step(name, StepState::TimedOut, None);
                if budget.is_none() {
                    self.fail(Outcome::TimedOut {
                        step: Some(name.to_string()),
                    });
                }
            }
            End::JobTimeout => {
                self.job_timed_out(Some(name));
                reap(child.as_mut());
                self.drain_last_pumps();
                let _ = self.set_step(name, StepState::Stopped, None);
            }
            End::Stop => {
                self.stop_scope(self.grace());
                reap(child.as_mut());
                self.drain_last_pumps();
                let _ = self.set_step(name, StepState::Stopped, None);
            }
            End::WaitFailed(error) => {
                self.note(
                    name,
                    &format!("the step's process could not be waited for: {error}"),
                );
                self.stop_scope(self.grace());
                let _ = self.set_step(name, StepState::Failed, None);
                if budget.is_none() {
                    self.fail(Outcome::Failed {
                        step: Some(name.to_string()),
                        exit_code: None,
                    });
                }
            }
        }
    }

    /// Start a service and wait for its readiness probe.
    fn start_service(&mut self, step: &StepSpec) {
        let name = step.name.as_str();
        if self.set_step(name, StepState::Running, None).is_err() {
            return;
        }
        let found = Arc::new(AtomicBool::new(false));
        let watch = match step.readiness.as_ref().map(|readiness| &readiness.probe) {
            Some(ReadinessProbe::LogLine { contains }) => Some((contains.as_str(), &found)),
            _ => None,
        };
        let mut child = match self.spawn(step, &step.command, true, watch) {
            Ok(child) => child,
            Err(reason) => {
                self.note(name, &format!("the service could not be started: {reason}"));
                let _ = self.set_step(name, StepState::Failed, None);
                self.fail(Outcome::Failed {
                    step: Some(name.to_string()),
                    exit_code: None,
                });
                return;
            }
        };

        if let Some(readiness) = &step.readiness {
            let deadline = Instant::now() + Duration::from_secs(readiness.timeout_secs);
            let interval = Duration::from_millis(readiness.interval_ms.max(10));
            loop {
                if let Ok(Some(exit)) = child.try_wait() {
                    self.drain_last_pumps();
                    let code = exit_code(&exit);
                    self.note(name, "the service exited before it became ready");
                    let _ = self.set_step(name, StepState::Failed, code);
                    self.fail(Outcome::Failed {
                        step: Some(name.to_string()),
                        exit_code: code,
                    });
                    return;
                }
                let ready = match &readiness.probe {
                    ReadinessProbe::Tcp { port } => readiness::tcp_ready(*port),
                    ReadinessProbe::Http { port, path, status } => {
                        readiness::http_ready(*port, path, *status)
                    }
                    ReadinessProbe::LogLine { .. } => found.load(Ordering::SeqCst),
                    ReadinessProbe::Command { command } => {
                        self.probe_command(step, command, deadline)
                    }
                };
                if ready {
                    break;
                }
                if self.stopped() {
                    self.stop_scope(self.grace());
                    reap(child.as_mut());
                    let _ = self.set_step(name, StepState::Stopped, None);
                    return;
                }
                let now = Instant::now();
                if now >= self.job_deadline {
                    self.job_timed_out(Some(name));
                    reap(child.as_mut());
                    let _ = self.set_step(name, StepState::Stopped, None);
                    return;
                }
                if now >= deadline {
                    self.note(
                        name,
                        &format!(
                            "the service was not ready within {} s; stopping every process of the attempt",
                            readiness.timeout_secs
                        ),
                    );
                    self.stop_scope(self.grace());
                    reap(child.as_mut());
                    let _ = self.set_step(name, StepState::TimedOut, None);
                    self.fail(Outcome::TimedOut {
                        step: Some(name.to_string()),
                    });
                    return;
                }
                self.handle.sleep(interval.min(deadline - now));
            }
        }
        if self.set_step(name, StepState::Ready, None).is_ok() {
            self.services.push(Service {
                name: name.to_string(),
                child,
                alive: true,
            });
        } else {
            // A stop arrived; the scope is stopped by the caller's path.
            self.services.push(Service {
                name: name.to_string(),
                child,
                alive: true,
            });
        }
    }

    /// Run a readiness command inside the scope. Ready means exit status 0.
    fn probe_command(&mut self, step: &StepSpec, command: &CommandSpec, deadline: Instant) -> bool {
        let Ok(mut child) = self.spawn(step, command, false, None) else {
            return false;
        };
        let end = deadline.min(Instant::now() + PROBE_COMMAND_LIMIT);
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => return exit.success(),
                Ok(None) => {}
                Err(_) => return false,
            }
            if self.stopped() || Instant::now() >= end {
                // It ends with the scope; it is not waited for further.
                return false;
            }
            self.handle.sleep(self.core.tuning.poll_interval);
        }
    }

    // --------------------------------------------------------- environment

    /// Keep the services alive as an environment until it is finished,
    /// cancelled, loses its authority or a service ends.
    fn hold_environment(&mut self) {
        let hold = self.offer.spec.environment.is_some()
            && self.failure.is_none()
            && !self.stopped()
            && self.services.iter().any(|service| service.alive);
        if !hold {
            return;
        }
        let steps = &self.offer.spec.steps;
        let endpoints: Vec<Endpoint> = self
            .services
            .iter()
            .filter(|service| service.alive)
            .filter_map(|service| steps.iter().find(|step| step.name == service.name))
            .flat_map(|step| {
                step.ports.iter().map(|port| Endpoint {
                    step: step.name.clone(),
                    host: "127.0.0.1".to_string(),
                    port: *port,
                })
            })
            .collect();
        if self
            .set_state(
                LocalAttemptState::EnvironmentHeld,
                AttemptEvent::EnvironmentReady { endpoints },
            )
            .is_err()
        {
            return;
        }
        let poll = self.core.tuning.poll_interval * 4;
        while !self.stopped() && !self.handle.environment_finished() {
            self.poll_services();
            if self.failure.is_some() {
                break;
            }
            self.handle.sleep(poll);
        }
    }

    // ------------------------------------------------------------- collect

    /// Upload the declared outputs, then announce each one. An upload that
    /// fails is noted in the log and changes nothing else.
    fn collect(&mut self) {
        if self
            .set_state(LocalAttemptState::Collecting, AttemptEvent::Collecting)
            .is_err()
        {
            return;
        }
        let outputs = self.offer.spec.outputs.clone();
        let mut used = 0u64;
        let mut uploaded: BTreeMap<String, (Digest, u64)> = BTreeMap::new();
        let kinds = [
            (ArtifactKind::Artifact, &outputs.artifacts),
            (ArtifactKind::TestReport, &outputs.test_reports),
        ];
        for (kind, patterns) in kinds {
            let (found, errors) = collect::expand(&self.src, patterns, MAX_COLLECTED_FILES);
            for error in errors {
                self.note(
                    SYSTEM_STEP,
                    &format!(
                        "output pattern `{}` was ignored: it {}",
                        error.pattern, error.reason
                    ),
                );
            }
            if found.len() >= MAX_COLLECTED_FILES {
                self.note(
                    SYSTEM_STEP,
                    &format!("only the first {MAX_COLLECTED_FILES} matching files are collected"),
                );
            }
            for file in found {
                if self.stopped() {
                    return;
                }
                let known = uploaded.get(&file.rel).cloned();
                let (digest, size) = match known {
                    Some(known) => known,
                    None => {
                        if used.saturating_add(file.size) > outputs.max_artifact_bytes {
                            self.note(
                                SYSTEM_STEP,
                                &format!(
                                    "`{}` ({} bytes) was not collected: the attempt's artifact allowance of {} bytes would be exceeded",
                                    file.rel, file.size, outputs.max_artifact_bytes
                                ),
                            );
                            continue;
                        }
                        match upload(self.core, &file.path) {
                            Ok(stored) => {
                                used = used.saturating_add(stored.1);
                                uploaded.insert(file.rel.clone(), stored.clone());
                                stored
                            }
                            Err(error) => {
                                self.note(
                                    SYSTEM_STEP,
                                    &format!("`{}` could not be uploaded: {error}", file.rel),
                                );
                                continue;
                            }
                        }
                    }
                };
                // Announced only now that the object is in the store.
                let _ = self.core.emit(
                    &self.attempt,
                    AttemptEvent::ArtifactPublished {
                        path: file.rel,
                        kind,
                        digest,
                        size,
                    },
                );
            }
        }
    }

    // ------------------------------------------------------------- the run

    /// Prepare, run the steps, hold the environment and collect outputs.
    /// Returns the attempt's outcome.
    fn execute(&mut self) -> Outcome {
        if let Err(outcome) = self.prepare() {
            return outcome;
        }
        self.run_steps();
        self.hold_environment();
        // The outcome is decided by what the steps did, or by what stopped
        // them. Nothing after this point changes it.
        let outcome = match self.handle.stop_reason() {
            Some(reason) => reason.outcome(),
            None => self.failure.clone().unwrap_or(Outcome::Succeeded),
        };
        // Services and anything a step left in the background end here.
        self.stop_scope(self.grace());
        if !self.stopped() {
            self.collect();
        }
        outcome
    }

    fn measure(&self) -> (UsageSummary, Option<LimitHits>) {
        let guard = self
            .handle
            .scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hits = None;
        if let Some(scope) = guard.as_ref() {
            if let Ok(usage) = scope.usage() {
                self.handle.record_usage(usage);
            }
            hits = scope.limit_hits().ok().flatten();
        }
        drop(guard);
        (self.handle.usage(), hits)
    }

    /// Report the outcome, run the cleanup steps and release or quarantine
    /// the attempt.
    fn conclude(&mut self, mut outcome: Outcome) {
        let (usage, hits) = self.measure();
        if matches!(outcome, Outcome::Failed { .. })
            && let Some(hits) = hits
            && (hits.memory > 0 || hits.processes > 0)
        {
            // A limit ended it, not the job's own logic.
            let detail = if hits.memory > 0 {
                format!(
                    "the attempt hit its memory limit {} time(s): peak {} bytes against a limit of {} bytes",
                    hits.memory, usage.memory_peak_bytes, self.offer.reservation.memory_bytes
                )
            } else {
                format!(
                    "the attempt hit its process limit {} time(s) with {} process(es) running",
                    hits.processes, usage.processes
                )
            };
            self.note(SYSTEM_STEP, &detail);
            outcome = Outcome::ResourceExhausted { detail };
        }
        if let Outcome::Rejected { detail }
        | Outcome::InfraInterrupted { detail }
        | Outcome::ResourceExhausted { detail } = &outcome
        {
            self.note(SYSTEM_STEP, detail);
        }
        if self.logs.as_ref().is_some_and(|logs| logs.truncated()) {
            tracing::info!(attempt = %self.id(), "the attempt's log was truncated at its allowance");
        }

        let (id, attempt) = (self.id().clone(), self.attempt.clone());
        let journaled = self.core.tx(|tx| {
            if tx.record_outcome(&id, &outcome, &usage)? {
                tx.append_event(
                    &attempt,
                    AttemptEvent::Exited {
                        outcome: outcome.clone(),
                        usage: usage.clone(),
                    },
                )?;
            }
            tx.set_state(&id, LocalAttemptState::Cleaning)?;
            tx.append_event(
                &attempt,
                AttemptEvent::Cleanup {
                    state: CleanupState::WorkerAcknowledged,
                    detail: "cleanup started".to_string(),
                },
            )?;
            Ok(())
        });
        if let Err(error) = journaled {
            tracing::error!(attempt = %id, %error, "could not journal the end of an attempt");
        }
        self.handle.set_state(LocalAttemptState::Cleaning);
        self.core.events_pending(&id);
        self.core.heartbeat_wake.notify_one();

        self.run_cleanup_steps();
        self.skip_unreached(true);

        self.handle.mark_done();
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
        let pumps = std::mem::take(&mut self.pumps);
        finish_cleanup(self.core, self.handle, &pumps, None);
    }

    /// Cleanup steps always run, whatever ended the attempt, within the
    /// job's cleanup timeout.
    fn run_cleanup_steps(&mut self) {
        let steps: Vec<StepSpec> = self
            .offer
            .spec
            .steps
            .iter()
            .filter(|step| step.kind == StepKind::Cleanup)
            .cloned()
            .collect();
        if steps.is_empty() || !self.materialized {
            return;
        }
        let budget = Instant::now() + Duration::from_secs(self.offer.spec.cleanup_timeout_secs);
        for step in &steps {
            if Instant::now() >= budget {
                self.note(
                    &step.name,
                    "the cleanup step was skipped: the job's cleanup timeout was used up",
                );
                break;
            }
            self.run_task(step, Some(budget));
        }
    }
}

/// Collect the exit status of a process whose scope was just stopped.
fn reap(child: &mut dyn Child) {
    let deadline = Instant::now() + REAP_LIMIT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            _ => return,
        }
    }
}

/// Store one regular file in the controller's object store.
fn upload(core: &Core, path: &Path) -> Result<(Digest, u64), String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let regular = file.metadata().map(|meta| meta.is_file()).unwrap_or(false);
    if !regular {
        return Err("it is not a regular file".to_string());
    }
    core.cache
        .remote()
        .put(&mut file, None)
        .map_err(|error| error.to_string())
}

/// The object a materialization error blames, when it blames one.
fn damaged_object(error: &MaterializeError) -> Option<&Digest> {
    match error {
        MaterializeError::DigestMismatch { digest, .. }
        | MaterializeError::ObjectTooLong { digest, .. }
        | MaterializeError::ObjectTooShort { digest, .. }
        | MaterializeError::Store { digest, .. } => Some(digest),
        _ => None,
    }
}

/// Is the error about the snapshot itself, so that no machine like this
/// one could run it, rather than about missing objects or local I/O?
fn cannot_run_as_specified(error: &MaterializeError) -> bool {
    matches!(
        error,
        MaterializeError::InvalidManifest(_)
            | MaterializeError::TooManyEntries { .. }
            | MaterializeError::TooManyBytes { .. }
            | MaterializeError::LinkInPath { .. }
            | MaterializeError::Conflict { .. }
            | MaterializeError::SymlinkEscapes { .. }
            | MaterializeError::SymlinksTooComplex { .. }
            | MaterializeError::UnsafePath { .. }
            | MaterializeError::Incompatible(_)
            | MaterializeError::SymlinksUnsupported { .. }
    )
}

/// Measure the workspace against the scratch disk reservation until the
/// attempt is done. Exceeding it stops the attempt.
fn spawn_monitor(
    core: &Arc<Core>,
    handle: &Arc<AttemptHandle>,
    workspace: &Path,
) -> Option<JoinHandle<()>> {
    let core = core.clone();
    let handle = handle.clone();
    let workspace = workspace.to_path_buf();
    let limit = handle.offer.reservation.disk_bytes;
    std::thread::Builder::new()
        .name(format!("bella-disk-{}", short_id(handle.id())))
        .spawn(move || {
            while !handle.is_done() {
                let used = collect::tree_bytes(&workspace);
                handle.disk_bytes.store(used, Ordering::SeqCst);
                if limit > 0 && used > limit {
                    let reason = StopReason::DiskExhausted { used, limit };
                    let detail = match reason.outcome() {
                        Outcome::ResourceExhausted { detail } => detail,
                        _ => String::new(),
                    };
                    if handle.request_stop(reason, None) {
                        tracing::warn!(attempt = %handle.id(), used, limit, "the workspace outgrew its disk reservation");
                        let _ = core.journal.request_cancel(&handle.attempt, &detail);
                    }
                }
                handle.sleep_until_done(core.tuning.disk_check_interval);
            }
        })
        .ok()
}
