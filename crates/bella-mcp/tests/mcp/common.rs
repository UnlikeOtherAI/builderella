//! A fake fleet and a client that drives `serve` over in-memory pipes.

use bella_core::api::{
    ArtifactView, AttemptView, EnvironmentState, EnvironmentView, ExplainResponse, FleetOverview,
    JobView, LogPage, LogQuery, MachineView, PlanResponse, SessionInfo, StepView, WaitQuery,
    WaitResponse,
};
use bella_core::job::{
    CleanupState, JobPhase, LeaseSpec, Outcome, Priority, QueueReason, QueueReasonCode, StepKind,
};
use bella_core::lease::Lease;
use bella_core::machine::{
    Hardware, Inventory, LocalControl, MachineLifecycle, MachinePolicy, MachineProfile, OsInfo,
};
use bella_core::platform::Platform;
use bella_core::protocol::{ArtifactKind, Endpoint, StepState};
use bella_core::resources::{Enforcement, EnforcementReport, ResourceEnvelope, ResourceVector};
use bella_core::snapshot::Consistency;
use bella_core::{
    ApiError, ArtifactId, AttemptId, Digest, EnvironmentId, ErrorCode, FleetId, JobId, MachineId,
    PrincipalId, ProjectId, SessionId, SnapshotId, UnixMillis,
};
use bella_mcp::{FleetOps, PlanArgs, ServerOptions, SubmitArgs, SubmitOutcome, serve};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a test waits for something that should happen at once.
pub const PATIENCE: Duration = Duration::from_secs(20);

pub const GIB: u64 = 1 << 30;

// ------------------------------------------------------------ fixtures

pub fn snapshot_id() -> SnapshotId {
    SnapshotId(Digest::of_bytes(b"snapshot"))
}

pub fn job_view(id: &JobId, phase: JobPhase) -> JobView {
    JobView {
        id: id.clone(),
        project_id: ProjectId::derive(b"project"),
        project_name: "demo".into(),
        principal_id: PrincipalId::derive(b"principal"),
        session: None,
        snapshot_id: snapshot_id(),
        recipe: "test".into(),
        priority: Priority::Normal,
        phase,
        outcome: None,
        cleanup: CleanupState::NotStarted,
        cancel_requested: false,
        queue_reason: (phase == JobPhase::Queued).then(|| {
            QueueReason::new(
                QueueReasonCode::WaitingForCapacity,
                "all compatible machines are busy",
            )
        }),
        envelope: ResourceEnvelope::default(),
        created_at: UnixMillis(1_700_000_000_000),
        updated_at: UnixMillis(1_700_000_001_000),
        attempts: Vec::new(),
        environment_id: None,
        last_event: 1,
        spec: None,
    }
}

pub fn attempt_view(generation: u32, machine: &str, phase: JobPhase) -> AttemptView {
    AttemptView {
        id: AttemptId::derive(format!("attempt-{generation}").as_bytes()),
        generation,
        machine_id: MachineId::derive(machine.as_bytes()),
        machine_name: machine.into(),
        phase,
        outcome: None,
        cleanup: CleanupState::NotStarted,
        uncertain: false,
        start_committed: true,
        reserved: ResourceVector::new(2000, 4 * GIB, 0),
        usage: None,
        offered_at: UnixMillis(1_700_000_000_500),
        started_at: None,
        exited_at: None,
        released_at: None,
        steps: Vec::new(),
    }
}

pub fn step_view(name: &str, state: StepState, exit_code: Option<i32>) -> StepView {
    StepView {
        name: name.into(),
        kind: StepKind::Task,
        state,
        exit_code,
        started_at: None,
        finished_at: None,
    }
}

