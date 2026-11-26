//! Shared fixtures: a controller on a loopback port with its own temporary
//! data directory and a manual clock, an HTTP client, and a scripted worker
//! that speaks the real WebSocket protocol.

#![allow(dead_code)]

use bella_controller::{Controller, ControllerConfig, ControllerHandle, Tls, Tuning};
use bella_core::api::{
    API_PREFIX, CreatePairingCodeRequest, CreateTokenRequest, EnrollRequest, EnrollResponse,
    FleetOverview, IDEMPOTENCY_HEADER, JobView, MachineView, PairingCodeResponse,
    RegisterSnapshotRequest, Role, SnapshotView, SubmitJobRequest, TokenResponse,
};
use bella_core::ids::{AttemptId, BootId, CommandId, Digest, JobId, MachineId, SnapshotId};
use bella_core::job::{CommandSpec, JobSpec, Outcome, StepKind, StepSpec};
use bella_core::machine::{Hardware, Inventory, LocalControl, MachineProfile, OsInfo, Telemetry};
use bella_core::platform::{Arch, Os, Platform};
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, AttemptRef, AttemptReport, CommandResult, ControllerCommand,
    ControllerMsg, ExecutionAuthority, Heartbeat, LogChunk, LogStream, Offer, OfferDecision,
    PROTOCOL_VERSION, ReconcileVerdict, ReleaseProof, UsageSummary, WORKER_WS_PATH, WorkerMsg,
};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use bella_core::snapshot::{Consistency, Entry, Manifest, RootManifest, SnapshotMeta};
use bella_core::{ApiError, ManualClock, UnixMillis};
use bella_net::Fingerprint;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const GIB: u64 = 1 << 30;
pub const START: UnixMillis = UnixMillis(1_800_000_000_000);
/// How long a test waits for something the controller should do promptly.
pub const PATIENCE: Duration = Duration::from_secs(20);

/// Intervals short enough for tests. Deadlines stay on the manual clock, so
/// nothing expires unless a test moves it.
pub fn tuning() -> Tuning {
    Tuning {
        scheduler_tick: Duration::from_millis(25),
        dispatch_min_interval: Duration::from_millis(2),
        sweep_interval: Duration::from_millis(25),
        event_poll_interval: Duration::from_millis(50),
        hello_timeout: Duration::from_secs(3),
        ws_ping_interval: Duration::from_secs(1),
        shutdown_grace: Duration::from_secs(3),
        ..Tuning::default()
    }
}

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A running controller with its data directory and clock.
pub struct Harness {
    pub handle: Option<ControllerHandle>,
    pub dir: TempDir,
    pub clock: ManualClock,
    /// The bootstrap administrator token.
    pub admin: String,
    configure: Arc<dyn Fn(&mut ControllerConfig) + Send + Sync>,
}

impl Harness {
    /// Plain HTTP on loopback with the test tuning.
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(
        configure: impl Fn(&mut ControllerConfig) + Send + Sync + 'static,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::at(START);
        let configure: Arc<dyn Fn(&mut ControllerConfig) + Send + Sync> = Arc::new(configure);
        let handle = launch(&dir, &clock, configure.as_ref()).await;
        let admin = handle
            .bootstrap_admin_token
            .clone()
            .expect("the first start creates the admin token");
        Self {
            handle: Some(handle),
            dir,
            clock,
            admin,
            configure,
        }
    }

    pub fn handle(&self) -> &ControllerHandle {
        self.handle.as_ref().expect("the controller is running")
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle().local_addr
    }

    pub fn fingerprint(&self) -> Option<Fingerprint> {
        self.handle().fingerprint
    }