pub fn machine_view(name: &str, platform: &str) -> MachineView {
    let platform: Platform = platform.parse().unwrap();
    let policy = MachinePolicy::defaults_for(MachineProfile::DedicatedBuilder);
    MachineView {
        id: MachineId::derive(name.as_bytes()),
        name: name.into(),
        lifecycle: MachineLifecycle::Ready,
        connected: true,
        flags: Vec::new(),
        policy,
        inventory: Inventory {
            revision: 1,
            hardware: Hardware {
                logical_cpus: 8,
                physical_cpus: Some(8),
                cpu_model: "test cpu".into(),
                memory_bytes: 32 * GIB,
                disks: Vec::new(),
                gpus: Vec::new(),
            },
            os: OsInfo {
                platform,
                os_version: "15.1".into(),
                kernel: String::new(),
                hostname: String::new(),
            },
            capabilities: Vec::new(),
            executors: Vec::new(),
            worker_version: "0.1.0".into(),
        },
        telemetry: None,
        last_seen_at: None,
        local_control: LocalControl::default(),
        managed_ceiling: ResourceVector::new(8000, 24 * GIB, u64::MAX),
        effective_budget: Some(ResourceVector::new(6000, 20 * GIB, u64::MAX)),
        reserved: ResourceVector::new(2000, 4 * GIB, 0),
        enforcement: EnforcementReport {
            cpu: Enforcement::Advisory,
            memory: Enforcement::Advisory,
            process_tree: Enforcement::Hard,
            process_count: Enforcement::None,
            disk: Enforcement::None,
            verified: true,
            notes: Vec::new(),
        },
        attempts: Vec::new(),
    }
}

pub fn fleet_overview(machines: Vec<MachineView>) -> FleetOverview {
    FleetOverview {
        fleet_id: FleetId::derive(b"fleet"),
        controller_version: "0.1.0".into(),
        controller_epoch: 3,
        server_time: UnixMillis(1_700_000_000_000),
        reconciling: false,
        machines,
        queued: 2,
        running: 1,
        environments: 1,
    }
}

pub fn environment_view(id: &EnvironmentId, state: EnvironmentState) -> EnvironmentView {
    let lease = Lease::start(LeaseSpec::default(), UnixMillis(1_700_000_000_000));
    EnvironmentView {
        id: id.clone(),
        job_id: JobId::derive(b"env-job"),
        attempt_id: AttemptId::derive(b"env-attempt"),
        project_id: ProjectId::derive(b"project"),
        machine_id: MachineId::derive(b"mini"),
        machine_name: "mini".into(),
        snapshot_id: snapshot_id(),
        state,
        endpoints: vec![Endpoint {
            step: "web".into(),
            host: "mini.fleet.test".into(),
            port: 8080,
        }],
        effective_deadline: lease.effective_deadline(),
        lease,
        reserved: ResourceVector::new(1500, 2 * GIB, 0),
        ended_reason: None,
    }
}

pub fn artifact_view(job: &JobId, path: &str, content: &[u8], attempt: &str) -> ArtifactView {
    ArtifactView {
        id: ArtifactId::derive(format!("{path}{attempt}").as_bytes()),
        job_id: job.clone(),
        attempt_id: AttemptId::parse(attempt).unwrap(),
        snapshot_id: snapshot_id(),
        path: path.into(),
        kind: ArtifactKind::Artifact,
        digest: Digest::of_bytes(content),
        size: content.len() as u64,
    }
}

pub const ATTEMPT_1: &str = "att_00000000000000000000000000000001";
pub const ATTEMPT_2: &str = "att_00000000000000000000000000000002";

pub fn session() -> SessionInfo {
    SessionInfo {
        id: SessionId::derive(b"session"),
        label: Some("test session".into()),
        agent: Some("test".into()),
        origin_machine: None,
        thread_ref: None,
    }
}

pub fn options(root: &Path) -> ServerOptions {
    let mut options = ServerOptions::new(vec![root.to_path_buf()], session());
    options.dashboard_url = Some("https://fleet.test/dashboard/".into());
    options
}

// ---------------------------------------------------------- fake fleet

#[derive(Default)]
struct State {
    calls: Vec<String>,
    jobs: HashMap<JobId, JobView>,
    keys: HashMap<String, JobId>,
    submits: Vec<SubmitArgs>,
    plans: Vec<PlanArgs>,
    log_queries: Vec<LogQuery>,
    wait_queries: Vec<WaitQuery>,
    errors: HashMap<&'static str, ApiError>,
    held: Vec<&'static str>,
    inside: HashMap<&'static str, usize>,
    waiting: usize,
    log_page: Option<LogPage>,
    artifacts: Vec<ArtifactView>,
    contents: BTreeMap<String, Vec<u8>>,
    fetch_outside: Option<PathBuf>,
    environment: Option<EnvironmentView>,
    explain: Option<ExplainResponse>,
    plan: Option<PlanResponse>,
    fleet: Option<FleetOverview>,
    excluded: Vec<String>,
    warnings: Vec<String>,
}

/// An in-memory fleet that records every call made to it.
#[derive(Default)]
pub struct FakeFleet {
    state: Mutex<State>,
    changed: Condvar,
}

impl FakeFleet {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Record the call, park it while the operation is held, and fail it if
    /// a failure was arranged.
    fn enter(&self, op: &'static str) -> Result<std::sync::MutexGuard<'_, State>, ApiError> {
        let mut state = self.state();
        state.calls.push(op.to_string());
        *state.inside.entry(op).or_default() += 1;
        self.changed.notify_all();
        while state.held.contains(&op) {
            state = self.changed.wait(state).unwrap();
        }
        match state.errors.get(op) {
            Some(error) => Err(error.clone()),
            None => Ok(state),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    pub fn count(&self, op: &str) -> usize {
        self.state().calls.iter().filter(|call| *call == op).count()
    }

    pub fn fail(&self, op: &'static str, code: ErrorCode, message: &str) {
        self.state()
            .errors
            .insert(op, ApiError::new(code, message));
    }

    /// Make every call of `op` block until `release`.
    pub fn hold(&self, op: &'static str) {
        self.state().held.push(op);
    }

    pub fn release(&self, op: &'static str) {
        self.state().held.retain(|held| *held != op);
        self.changed.notify_all();
    }

    /// Block until a call of `op` has reached the fleet.
    pub fn wait_until_inside(&self, op: &'static str) {
        let deadline = Instant::now() + PATIENCE;
        let mut state = self.state();
        while state.inside.get(op).copied().unwrap_or(0) == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no call reached FleetOps::{op}");
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }

    pub fn add_job(&self, job: JobView) -> JobId {
        let id = job.id.clone();
        self.state().jobs.insert(id.clone(), job);
        self.changed.notify_all();
        id
    }

    pub fn job_now(&self, id: &JobId) -> JobView {
        self.state().jobs[id].clone()
    }

    /// Change a job as the fleet would and wake every waiter.
    pub fn update_job(&self, id: &JobId, change: impl FnOnce(&mut JobView)) {
        let mut state = self.state();
        let job = state.jobs.get_mut(id).unwrap();
        change(job);
        job.last_event += 1;
        drop(state);
        self.changed.notify_all();
    }

    pub fn finish_job(&self, id: &JobId, outcome: Outcome) {
        self.update_job(id, |job| {
            job.phase = JobPhase::Finished;
            job.outcome = Some(outcome);
            job.cleanup = CleanupState::Released;
            job.queue_reason = None;
        });
    }

    /// Block until `count` calls are inside `wait`.
    pub fn wait_until_waiting(&self, count: usize) {
        let deadline = Instant::now() + PATIENCE;
        let mut state = self.state();
        while state.waiting < count {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no call reached FleetOps::wait");
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }

    pub fn submits(&self) -> Vec<SubmitArgs> {
        self.state().submits.clone()
    }

    pub fn plans(&self) -> Vec<PlanArgs> {
        self.state().plans.clone()
    }

    pub fn log_queries(&self) -> Vec<LogQuery> {
        self.state().log_queries.clone()
    }

    pub fn wait_queries(&self) -> Vec<WaitQuery> {
        self.state().wait_queries.clone()
    }

    pub fn set_log_page(&self, page: LogPage) {
        self.state().log_page = Some(page);
    }

    pub fn add_artifact(&self, artifact: ArtifactView, content: &[u8]) {
        let mut state = self.state();
        state
            .contents
            .insert(artifact.path.clone(), content.to_vec());
        state.artifacts.push(artifact);
    }

    /// Make `fetch_artifact` write to this file instead of into `dest`.
    pub fn fetch_outside(&self, file: PathBuf) {
        self.state().fetch_outside = Some(file);
    }

    pub fn set_environment(&self, view: EnvironmentView) {
        self.state().environment = Some(view);
    }

    pub fn set_explain(&self, response: ExplainResponse) {
        self.state().explain = Some(response);
    }

    pub fn set_plan(&self, response: PlanResponse) {
        self.state().plan = Some(response);
    }

    pub fn set_fleet(&self, overview: FleetOverview) {
        self.state().fleet = Some(overview);
    }

    pub fn set_submit_extras(&self, excluded: Vec<String>, warnings: Vec<String>) {
        let mut state = self.state();
        state.excluded = excluded;
        state.warnings = warnings;
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError::not_found(what)
}

impl FleetOps for FakeFleet {
    fn plan(&self, req: PlanArgs) -> Result<PlanResponse, ApiError> {
        let mut state = self.enter("plan")?;
        state.plans.push(req);
        Ok(state.plan.clone().unwrap_or(PlanResponse {
            feasible: true,
            placements: Vec::new(),
            issues: Vec::new(),
        }))
    }

    fn submit(&self, req: SubmitArgs) -> Result<SubmitOutcome, ApiError> {
        let mut state = self.enter("submit")?;
        let id = match state.keys.get(&req.idempotency_key) {
            Some(id) => id.clone(),
            None => {
                let id = JobId::generate();
                let mut job = job_view(&id, JobPhase::Queued);
                job.recipe = req.recipe.clone().unwrap_or_else(|| "command".into());
                job.session = Some(req.session.clone());
                state.keys.insert(req.idempotency_key.clone(), id.clone());
                state.jobs.insert(id.clone(), job);
                id
            }
        };
        state.submits.push(req);
        Ok(SubmitOutcome {
            job: state.jobs[&id].clone(),
            snapshot_id: snapshot_id(),
            consistency: Consistency::BestEffort,
            excluded_secrets: state.excluded.clone(),
            warnings: state.warnings.clone(),
        })
    }

    fn job(&self, id: &JobId) -> Result<JobView, ApiError> {
        let state = self.enter("job")?;
        state.jobs.get(id).cloned().ok_or_else(|| not_found("job"))
    }

    fn wait(&self, id: &JobId, query: WaitQuery) -> Result<WaitResponse, ApiError> {
        let mut state = self.enter("wait")?;
        state.wait_queries.push(query.clone());
        let deadline = Instant::now() + Duration::from_millis(query.timeout_ms.unwrap_or(0));
        state.waiting += 1;
        self.changed.notify_all();
        let result = loop {
            let Some(job) = state.jobs.get(id) else {
                break Err(not_found("job"));
            };
            let ready = match query.after_event {
                Some(seen) => job.last_event > seen,
                None => job.outcome.is_some(),
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if ready || left.is_zero() {
                break Ok(WaitResponse {
                    job: job.clone(),
                    timed_out: !ready,
                });
            }
            state = self.changed.wait_timeout(state, left).unwrap().0;
        };
        state.waiting -= 1;
        result
    }

    fn logs(&self, _id: &JobId, query: LogQuery) -> Result<LogPage, ApiError> {
        let mut state = self.enter("logs")?;
        state.log_queries.push(query);
        Ok(state.log_page.clone().unwrap_or(LogPage {
            entries: Vec::new(),
            next_cursor: "c0".into(),
            truncated: false,
            complete: true,
        }))
    }

    fn cancel(&self, id: &JobId) -> Result<JobView, ApiError> {
        let mut state = self.enter("cancel")?;
        let job = state.jobs.get_mut(id).ok_or_else(|| not_found("job"))?;
        job.cancel_requested = true;
        job.cleanup = CleanupState::Requested;
        job.last_event += 1;
        let view = job.clone();
        drop(state);
        self.changed.notify_all();
        Ok(view)
    }

    fn artifacts(&self, _id: &JobId) -> Result<Vec<ArtifactView>, ApiError> {
        let state = self.enter("artifacts")?;
        Ok(state.artifacts.clone())
    }

    fn fetch_artifact(&self, _id: &JobId, path: &str, dest: &Path) -> Result<PathBuf, ApiError> {
        let state = self.enter("fetch_artifact")?;
        let content = state
            .contents
            .get(path)
            .cloned()
            .ok_or_else(|| not_found("artifact"))?;
        let file = match &state.fetch_outside {
            Some(file) => file.clone(),
            None => path.split('/').fold(dest.to_path_buf(), |dir, part| dir.join(part)),
        };
        drop(state);
        let io_error = |error: io::Error| ApiError::new(ErrorCode::Conflict, error.to_string());
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
            .map_err(io_error)?;
        out.write_all(&content).map_err(io_error)?;
        Ok(file)
    }

    fn environment(&self, _id: &EnvironmentId) -> Result<EnvironmentView, ApiError> {
        let state = self.enter("environment")?;
        state
            .environment
            .clone()
            .ok_or_else(|| not_found("environment"))
    }

    fn renew_environment(&self, _id: &EnvironmentId) -> Result<EnvironmentView, ApiError> {
        let mut state = self.enter("renew_environment")?;
        let view = state
            .environment
            .as_mut()
            .ok_or_else(|| not_found("environment"))?;
        view.lease.lease_until = view.lease.lease_until.plus_secs(600);
        view.effective_deadline = view.effective_deadline.plus_secs(600);
        Ok(view.clone())
    }

    fn finish_environment(&self, _id: &EnvironmentId) -> Result<EnvironmentView, ApiError> {
        let mut state = self.enter("finish_environment")?;
        let view = state
            .environment
            .as_mut()
            .ok_or_else(|| not_found("environment"))?;
        view.state = EnvironmentState::Finishing;
        Ok(view.clone())
    }

    fn explain(&self, _id: &JobId) -> Result<ExplainResponse, ApiError> {
        let state = self.enter("explain")?;
        state.explain.clone().ok_or_else(|| not_found("job"))
    }

    fn fleet(&self) -> Result<FleetOverview, ApiError> {
        let state = self.enter("fleet")?;
        Ok(state
            .fleet
            .clone()
            .unwrap_or_else(|| fleet_overview(Vec::new())))
    }
}

// --------------------------------------------------------------- pipes

/// The server's input: bytes arrive in whatever pieces the test sends.
struct PipeReader {
    source: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.pending.is_empty() {
            match self.source.recv() {
                Ok(bytes) => self.pending = bytes,
                // Every sender is gone: end of input.
                Err(_) => return Ok(0),
            }
        }
        let count = buffer.len().min(self.pending.len());
        buffer[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        Ok(count)
    }
}

/// The server's output. A dropped receiver behaves like a closed pipe.
struct PipeWriter {
    sink: Sender<Vec<u8>>,
}

impl Write for PipeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.sink
            .send(bytes.to_vec())
            .map(|()| bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client went away"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A client connected to a running server.
pub struct Client {
    input: Option<Sender<Vec<u8>>>,
    output: Option<Receiver<Vec<u8>>>,
    buffered: Vec<u8>,
    server: Option<JoinHandle<io::Result<()>>>,
    next_id: i64,
    stash: Vec<Value>,
    schemas: Option<HashMap<String, Value>>,
    /// `_meta` attached to every request made through `request`, for the
    /// stateless revision.
    pub meta: Option<Value>,
}

impl Client {
    pub fn start(fleet: Arc<FakeFleet>, options: ServerOptions) -> Self {
        let (input, source) = mpsc::channel();
        let (sink, output) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let reader = BufReader::new(PipeReader {
                source,
                pending: Vec::new(),
            });
            serve(reader, PipeWriter { sink }, fleet, options)
        });
        Self {
            input: Some(input),
            output: Some(output),
            buffered: Vec::new(),
            server: Some(server),
            next_id: 0,
            stash: Vec::new(),
            schemas: None,
            meta: None,
        }
    }

    /// A server that has completed the handshake for `version`.
    pub fn initialized(fleet: Arc<FakeFleet>, options: ServerOptions, version: &str) -> Self {
        let mut client = Self::start(fleet, options);
        let reply = client.initialize(version);
        assert_eq!(reply["result"]["protocolVersion"], version, "{reply}");
        client
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) {
        self.input
            .as_ref()
            .expect("input already closed")
            .send(bytes.to_vec())
            .expect("the server stopped reading");
    }

    pub fn send(&mut self, message: &Value) {
        let mut bytes = serde_json::to_vec(message).unwrap();
        bytes.push(b'\n');
        self.send_bytes(&bytes);
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next line of output within `limit`, parsed. Everything the
    /// server writes must be one JSON-RPC message per line.
    pub fn next_within(&mut self, limit: Duration) -> Option<Value> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffered.drain(..=end).collect();
                let text = std::str::from_utf8(&line[..end]).expect("output is not UTF-8");
                let message: Value = serde_json::from_str(text)
                    .unwrap_or_else(|error| panic!("output line is not JSON ({error}): {text}"));
                let messages: Vec<&Value> = match &message {
                    Value::Array(items) => items.iter().collect(),
                    other => vec![other],
                };
                for item in messages {
                    assert_eq!(item["jsonrpc"], "2.0", "not a JSON-RPC message: {text}");
                    assert!(
                        item.get("result").is_some() != item.get("error").is_some(),
                        "neither a result nor an error: {text}"
                    );
                }
                return Some(message);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.output.as_ref()?.recv_timeout(left) {
                Ok(bytes) => self.buffered.extend_from_slice(&bytes),
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    assert!(self.buffered.is_empty(), "output ended in a partial line");
                    return None;
                }
            }
        }
    }

    /// The next message, whatever it is.
    pub fn next(&mut self) -> Value {
        if !self.stash.is_empty() {
            return self.stash.remove(0);
        }
        self.next_within(PATIENCE)
            .expect("the server sent nothing")
    }

    /// The response to request `id`; other messages are kept for later.
    pub fn response(&mut self, id: &Value) -> Value {
        if let Some(index) = self.stash.iter().position(|m| m.get("id") == Some(id)) {
            return self.stash.remove(index);
        }
        loop {
            let message = self
                .next_within(PATIENCE)
                .unwrap_or_else(|| panic!("no response to request {id}"));
            if message.get("id") == Some(id) {
                return message;
            }
            self.stash.push(message);
        }
    }

    /// Send a request without waiting for its response. Returns its id.
    pub fn begin(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        let mut params = params;
        if let (Some(meta), Some(object)) = (&self.meta, params.as_object_mut()) {
            object.insert("_meta".into(), meta.clone());
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.begin(method, params);
        self.response(&id)
    }

    pub fn initialize(&mut self, version: &str) -> Value {
        let reply = self.request(
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1.0.0"}
            }),
        );
        self.notify("notifications/initialized", json!({}));
        reply
    }

    pub fn begin_call(&mut self, tool: &str, arguments: Value) -> Value {
        self.begin("tools/call", json!({"name": tool, "arguments": arguments}))
    }

    /// Call a tool and return the `result` object, whether or not the tool
    /// reported an error. A protocol error fails the test.
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(reply.get("error").is_none(), "protocol error from {tool}: {reply}");
        let result = reply["result"].clone();
        assert!(result["isError"].is_boolean(), "{result}");
        assert_eq!(result["content"][0]["type"], "text", "{result}");
        result
    }

    /// Call a tool that must succeed. The structured result is checked
    /// against the output schema the server published for the tool.
    pub fn call_ok(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], false, "{tool} failed: {}", text(&result));
        let schema = self.output_schema(tool);
        if let Err(problem) = check_schema(&schema, &result["structuredContent"], "result") {
            panic!("{tool}: structured result does not match its outputSchema: {problem}\n{result}");
        }
        result
    }

    /// Call a tool that must report an error, and return the error text.
    pub fn call_err(&mut self, tool: &str, arguments: Value) -> String {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], true, "{tool} should have failed: {result}");
        assert!(result.get("structuredContent").is_none(), "{result}");
        text(&result)
    }

    fn output_schema(&mut self, tool: &str) -> Value {
        if self.schemas.is_none() {
            let reply = self.request("tools/list", json!({}));
            let mut schemas = HashMap::new();
            for entry in reply["result"]["tools"].as_array().unwrap() {
                schemas.insert(
                    entry["name"].as_str().unwrap().to_string(),
                    entry["outputSchema"].clone(),
                );
            }
            self.schemas = Some(schemas);
        }
        self.schemas.as_ref().unwrap()[tool].clone()
    }

    /// Stop reading the server's output, as a client that went away would.
    pub fn drop_output(&mut self) {
        self.output = None;
    }

    /// Close the input and wait for `serve` to return. Returns its result
    /// and every message it still wrote.
    pub fn close(mut self) -> (io::Result<()>, Vec<Value>) {
        self.input = None;
        let server = self.server.take().unwrap();
        let started = Instant::now();
        while !server.is_finished() {
            assert!(started.elapsed() < PATIENCE, "serve did not return after end of input");
            std::thread::sleep(Duration::from_millis(5));
        }
        let result = server.join().expect("serve panicked");
        let mut rest = std::mem::take(&mut self.stash);
        while let Some(message) = self.next_within(Duration::from_millis(200)) {
            rest.push(message);
        }
        (result, rest)
    }
}