    pub async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().await;
        }
    }

    /// Stop the controller and start a new one on the same data directory.
    pub async fn restart(&mut self) {
        self.stop().await;
        self.handle = Some(launch(&self.dir, &self.clock, self.configure.as_ref()).await);
    }

    pub fn advance(&self, duration: Duration) {
        self.clock.advance(duration);
    }

    pub fn api(&self, token: &str) -> Api {
        Api::new(self.addr(), self.fingerprint(), Some(token.to_string()))
    }

    pub fn anonymous(&self) -> Api {
        Api::new(self.addr(), self.fingerprint(), None)
    }

    pub fn admin_api(&self) -> Api {
        self.api(&self.admin)
    }

    /// Mint a token for a new principal with `role`.
    pub async fn token(&self, role: Role) -> String {
        let response: TokenResponse = self
            .admin_api()
            .post(
                "/tokens",
                &CreateTokenRequest {
                    name: format!("{role:?}").to_lowercase(),
                    role,
                },
            )
            .await;
        response.token
    }

    pub async fn developer(&self) -> Api {
        self.api(&self.token(Role::Developer).await)
    }

    /// Create a pairing code and enroll a machine with it.
    pub async fn enroll(&self, name: &str, cpus: u32, memory_gib: u64) -> Enrolled {
        let pairing: PairingCodeResponse = self
            .admin_api()
            .post(
                "/enroll/pairing-codes",
                &CreatePairingCodeRequest {
                    profile: MachineProfile::DedicatedBuilder,
                    ttl_secs: None,
                },
            )
            .await;
        let inventory = inventory(name, cpus, memory_gib);
        let response: EnrollResponse = self
            .anonymous()
            .post(
                "/enroll",
                &EnrollRequest {
                    code: pairing.code,
                    name: name.to_string(),
                    boot_id: boot(1),
                    inventory: inventory.clone(),
                },
            )
            .await;
        Enrolled {
            machine_id: response.machine_id,
            token: response.machine_token,
            inventory,
        }
    }

    /// Enroll a 16-CPU, 64 GiB builder, connect it and send one heartbeat,
    /// so it can be offered work.
    pub async fn worker(&self, name: &str) -> FakeWorker {
        let enrolled = self.enroll(name, 16, 64).await;
        let (mut worker, _) = FakeWorker::connect(self, &enrolled, boot(1), 1, Vec::new()).await;
        worker.heartbeat().await;
        worker
    }

    pub async fn fleet(&self) -> FleetOverview {
        self.admin_api().get("/fleet").await
    }

    pub async fn machine(&self, id: &MachineId) -> MachineView {
        self.admin_api().get(&format!("/machines/{id}")).await
    }

    pub async fn job(&self, id: &JobId) -> JobView {
        self.admin_api().get(&format!("/jobs/{id}")).await
    }

    /// Poll a job until `done` holds.
    pub async fn job_until(
        &self,
        id: &JobId,
        what: &str,
        done: impl Fn(&JobView) -> bool,
    ) -> JobView {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let job = self.job(id).await;
            if done(&job) {
                return job;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; the job is {job:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll a machine until `done` holds.
    pub async fn machine_until(
        &self,
        id: &MachineId,
        what: &str,
        done: impl Fn(&MachineView) -> bool,
    ) -> MachineView {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let machine = self.machine(id).await;
            if done(&machine) {
                return machine;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; the machine is {machine:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Upload `files`, register them as a snapshot of `project` and return
    /// its id with the manifest.
    pub async fn snapshot(&self, api: &Api, project: &str, files: &[(&str, &[u8])]) -> SnapshotId {
        let mut entries = Vec::new();
        for (path, content) in files {
            let digest = Digest::of_bytes(content);
            api.put_object(&digest, content).await;
            entries.push(Entry::File {
                path: (*path).to_string(),
                digest,
                size: content.len() as u64,
                executable: false,
            });
        }
        let manifest = Manifest::new(vec![RootManifest {
            mount: ".".into(),
            entries,
        }]);
        let view: SnapshotView = api
            .post(
                "/snapshots",
                &RegisterSnapshotRequest {
                    project: project.to_string(),
                    manifest: manifest.clone(),
                    meta: meta(),
                },
            )
            .await;
        assert_eq!(view.id, manifest.id());
        view.id
    }

    /// Register a small snapshot and submit `spec` against it.
    pub async fn submit(&self, api: &Api, project: &str, spec: JobSpec) -> JobView {
        let snapshot_id = self
            .snapshot(api, project, &[("main.rs", b"fn main() {}\n")])
            .await;
        api.post("/jobs", &request(project, snapshot_id, spec))
            .await
    }
}

/// Lift the soft open-file limit to the hard one, once per test process.
/// The tests run many controllers side by side, and macOS gives processes a
/// soft limit of 256 (launchd's default), which they exceed.
fn raise_open_file_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(unix)]
        if let Err(error) = rlimit::increase_nofile_limit(u64::MAX) {
            eprintln!("could not raise the open-file limit: {error}");
        }
    });
}

async fn launch(
    dir: &TempDir,
    clock: &ManualClock,
    configure: &(dyn Fn(&mut ControllerConfig) + Send + Sync),
) -> ControllerHandle {
    raise_open_file_limit();
    let mut config = ControllerConfig::new(dir.path(), "127.0.0.1:0".parse().unwrap());
    config.tls = Tls::Disabled;
    config.clock = Arc::new(clock.clone());
    config.tuning = tuning();
    configure(&mut config);
    Controller::start(config)
        .await
        .expect("the controller starts")
}

pub fn request(project: &str, snapshot_id: SnapshotId, spec: JobSpec) -> SubmitJobRequest {
    SubmitJobRequest {
        project: project.to_string(),
        snapshot_id,
        spec,
        origin_platform: platform(),
        session: None,
    }
}

pub fn meta() -> SnapshotMeta {
    SnapshotMeta {
        consistency: Consistency::BestEffort,
        captured_at: START,
        roots: Vec::new(),
        windows_incompatible: None,
        case_sensitive_only: false,
        has_symlinks: false,
    }
}

pub fn platform() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

pub fn inventory(name: &str, cpus: u32, memory_gib: u64) -> Inventory {
    Inventory {
        revision: 1,
        hardware: Hardware {
            logical_cpus: cpus,
            physical_cpus: None,
            cpu_model: String::new(),
            memory_bytes: memory_gib * GIB,
            disks: Vec::new(),
            gpus: Vec::new(),
        },
        os: OsInfo {
            platform: platform(),
            os_version: "24.04".into(),
            kernel: String::new(),
            hostname: name.into(),
        },
        capabilities: Vec::new(),
        executors: Vec::new(),
        worker_version: "0.1.0".into(),
    }
}

pub fn boot(n: u32) -> BootId {
    BootId::derive(format!("boot-{n}").as_bytes())
}

pub fn step(name: &str) -> StepSpec {
    StepSpec {
        name: name.into(),
        kind: StepKind::Task,
        command: CommandSpec::Argv {
            argv: vec!["make".into(), name.into()],
        },
        cwd: None,
        env: Default::default(),
        depends_on: Vec::new(),
        readiness: None,
        timeout_secs: None,
        resources: None,
        ports: Vec::new(),
    }
}

/// A one-step job asking for one CPU and `memory_gib` of memory.
pub fn spec_with_memory(memory_gib: u64) -> JobSpec {
    JobSpec {
        recipe: "build".into(),
        recipe_digest: Digest::of_bytes(b"recipe"),
        steps: vec![step("compile")],
        envelope: ResourceEnvelope::fixed(ResourceVector::new(1000, memory_gib * GIB, 0)),
        compatibility: Default::default(),
        placement: Default::default(),
        priority: Default::default(),
        retry_class: Default::default(),
        max_attempts: 2,
        timeout_secs: 600,
        cleanup_timeout_secs: 60,
        cloud: Default::default(),
        source_lan_only: false,
        outputs: Default::default(),
        environment: None,
        env: Default::default(),
        disconnected_continuation_secs: 120,
        parallelism: None,
    }
}

pub fn spec() -> JobSpec {
    spec_with_memory(1)
}

pub fn proof() -> ReleaseProof {
    ReleaseProof {
        scope_empty: true,
        workspace_removed: true,
        method: "scripted worker".into(),
    }
}

pub fn exited(outcome: Outcome) -> AttemptEvent {
    AttemptEvent::Exited {
        outcome,
        usage: UsageSummary::default(),
    }
}

pub fn released() -> AttemptEvent {
    AttemptEvent::Released { proof: proof() }
}

/// Standard base64 with padding, for log chunks.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ------------------------------------------------------------------- HTTP

/// A TLS configuration that pins `fingerprint`. The HTTP client is built
/// without a crypto provider of its own, so it always gets one from here,
/// also when the controller under test serves plain HTTP.
pub fn client_tls(fingerprint: Option<Fingerprint>) -> tokio_rustls::rustls::ClientConfig {
    let fingerprint = fingerprint.unwrap_or_else(|| Fingerprint::of_certificate(b"no certificate"));
    let config = bella_net::pinned_client_config(fingerprint).expect("a pinned client config");
    (*config).clone()
}

pub fn http_client(fingerprint: Option<Fingerprint>) -> reqwest::Client {
    reqwest::Client::builder()
        .tls_backend_preconfigured(client_tls(fingerprint))
        .timeout(Duration::from_secs(90))
        .build()
        .expect("an HTTP client")
}

/// An HTTP client for one controller and one token.
#[derive(Clone)]
pub struct Api {
    pub http: reqwest::Client,
    pub base: String,
    pub token: Option<String>,
}

/// A non-2xx answer: its status and the `ApiError` body.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub error: ApiError,
    pub headers: reqwest::header::HeaderMap,
}