/// The text content of a tool result.
pub fn text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

pub fn structured(result: &Value) -> &Map<String, Value> {
    result["structuredContent"]
        .as_object()
        .unwrap_or_else(|| panic!("no structured content: {result}"))
}

// ------------------------------------------------------- schema checks

fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "null" => value.is_null(),
        other => panic!("unknown schema type `{other}`"),
    }
}

/// A small, independent JSON Schema check covering the keywords the tools
/// publish. Unknown keywords fail, so a schema cannot promise more than is
/// checked here.
pub fn check_schema(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let schema = schema
        .as_object()
        .ok_or_else(|| format!("{at}: the schema is not an object"))?;
    for (keyword, rule) in schema {
        let ok = match keyword.as_str() {
            "description" | "title" | "default" => true,
            "type" => match rule {
                Value::String(name) => is_type(value, name),
                Value::Array(names) => names.iter().any(|n| is_type(value, n.as_str().unwrap())),
                _ => return Err(format!("{at}: bad `type`")),
            },
            "enum" => rule.as_array().unwrap().contains(value),
            "required" => match value.as_object() {
                Some(object) => {
                    for name in rule.as_array().unwrap() {
                        if !object.contains_key(name.as_str().unwrap()) {
                            return Err(format!("{at}: `{name}` is required"));
                        }
                    }
                    true
                }
                None => true,
            },
            "properties" => {
                if let Some(object) = value.as_object() {
                    for (name, sub) in rule.as_object().unwrap() {
                        if let Some(child) = object.get(name) {
                            check_schema(sub, child, &format!("{at}.{name}"))?;
                        }
                    }
                }
                true
            }
            "additionalProperties" => {
                if let Some(object) = value.as_object() {
                    let known = schema.get("properties").and_then(Value::as_object);
                    for (name, child) in object {
                        if known.is_some_and(|known| known.contains_key(name)) {
                            continue;
                        }
                        match rule {
                            Value::Bool(false) => {
                                return Err(format!("{at}: `{name}` is not allowed"));
                            }
                            Value::Bool(true) => {}
                            sub => check_schema(sub, child, &format!("{at}.{name}"))?,
                        }
                    }
                }
                true
            }
            "maxProperties" => value
                .as_object()
                .is_none_or(|object| object.len() as u64 <= rule.as_u64().unwrap()),
            "items" => {
                if let Some(items) = value.as_array() {
                    for (index, item) in items.iter().enumerate() {
                        check_schema(rule, item, &format!("{at}[{index}]"))?;
                    }
                }
                true
            }
            "minItems" => value
                .as_array()
                .is_none_or(|items| items.len() as u64 >= rule.as_u64().unwrap()),
            "maxItems" => value
                .as_array()
                .is_none_or(|items| items.len() as u64 <= rule.as_u64().unwrap()),
            "minLength" => value
                .as_str()
                .is_none_or(|text| text.chars().count() as u64 >= rule.as_u64().unwrap()),
            "maxLength" => value
                .as_str()
                .is_none_or(|text| text.chars().count() as u64 <= rule.as_u64().unwrap()),
            "minimum" => value
                .as_f64()
                .is_none_or(|number| number >= rule.as_f64().unwrap()),
            "maximum" => value
                .as_f64()
                .is_none_or(|number| number <= rule.as_f64().unwrap()),
            other => return Err(format!("{at}: unsupported keyword `{other}`")),
        };
        if !ok {
            return Err(format!("{at}: `{keyword}` is violated by {value}"));
        }
    }
    Ok(())
}

/// Create a directory link at `link` that leads to `target`: a symbolic
/// link on Unix, a junction on Windows (which needs no privilege).
pub fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "could not create a junction");
    }
}