impl Api {
    pub fn new(addr: SocketAddr, fingerprint: Option<Fingerprint>, token: Option<String>) -> Self {
        let scheme = if fingerprint.is_some() {
            "https"
        } else {
            "http"
        };
        Self {
            http: http_client(fingerprint),
            base: format!("{scheme}://{addr}"),
            token,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{API_PREFIX}{path}", self.base)
    }

    pub fn builder(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, self.url(path));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Send a request and decode the answer: `Ok` for 2xx, `Err` with the
    /// `ApiError` body otherwise.
    pub async fn send<T: DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<T, Refusal> {
        let response = builder.send().await.expect("the request is answered");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.expect("the body arrives");
        if status.is_success() {
            let text = if bytes.is_empty() {
                &b"null"[..]
            } else {
                &bytes[..]
            };
            Ok(serde_json::from_slice(text).unwrap_or_else(|error| {
                panic!(
                    "undecodable {status} body: {error}: {}",
                    String::from_utf8_lossy(&bytes)
                )
            }))
        } else {
            let error = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                panic!(
                    "a {status} answer must carry an ApiError body: {error}: {}",
                    String::from_utf8_lossy(&bytes)
                )
            });
            Err(Refusal {
                status: status.as_u16(),
                error,
                headers,
            })
        }
    }

    pub async fn try_get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Refusal> {
        self.send(self.builder(reqwest::Method::GET, path)).await
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> T {
        self.try_get(path)
            .await
            .unwrap_or_else(|refusal| panic!("GET {path} was refused: {refusal:?}"))
    }

    pub async fn try_post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, Refusal> {
        self.send(self.builder(reqwest::Method::POST, path).json(body))
            .await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> T {
        self.try_post(path, body)
            .await
            .unwrap_or_else(|refusal| panic!("POST {path} was refused: {refusal:?}"))
    }

    /// `POST` without a body.
    pub async fn try_action<T: DeserializeOwned>(&self, path: &str) -> Result<T, Refusal> {
        self.send(self.builder(reqwest::Method::POST, path)).await
    }

    pub async fn action<T: DeserializeOwned>(&self, path: &str) -> T {
        self.try_action(path)
            .await
            .unwrap_or_else(|refusal| panic!("POST {path} was refused: {refusal:?}"))
    }

    pub async fn try_put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, Refusal> {
        self.send(self.builder(reqwest::Method::PUT, path).json(body))
            .await
    }

    pub async fn try_patch<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, Refusal> {
        self.send(self.builder(reqwest::Method::PATCH, path).json(body))
            .await
    }

    /// Upload raw bytes as an object; panics unless it is stored.
    pub async fn put_object(&self, digest: &Digest, content: &[u8]) {
        let status = self
            .put_bytes(digest, "application/octet-stream", content.to_vec())
            .await
            .unwrap_or_else(|refusal| panic!("PUT object was refused: {refusal:?}"));
        assert_eq!(status, 204);
    }

    /// `PUT /objects/{digest}` with a content type; `Ok` carries the status.
    pub async fn put_bytes(
        &self,
        digest: &Digest,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<u16, Refusal> {
        let response = self
            .builder(reqwest::Method::PUT, &format!("/objects/{digest}"))
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .expect("the request is answered");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.expect("the body arrives");
        if status.is_success() {
            Ok(status.as_u16())
        } else {
            Err(Refusal {
                status: status.as_u16(),
                error: serde_json::from_slice(&bytes).expect("an ApiError body"),
                headers,
            })
        }
    }

    /// A raw response, for routes that do not answer with JSON.
    pub async fn raw(&self, method: reqwest::Method, path: &str) -> reqwest::Response {
        self.builder(method, path)
            .send()
            .await
            .expect("the request is answered")
    }

    pub async fn submit(
        &self,
        request: &SubmitJobRequest,
        key: Option<&str>,
    ) -> Result<JobView, Refusal> {
        let mut builder = self.builder(reqwest::Method::POST, "/jobs").json(request);
        if let Some(key) = key {
            builder = builder.header(IDEMPOTENCY_HEADER, key);
        }
        self.send(builder).await
    }
}

// ----------------------------------------------------------------- worker

#[derive(Clone)]
pub struct Enrolled {
    pub machine_id: MachineId,
    pub token: String,
    pub inventory: Inventory,
}

/// Open the worker WebSocket with `token`, over TLS when `fingerprint` is
/// given. `Err` carries the HTTP status of a refused upgrade.
pub async fn open_socket(
    addr: SocketAddr,
    fingerprint: Option<Fingerprint>,
    token: &str,
) -> Result<WebSocketStream<Box<dyn Io>>, u16> {
    let tcp = TcpStream::connect(addr)
        .await
        .expect("the controller accepts");
    let stream: Box<dyn Io> = match fingerprint {
        None => Box::new(tcp),
        Some(fingerprint) => {
            let config = bella_net::pinned_client_config(fingerprint).unwrap();
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let tls = tokio_rustls::TlsConnector::from(config)
                .connect(name, tcp)
                .await
                .expect("the pinned handshake succeeds");
            Box::new(tls)
        }
    };
    let mut request = format!("ws://{addr}{WORKER_WS_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    match tokio_tungstenite::client_async(request, stream).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(other) => panic!("the WebSocket handshake failed: {other}"),
    }
}

/// A worker driven step by step by a test.
pub struct FakeWorker {
    pub machine_id: MachineId,
    pub token: String,
    pub inventory: Inventory,
    pub boot_id: BootId,
    pub connection_generation: u64,
    /// The epoch announced in `Welcome`.
    pub epoch: u64,
    pub budget: ResourceVector,
    socket: WebSocketStream<Box<dyn Io>>,
    heartbeat_seq: u64,
    event_seq: HashMap<AttemptId, u64>,
    log_offsets: HashMap<(AttemptId, String), u64>,
    /// Messages a `expect_*` call skipped while looking for another.
    pub skipped: Vec<ControllerMsg>,
    clock: ManualClock,
}

impl FakeWorker {
    /// Connect and complete the handshake: `Hello`, then `Welcome` and the
    /// `Reconcile` command, in that order, before anything else.
    pub async fn connect(
        harness: &Harness,
        enrolled: &Enrolled,
        boot_id: BootId,
        connection_generation: u64,
        attempts: Vec<AttemptReport>,
    ) -> (Self, Vec<ReconcileVerdict>) {
        let socket = open_socket(harness.addr(), harness.fingerprint(), &enrolled.token)
            .await
            .expect("the machine token opens the worker socket");
        let mut worker = Self {
            machine_id: enrolled.machine_id.clone(),
            token: enrolled.token.clone(),
            inventory: enrolled.inventory.clone(),
            boot_id: boot_id.clone(),
            connection_generation,
            epoch: 0,
            budget: ResourceVector::new(14_000, 48 * GIB, 100 * GIB),
            socket,
            heartbeat_seq: 0,
            event_seq: HashMap::new(),
            log_offsets: HashMap::new(),
            skipped: Vec::new(),
            clock: harness.clock.clone(),
        };
        worker
            .send(WorkerMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                machine_id: enrolled.machine_id.clone(),
                boot_id,
                connection_generation,
                known_epoch: 0,
                inventory: Box::new(enrolled.inventory.clone()),
                policy_revision: 1,
                local_control: LocalControl::default(),
                attempts,
            })
            .await;
        match worker.recv().await {
            Some(ControllerMsg::Welcome {
                protocol_version,
                controller_epoch,
                ..
            }) => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                worker.epoch = controller_epoch;
            }
            other => panic!("the first message must be Welcome, got {other:?}"),
        }
        let verdicts = match worker.recv().await {
            Some(ControllerMsg::Command { envelope }) => {
                assert_eq!(envelope.controller_epoch, worker.epoch);
                match envelope.command {
                    ControllerCommand::Reconcile { verdicts } => verdicts,
                    other => panic!("the second message must be Reconcile, got {other:?}"),
                }
            }
            other => panic!("the second message must be Reconcile, got {other:?}"),
        };
        (worker, verdicts)
    }

    pub async fn send(&mut self, message: WorkerMsg) {
        let text = serde_json::to_string(&message).unwrap();
        self.socket
            .send(Message::text(text))
            .await
            .expect("the socket takes the frame");
    }

    /// Send a frame without caring whether the socket is still open.
    pub async fn try_send(&mut self, message: WorkerMsg) -> bool {
        let text = serde_json::to_string(&message).unwrap();
        self.socket.send(Message::text(text)).await.is_ok()
    }

    /// The next protocol message, or `None` once the socket is closed.
    /// Panics if nothing arrives within `limit`.
    pub async fn recv_within(&mut self, limit: Duration) -> Option<ControllerMsg> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let frame = tokio::time::timeout_at(deadline, self.socket.next())
                .await
                .expect("timed out waiting for a message from the controller");
            match frame {
                Some(Ok(Message::Text(text))) => {
                    return Some(serde_json::from_str(text.as_str()).expect("a ControllerMsg"));
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                Some(Ok(other)) => panic!("unexpected frame {other:?}"),
            }
        }
    }

    pub async fn recv(&mut self) -> Option<ControllerMsg> {
        self.recv_within(PATIENCE).await
    }

    /// The next message if one arrives within `limit`; `None` on silence.
    /// A closed socket also yields `None`.
    pub async fn poll(&mut self, limit: Duration) -> Option<ControllerMsg> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match tokio::time::timeout_at(deadline, self.socket.next()).await {
                Err(_) => return None,
                Ok(Some(Ok(Message::Text(text)))) => {
                    return Some(serde_json::from_str(text.as_str()).expect("a ControllerMsg"));
                }
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
                Ok(_) => return None,
            }
        }
    }

    /// Read until `pick` accepts a message, remembering what was skipped.
    pub async fn expect<T>(
        &mut self,
        what: &str,
        mut pick: impl FnMut(&ControllerMsg) -> Option<T>,
    ) -> T {
        loop {
            let message = self
                .recv()
                .await
                .unwrap_or_else(|| panic!("the socket closed while waiting for {what}"));
            if let Some(found) = pick(&message) {
                return found;
            }
            self.skipped.push(message);
        }
    }

    /// Read until a command that `pick` accepts arrives.
    pub async fn expect_command<T>(
        &mut self,
        what: &str,
        mut pick: impl FnMut(&ControllerCommand) -> Option<T>,
    ) -> (CommandId, T) {
        self.expect(what, |message| match message {
            ControllerMsg::Command { envelope } => {
                pick(&envelope.command).map(|found| (envelope.command_id.clone(), found))
            }
            _ => None,
        })
        .await
    }

    pub async fn expect_offer(&mut self) -> (CommandId, Offer) {
        self.expect_command("an Offer", |command| match command {
            ControllerCommand::Offer { offer } => Some((**offer).clone()),
            _ => None,
        })
        .await
    }

    pub async fn expect_start(&mut self, attempt: &AttemptRef) -> (CommandId, ExecutionAuthority) {
        let wanted = attempt.clone();
        self.expect_command("a Start", move |command| match command {
            ControllerCommand::Start { attempt, authority } if *attempt == wanted => {
                Some(*authority)
            }
            _ => None,
        })
        .await
    }

    /// Read until the socket closes and return the reason the controller
    /// gave in its `Close` message.
    pub async fn expect_close(&mut self) -> String {
        let mut reason = None;
        while let Some(message) = self.recv().await {
            match message {
                ControllerMsg::Close { reason: given } => reason = Some(given),
                other => self.skipped.push(other),
            }
        }
        reason.expect("the controller says why it closes the connection")
    }

    pub fn heartbeat_message(&mut self) -> Heartbeat {
        self.heartbeat_seq += 1;
        Heartbeat {
            machine_id: self.machine_id.clone(),
            boot_id: self.boot_id.clone(),
            connection_generation: self.connection_generation,
            seq: self.heartbeat_seq,
            sent_at: self.clock.now_millis(),
            inventory_revision: 1,
            policy_revision: 1,
            last_applied_command: None,
            telemetry: Telemetry {
                sampled_at: self.clock.now_millis(),
                memory_total_bytes: self.inventory.hardware.memory_bytes,
                memory_available_bytes: self.inventory.hardware.memory_bytes,
                ..Telemetry::default()
            },
            local_control: LocalControl::default(),
            effective_budget: self.budget.clone(),
            admitting: true,
            flags: Vec::new(),
            attempts: Vec::new(),
        }
    }

    /// Report fresh telemetry and the current budget.
    pub async fn heartbeat(&mut self) {
        let heartbeat = self.heartbeat_message();
        self.send(WorkerMsg::Heartbeat {
            heartbeat: Box::new(heartbeat),
        })
        .await;
    }

    pub async fn reply(&mut self, command_id: &CommandId, offer: &Offer, decision: OfferDecision) {
        self.send(WorkerMsg::OfferReply {
            command_id: command_id.clone(),
            attempt: offer.attempt.clone(),
            decision,
        })
        .await;
    }

    pub async fn accept(&mut self, command_id: &CommandId, offer: &Offer) {
        self.reply(command_id, offer, OfferDecision::Accepted).await;
    }

    pub async fn ack(&mut self, command_id: &CommandId) {
        self.send(WorkerMsg::CommandAck {
            command_id: command_id.clone(),
            result: CommandResult::Applied,
        })
        .await;
    }

    /// Take the next offer through to an acknowledged start.
    pub async fn start_next(&mut self) -> Offer {
        let (offer_id, offer) = self.expect_offer().await;
        self.accept(&offer_id, &offer).await;
        let (start_id, _) = self.expect_start(&offer.attempt).await;
        self.ack(&start_id).await;
        offer
    }

    /// Send one attempt event with the next sequence number, without
    /// waiting for its acknowledgement.
    pub async fn send_event(&mut self, attempt: &AttemptRef, event: AttemptEvent) -> u64 {
        let seq = self
            .event_seq
            .entry(attempt.attempt_id.clone())
            .or_insert(0);
        *seq += 1;
        let seq = *seq;
        self.send_event_at(attempt, seq, event).await;
        seq
    }

    pub async fn send_event_at(&mut self, attempt: &AttemptRef, seq: u64, event: AttemptEvent) {
        let at = self.clock.now_millis();
        self.send(WorkerMsg::AttemptEvent {
            event: AttemptEventMsg {
                attempt: attempt.clone(),
                seq,
                at,
                event,
            },
        })
        .await;
    }

    /// Wait for `EventsAcked` covering `seq` of `attempt`.
    pub async fn expect_events_acked(&mut self, attempt: &AttemptRef, seq: u64) {
        let wanted = attempt.attempt_id.clone();
        self.expect("an event acknowledgement", move |message| match message {
            ControllerMsg::EventsAcked {
                attempt_id,
                through_seq,
            } if *attempt_id == wanted && *through_seq >= seq => Some(()),
            _ => None,
        })
        .await;
    }

    /// Send an event and wait until the controller has made it durable.
    pub async fn event(&mut self, attempt: &AttemptRef, event: AttemptEvent) {
        let seq = self.send_event(attempt, event).await;
        self.expect_events_acked(attempt, seq).await;
    }

    pub async fn events(&mut self, attempt: &AttemptRef, events: Vec<AttemptEvent>) {
        for event in events {
            self.event(attempt, event).await;
        }
    }

    /// Run a started attempt to a verified release with `outcome`.
    pub async fn finish(&mut self, attempt: &AttemptRef, outcome: Outcome) {
        self.events(
            attempt,
            vec![AttemptEvent::Preparing, exited(outcome), released()],
        )
        .await;
    }

    /// Send log bytes at the stream's next offset and wait until they are
    /// acknowledged as durable.
    pub async fn log(&mut self, attempt: &AttemptRef, step: &str, bytes: &[u8]) {
        let key = (attempt.attempt_id.clone(), step.to_string());
        let offset = *self.log_offsets.get(&key).unwrap_or(&0);
        self.send(WorkerMsg::Log {
            chunk: LogChunk {
                attempt: attempt.clone(),
                step: step.into(),
                stream: LogStream::Stdout,
                offset,
                data_b64: base64(bytes),
                truncated: false,
            },
        })
        .await;
        let end = offset + bytes.len() as u64;
        let wanted = attempt.attempt_id.clone();
        self.expect("a log acknowledgement", move |message| match message {
            ControllerMsg::LogsAcked {
                attempt_id,
                through_offset,
                ..
            } if *attempt_id == wanted && *through_offset >= end => Some(()),
            _ => None,
        })
        .await;
        self.log_offsets.insert(key, end);
    }

    /// Close the socket from the worker's side.
    pub async fn disconnect(mut self) {
        let _ = self.socket.close(None).await;
    }
}

/// `ManualClock` reads as a `UnixMillis` through the `Clock` trait.
pub trait NowMillis {
    fn now_millis(&self) -> UnixMillis;
}

impl NowMillis for ManualClock {
    fn now_millis(&self) -> UnixMillis {
        bella_core::Clock::now(self)
    }
}

/// Poll until `check` returns `Some`, or panic after [`PATIENCE`].
pub async fn eventually<T, F>(what: &str, mut check: impl FnMut() -> F) -> T
where
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
