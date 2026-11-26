//! Test support: a scripted controller over in-process channels, a worker
//! harness with injected telemetry and inventory, and a helper process.
//!
//! The helper is this test binary itself, started as a step with
//! `BELLA_TEST_HELPER=<mode>` in the step's environment, so the processes
//! the tests manage behave the same on every operating system.

#![allow(dead_code)]

use bella_contain::Containment;
use bella_core::defaults::GIB;
use bella_core::ids::Digest;
use bella_core::job::{
    CommandSpec, JobSpec, Outcome, OutputSpec, Readiness, ReadinessProbe, StepKind, StepSpec,
};
use bella_core::machine::{
    ExecutorInfo, Hardware, Inventory, LocalControl, MachinePolicy, MachineProfile,
    MeasurementQuality, OsInfo, PowerState, PressureLevel, Telemetry, ThermalState,
};
use bella_core::platform::{IsolationLevel, Platform};
use bella_core::protocol::{
    AttemptEvent, AttemptEventMsg, AttemptRef, AttemptReport, CommandEnvelope, CommandResult,
    ControllerCommand, ControllerMsg, ExecutionAuthority, Heartbeat, LogStream, Offer,
    OfferDecision, PROTOCOL_VERSION, ReconcileAction, ReconcileVerdict, ReleaseProof, StepState,
    WorkerMsg,
};
use bella_core::resources::{EnforcementReport, ResourceEnvelope, ResourceVector};
use bella_core::snapshot::{Entry, Manifest, RootManifest};
use bella_core::store::{MemStore, put_bytes};
use bella_core::{
    AttemptId, BootId, Clock, CommandId, FleetId, JobId, MachineId, ManualClock, ProjectId,
    SnapshotId, SystemClock, UnixMillis,
};
use bella_journal::{Enrollment, Journal};
use bella_probe::ManagedUsage;
use bella_worker::{
    ConnectError, ConnectFuture, Connection, Connector, Hooks, TelemetrySource, Tuning,
    WorkerConfig, WorkerError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// How long a test waits for something the worker must eventually do.
pub const WAIT: Duration = Duration::from_secs(90);

pub const HELPER_ENV: &str = "BELLA_TEST_HELPER";
pub const TOKEN: &str = "bella_machine_token_for_tests_only";
pub const CONTROLLER_URL: &str = "https://controller.invalid:7420";

// ------------------------------------------------------------------ helper

/// Run the helper mode named by `BELLA_TEST_HELPER`, if any, and exit.
pub fn helper_entry() {
    // A mode on the command line (see `helper_command`) wins over the
    // environment, which a readiness probe shares with its service.
    let from_args = std::env::args().find_map(|arg| arg.strip_prefix(MODE_ARG).map(str::to_string));
    let Some(mode) = from_args.or_else(|| std::env::var(HELPER_ENV).ok()) else {
        return;
    };
    // End the line the test harness started ("test helper_entry ... "),
    // so the helper's own output starts on a line of its own.
    println!();
    let code = helper_main(&mode);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn append(path: &str, text: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

fn helper_main(mode: &str) -> i32 {
    let parts: Vec<&str> = mode.split('|').collect();
    let number =
        |index: usize| -> u64 { parts.get(index).and_then(|p| p.parse().ok()).unwrap_or(0) };
    match parts[0] {
        // A TCP service that answers every connection with an HTTP 200.
        "listen" => {
            let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", number(1) as u16)) else {
                eprintln!("could not bind");
                return 1;
            };
            println!("service ready");
            let _ = std::io::stdout().flush();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let mut request = [0u8; 512];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
            0
        }
        "connect" => {
            let address = std::net::SocketAddr::from(([127, 0, 0, 1], number(1) as u16));
            match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                Ok(_) => {
                    println!("connected");
                    0
                }
                Err(error) => {
                    eprintln!("connect failed: {error}");
                    1
                }
            }
        }
        "sleep" => {
            println!("sleeping");
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs(number(1)));
            0
        }
        "append" => {
            append(parts[1], &format!("{}\n", parts.get(2).unwrap_or(&"x")));
            println!("appended");
            0
        }
        "env" => {
            let mut text = String::new();
            for (name, value) in std::env::vars_os() {
                text.push_str(&format!(
                    "{}={}\n",
                    name.to_string_lossy(),
                    value.to_string_lossy()
                ));
            }
            let _ = std::fs::write(parts[1], text);
            0
        }
        // Leave a process behind that outlives this one, in a process
        // group of its own and without this process's pipes.
        "daemon" => {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(helper_args())
                .env(HELPER_ENV, format!("beat|{}", parts[1]))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
            match command.spawn() {
                Ok(child) => {
                    println!("daemon {}", child.id());
                    0
                }
                Err(error) => {
                    eprintln!("could not start the daemon: {error}");
                    1
                }
            }
        }
        // Like `daemon`, but the process left behind also drops every
        // environment variable, including the scope's tag, so only an
        // adapter that contains the whole tree can still find it.
        "hide" => {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(helper_args())
                .env_clear()
                .env(HELPER_ENV, format!("beat|{}", parts[1]))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                if let Ok(root) = std::env::var("SystemRoot") {
                    command.env("SystemRoot", root);
                }
                command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
            match command.spawn() {
                Ok(child) => {
                    println!("hidden {}", child.id());
                    0
                }
                Err(error) => {
                    eprintln!("could not start the hidden process: {error}");
                    1
                }
            }
        }
        "beat" => loop {
            append(parts[1], "x");
            std::thread::sleep(Duration::from_millis(50));
        },
        // Allocate and touch memory, then stay alive.
        "hog" => {
            let bytes = number(1) as usize * 1024 * 1024;
            let mut block = vec![0u8; bytes];
            for index in (0..bytes).step_by(4096) {
                block[index] = 1;
            }
            println!("hogged {}", block.len());
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs(number(2).max(1)));
            std::hint::black_box(&block);
            0
        }
        "flood" => {
            let total = number(1) as usize;
            let line = [b'x'; 1023];
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let mut written = 0usize;
            while written < total {
                let _ = out.write_all(&line);
                let _ = out.write_all(b"\n");
                written += 1024;
            }
            let _ = out.write_all(b"flood-done\n");
            0
        }
        // Print `n` numbered lines, one every `ms` milliseconds.
        "tick" => {
            for index in 0..number(1) {
                println!("line {index:04}");
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_millis(number(2)));
            }
            0
        }
        // Print `n` lines at once and exit with `code`.
        "lines" => {
            for index in 0..number(1) {
                println!("line {index:04}");
            }
            eprintln!("last words");
            number(2) as i32
        }
        "write" => {
            let data = vec![b'd'; number(2) as usize];
            match std::fs::write(parts[1], data) {
                Ok(()) => {
                    println!("written");
                    std::thread::sleep(Duration::from_secs(number(3)));
                    0
                }
                Err(error) => {
                    eprintln!("write failed: {error}");
                    1
                }
            }
        }
        other => {
            eprintln!("unknown helper mode {other}");
            2
        }
    }
}

fn helper_args() -> Vec<String> {
    ["helper_entry", "--exact", "--nocapture", "--test-threads=1"]
        .map(str::to_string)
        .to_vec()
}

/// A step that runs the helper in `mode`.
pub fn helper(name: &str, mode: &str) -> StepSpec {
    let mut argv = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    argv.extend(helper_args());
    let mut step = step(name, CommandSpec::Argv { argv });
    step.env.insert(HELPER_ENV.to_string(), mode.to_string());
    step
}

/// Prefix of the command-line argument that names a helper mode. The test
/// harness takes it as one more name filter, which matches no test.
const MODE_ARG: &str = "bella-helper-mode=";

/// The helper in `mode` as a plain command, independent of the step's
/// environment.
pub fn helper_command(mode: &str) -> CommandSpec {
    let mut argv = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    argv.extend(helper_args());
    argv.insert(2, format!("{MODE_ARG}{mode}"));
    CommandSpec::Argv { argv }
}

// ------------------------------------------------------------------- specs

pub fn step(name: &str, command: CommandSpec) -> StepSpec {
    StepSpec {
        name: name.to_string(),
        kind: StepKind::Task,
        command,
        cwd: None,
        env: BTreeMap::new(),
        depends_on: Vec::new(),
        readiness: None,
        timeout_secs: None,
        resources: None,
        ports: Vec::new(),
    }
}

/// The explicit shell the portable test scripts are written for.
pub fn shell() -> Vec<String> {
    if cfg!(windows) {
        ["cmd.exe", "/d", "/c"].map(str::to_string).to_vec()
    } else {
        ["/bin/sh", "-c"].map(str::to_string).to_vec()
    }
}

/// A step running `script`, which must mean the same to `sh` and `cmd`.
pub fn sh(name: &str, script: &str) -> StepSpec {
    step(
        name,
        CommandSpec::Shell {
            script: script.to_string(),
            shell: Some(shell()),
        },
    )
}

pub trait StepExt {
    fn kind(self, kind: StepKind) -> Self;
    fn needs(self, names: &[&str]) -> Self;
    fn timeout(self, secs: u64) -> Self;
    fn ready(self, probe: ReadinessProbe, timeout_secs: u64) -> Self;
    fn port(self, port: u16) -> Self;
    fn var(self, name: &str, value: &str) -> Self;
}

impl StepExt for StepSpec {
    fn kind(mut self, kind: StepKind) -> Self {
        self.kind = kind;
        self
    }
    fn needs(mut self, names: &[&str]) -> Self {
        self.depends_on = names.iter().map(|name| name.to_string()).collect();
        self
    }
    fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }
    fn ready(mut self, probe: ReadinessProbe, timeout_secs: u64) -> Self {
        self.readiness = Some(Readiness {
            probe,
            timeout_secs,
            interval_ms: 50,
        });
        self
    }
    fn port(mut self, port: u16) -> Self {
        self.ports.push(port);
        self
    }
    fn var(mut self, name: &str, value: &str) -> Self {
        self.env.insert(name.to_string(), value.to_string());
        self
    }
}

pub fn spec(steps: Vec<StepSpec>) -> JobSpec {
    let reservation = default_reservation();
    JobSpec {
        recipe: "test".to_string(),
        recipe_digest: Digest::of_bytes(b"recipe"),
        steps,
        envelope: ResourceEnvelope::fixed(reservation),
        compatibility: Default::default(),
        placement: Default::default(),
        priority: Default::default(),
        retry_class: Default::default(),
        max_attempts: 1,
        timeout_secs: 120,
        cleanup_timeout_secs: 30,
        cloud: Default::default(),
        source_lan_only: false,
        outputs: OutputSpec::default(),
        environment: None,
        env: BTreeMap::new(),
        disconnected_continuation_secs: 0,
        parallelism: None,
    }
}

pub fn default_reservation() -> ResourceVector {
    ResourceVector::new(2000, GIB, 0)
}

/// A port that was free a moment ago.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn port_open(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
}

// --------------------------------------------------------------- telemetry

/// A host with plenty of everything and no pressure.
pub fn calm_telemetry() -> Telemetry {
    Telemetry {
        sampled_at: UnixMillis(0),
        cpu_used_milli: 500,
        cpu_managed_milli: 0,
        memory_total_bytes: 64 * GIB,
        memory_available_bytes: 48 * GIB,
        memory_managed_bytes: 0,
        swap_used_bytes: 0,
        memory_pressure: PressureLevel::Normal,
        disk_total_bytes: 1000 * GIB,
        disk_free_bytes: 500 * GIB,
        power: PowerState::default(),
        thermal: ThermalState::Nominal,
        quality: MeasurementQuality::Good,
    }
}

struct FakeTelemetry {
    shared: Arc<Mutex<Telemetry>>,
    samples: Arc<AtomicU64>,
}

impl TelemetrySource for FakeTelemetry {
    fn sample(&mut self, managed: ManagedUsage) -> Telemetry {
        self.samples.fetch_add(1, Ordering::SeqCst);
        let mut sample = self.shared.lock().unwrap().clone();
        sample.memory_managed_bytes = managed.memory_bytes;
        sample
    }
}

pub fn test_inventory(containment: &dyn Containment) -> Inventory {
    Inventory {
        revision: 1,
        hardware: Hardware {
            logical_cpus: 16,
            physical_cpus: Some(8),
            cpu_model: "test".to_string(),
            memory_bytes: 64 * GIB,
            disks: Vec::new(),
            gpus: Vec::new(),
        },
        os: OsInfo {
            platform: Platform::current().expect("a supported platform"),
            os_version: "test".to_string(),
            kernel: "test".to_string(),
            hostname: "test".to_string(),
        },
        capabilities: Vec::new(),
        executors: vec![ExecutorInfo {
            isolation: IsolationLevel::Native,
            mechanism: containment.mechanism().to_string(),
            enforcement: containment.enforcement(),
        }],
        worker_version: "0.0.0-test".to_string(),
    }
}

pub fn test_policy() -> MachinePolicy {
    let mut policy = MachinePolicy::defaults_for(MachineProfile::DedicatedBuilder);
    policy.cleanup_grace_secs = 1;
    policy
}

pub fn test_tuning() -> Tuning {
    Tuning {
        protection_interval: Duration::from_millis(50),
        heartbeat_interval: Some(Duration::from_millis(150)),
        reconnect_min: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(100),
        handshake_timeout: Duration::from_secs(20),
        kill_wait: Duration::from_secs(20),
        poll_interval: Duration::from_millis(10),
        log_drain_grace: Duration::from_millis(500),
        disk_check_interval: Duration::from_millis(100),
        quarantine_retry_interval: Duration::from_millis(200),
        ..Tuning::default()
    }
}

// --------------------------------------------------------------- connector

pub struct ServerSide {
    pub tx: mpsc::Sender<ControllerMsg>,
    pub rx: mpsc::Receiver<WorkerMsg>,
}

/// Hands each connection the worker opens to the scripted controller.
pub struct FakeConnector {
    accept: mpsc::UnboundedSender<ServerSide>,
    pub allow: AtomicBool,
    pub attempts: AtomicU64,
}

impl Connector for FakeConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if !self.allow.load(Ordering::SeqCst) {
                return Err(ConnectError::new("the test controller refuses connections"));
            }
            let (worker_tx, server_rx) = mpsc::channel(16);
            let (server_tx, worker_rx) = mpsc::channel(64);
            self.accept
                .send(ServerSide {
                    tx: server_tx,
                    rx: server_rx,
                })
                .map_err(|_| ConnectError::new("the test controller is gone"))?;
            Ok(Connection {
                tx: worker_tx,
                rx: worker_rx,
            })
        })
    }
}

// -------------------------------------------------------------- controller

#[derive(Clone, Debug)]
pub struct Hello {
    pub machine_id: MachineId,
    pub boot_id: BootId,
    pub connection_generation: u64,
    pub known_epoch: u64,
    pub policy_revision: u64,
    pub local_control: LocalControl,
    pub attempts: Vec<AttemptReport>,
    pub inventory: Inventory,
}

type StreamKey = (AttemptId, String, LogStream);

/// The scripted controller. It records everything the worker sends and
/// panics when the worker breaks the numbering rules: an event sequence
/// with a gap, an acknowledged event or log range sent again, a log chunk
/// that leaves a hole or contradicts bytes already received.
pub struct Ctl {
    rt: tokio::runtime::Handle,
    accept: mpsc::UnboundedReceiver<ServerSide>,
    conn: Option<ServerSide>,
    pub epoch: u64,
    pub fleet: FleetId,
    pub policy: MachinePolicy,
    /// Acknowledge events and log chunks as they arrive.
    pub auto_ack: bool,
    pub hello: Option<Hello>,
    pub events: Vec<AttemptEventMsg>,
    pub event_seq: BTreeMap<AttemptId, u64>,
    pub acked_seq: BTreeMap<AttemptId, u64>,
    pub logs: BTreeMap<StreamKey, Vec<u8>>,
    pub acked_log: BTreeMap<StreamKey, u64>,
    pub truncated: BTreeSet<StreamKey>,
    pub heartbeats: Vec<Heartbeat>,
    pub acks: Vec<(CommandId, CommandResult)>,
    pub replies: Vec<(CommandId, AttemptRef, OfferDecision)>,
    pub pongs: Vec<u64>,
    /// Unacknowledged events and log bytes that arrived a second time.
    pub resent_events: u64,
    pub resent_log_bytes: u64,
    /// Events and log chunks received on the current connection.
    pub received_on_connection: u64,
    /// Event sequences received on the current connection.
    conn_events: BTreeSet<(AttemptId, u64)>,
    /// End of the log bytes received per stream on the current connection.
    conn_log_end: BTreeMap<StreamKey, u64>,
    /// Check the worker's numbering. Off only while a failed test unwinds.
    strict: bool,
}

pub fn b64_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            other => panic!("not base64: {other}"),
        }
    };
    assert_eq!(text.len() % 4, 0, "base64 must be padded");
    let mut out = Vec::new();
    for quad in text.as_bytes().chunks(4) {
        let pad = quad.iter().filter(|c| **c == b'=').count();
        let bits = quad
            .iter()
            .map(|c| if *c == b'=' { 0 } else { value(*c) })
            .fold(0u32, |acc, v| (acc << 6) | v);
        let bytes = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    out
}

impl Ctl {
    pub fn connected(&self) -> bool {
        self.conn.is_some()
    }

    pub fn send(&mut self, message: ControllerMsg) {
        let conn = self.conn.as_ref().expect("no connection");
        let tx = conn.tx.clone();
        self.rt
            .block_on(async move { tx.send(message).await })
            .expect("the worker closed the connection");
    }

    fn recv(&mut self, limit: Duration) -> Option<WorkerMsg> {
        let conn = self.conn.as_mut()?;
        let rt = self.rt.clone();
        let received = rt.block_on(async { tokio::time::timeout(limit, conn.rx.recv()).await });
        match received {
            Ok(Some(message)) => Some(message),
            Ok(None) => {
                // The worker dropped its end.
                self.conn = None;
                None
            }
            Err(_) => None,
        }
    }

    /// Send unless the worker already closed the connection, as it does
    /// while it shuts down. Returns whether the message went out.
    fn try_send(&mut self, message: ControllerMsg) -> bool {
        let Some(conn) = self.conn.as_ref() else {
            return false;
        };
        let tx = conn.tx.clone();
        let sent = self.rt.block_on(async move { tx.send(message).await });
        if sent.is_err() {
            self.conn = None;
        }
        sent.is_ok()
    }

    /// Close the connection from the controller's side.
    pub fn drop_connection(&mut self) {
        self.conn = None;
    }

    fn absorb(&mut self, message: WorkerMsg) {
        if !self.strict {
            return;
        }
        match message {
            WorkerMsg::Hello { .. } => panic!("a second Hello on one connection"),
            WorkerMsg::Heartbeat { heartbeat } => {
                if let Some(previous) = self.heartbeats.last()
                    && previous.connection_generation == heartbeat.connection_generation
                {
                    assert_eq!(
                        heartbeat.seq,
                        previous.seq + 1,
                        "heartbeat sequence must not skip or repeat"
                    );
                }
                self.heartbeats.push(*heartbeat);
            }
            WorkerMsg::OfferReply {
                command_id,
                attempt,
                decision,
            } => self.replies.push((command_id, attempt, decision)),
            WorkerMsg::CommandAck { command_id, result } => self.acks.push((command_id, result)),
            WorkerMsg::Pong { nonce } => self.pongs.push(nonce),
            WorkerMsg::InventoryChanged { .. } => {}
            WorkerMsg::AttemptEvent { event } => {
                self.received_on_connection += 1;
                let id = event.attempt.attempt_id.clone();
                let last = self.event_seq.get(&id).copied().unwrap_or(0);
                // An acknowledgement can be lost with its connection, so a
                // new connection may repeat acknowledged events; one
                // connection never carries an event twice.
                assert!(
                    self.conn_events.insert((id.clone(), event.seq)),
                    "event {} of {id} was sent twice on one connection",
                    event.seq
                );
                if event.seq <= last {
                    let known = self
                        .events
                        .iter()
                        .find(|known| known.attempt.attempt_id == id && known.seq == event.seq)
                        .expect("a known sequence");
                    assert_eq!(*known, event, "a resent event must be identical");
                    self.resent_events += 1;
                } else {
                    assert_eq!(event.seq, last + 1, "event sequence of {id} has a gap");
                    self.event_seq.insert(id.clone(), event.seq);
                    self.events.push(event);
                }
                if self.auto_ack {
                    self.ack_events(&id);
                }
            }
            WorkerMsg::Log { chunk } => {
                self.received_on_connection += 1;
                let data = b64_decode(&chunk.data_b64);
                assert!(
                    data.len() <= bella_core::defaults::LOG_CHUNK_BYTES,
                    "a log chunk is larger than LOG_CHUNK_BYTES"
                );
                let key = (
                    chunk.attempt.attempt_id.clone(),
                    chunk.step.clone(),
                    chunk.stream,
                );
                let sent_here = self.conn_log_end.get(&key).copied().unwrap_or(0);
                assert!(
                    chunk.offset >= sent_here,
                    "log bytes at {} of {key:?} were sent twice on one connection (already through {sent_here})",
                    chunk.offset
                );
                self.conn_log_end
                    .insert(key.clone(), chunk.offset + data.len() as u64);
                let have = self.logs.entry(key.clone()).or_default();
                let offset = chunk.offset as usize;
                assert!(
                    offset <= have.len(),
                    "log stream {key:?} has a hole at {offset}"
                );
                let overlap = (have.len() - offset).min(data.len());
                assert_eq!(
                    &have[offset..offset + overlap],
                    &data[..overlap],
                    "resent log bytes of {key:?} differ"
                );
                self.resent_log_bytes += overlap as u64;
                have.extend_from_slice(&data[overlap..]);
                if chunk.truncated {
                    self.truncated.insert(key.clone());
                }
                if self.auto_ack {
                    self.ack_log(&key);
                }
            }
        }
    }

    /// Acknowledge every event of `attempt` received so far.
    pub fn ack_events(&mut self, attempt: &AttemptId) {
        let through = self.event_seq.get(attempt).copied().unwrap_or(0);
        if through > self.acked_seq.get(attempt).copied().unwrap_or(0)
            && self.try_send(ControllerMsg::EventsAcked {
                attempt_id: attempt.clone(),
                through_seq: through,
            })
        {
            self.acked_seq.insert(attempt.clone(), through);
        }
    }

    /// Acknowledge every byte of one log stream received so far.
    pub fn ack_log(&mut self, key: &StreamKey) {
        let through = self.logs.get(key).map_or(0, |bytes| bytes.len() as u64);
        if through > self.acked_log.get(key).copied().unwrap_or(0)
            && self.try_send(ControllerMsg::LogsAcked {
                attempt_id: key.0.clone(),
                step: key.1.clone(),
                stream: key.2,
                through_offset: through,
            })
        {
            self.acked_log.insert(key.clone(), through);
        }
    }

    pub fn ack_everything(&mut self) {
        let attempts: Vec<AttemptId> = self.event_seq.keys().cloned().collect();
        for attempt in attempts {
            self.ack_events(&attempt);
        }
        let streams: Vec<StreamKey> = self.logs.keys().cloned().collect();
        for stream in streams {
            self.ack_log(&stream);
        }
    }

    /// Process incoming messages for `limit`.
    pub fn pump(&mut self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.recv(left.min(Duration::from_millis(50))) {
                Some(message) => self.absorb(message),
                None if self.conn.is_none() => std::thread::sleep(Duration::from_millis(10)),
                None => {}
            }
        }
    }

    /// Process incoming messages until `check` answers, or panic after
    /// [`WAIT`] with what was received.
    pub fn wait<T>(&mut self, what: &str, mut check: impl FnMut(&Ctl) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(value) = check(self) {
                return value;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}\n{}", self.dump());
            }
            match self.recv(Duration::from_millis(50)) {
                Some(message) => self.absorb(message),
                None if self.conn.is_none() => std::thread::sleep(Duration::from_millis(10)),
                None => {}
            }
        }
    }

    pub fn dump(&self) -> String {
        let mut text = String::new();
        for event in &self.events {
            text.push_str(&format!(
                "event {} #{}: {:?}\n",
                event.attempt.attempt_id, event.seq, event.event
            ));
        }
        for ((attempt, step, stream), bytes) in &self.logs {
            text.push_str(&format!(
                "log {attempt} {step} {stream:?}: {}\n",
                String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(600)..])
            ));
        }
        for (id, result) in &self.acks {
            text.push_str(&format!("ack {id}: {result:?}\n"));
        }
        for (id, _, decision) in &self.replies {
            text.push_str(&format!("offer reply {id}: {decision:?}\n"));
        }
        if let Some(heartbeat) = self.heartbeats.last() {
            text.push_str(&format!(
                "last heartbeat: admitting={} flags={:?} attempts={:?}\n",
                heartbeat.admitting, heartbeat.flags, heartbeat.attempts
            ));
        }
        text
    }

    /// Wait for the worker's next connection and its `Hello`.
    pub fn accept_hello(&mut self) -> Hello {
        let rt = self.rt.clone();
        let side = rt
            .block_on(async { tokio::time::timeout(WAIT, self.accept.recv()).await })
            .expect("the worker did not connect")
            .expect("the connector is gone");
        self.conn = Some(side);
        self.received_on_connection = 0;
        self.conn_events.clear();
        self.conn_log_end.clear();
        let first = self.recv(WAIT).expect("the worker did not say Hello");
        let WorkerMsg::Hello {
            protocol_version,
            machine_id,
            boot_id,
            connection_generation,
            known_epoch,
            inventory,
            policy_revision,
            local_control,
            attempts,
        } = first
        else {
            panic!("the first message was not Hello: {first:?}");
        };
        assert_eq!(protocol_version, PROTOCOL_VERSION);
        let hello = Hello {
            machine_id,
            boot_id,
            connection_generation,
            known_epoch,
            policy_revision,
            local_control,
            attempts,
            inventory: *inventory,
        };
        self.hello = Some(hello.clone());
        hello
    }

    pub fn welcome(&mut self) {
        self.send(ControllerMsg::Welcome {
            protocol_version: PROTOCOL_VERSION,
            controller_epoch: self.epoch,
            fleet_id: self.fleet.clone(),
            policy: self.policy.clone(),
            server_time: UnixMillis::now(),
            heartbeat_interval_ms: 5000,
        });
    }

    /// Accept, welcome and tell the worker to continue everything it
    /// reported that still holds resources.
    pub fn connect(&mut self) -> Hello {
        let hello = self.accept_hello();
        self.welcome();
        let verdicts = hello
            .attempts
            .iter()
            .map(|report| ReconcileVerdict {
                attempt: report.attempt.clone(),
                action: ReconcileAction::Continue,
            })
            .collect();
        self.reconcile(verdicts);
        hello
    }

    pub fn reconcile(&mut self, verdicts: Vec<ReconcileVerdict>) {
        let id = self.command(ControllerCommand::Reconcile { verdicts });
        assert_eq!(self.wait_ack(&id), CommandResult::Applied);
    }

    pub fn command(&mut self, command: ControllerCommand) -> CommandId {
        let id = CommandId::generate();
        self.command_as(&id, self.epoch, command);
        id
    }

    pub fn command_as(&mut self, id: &CommandId, epoch: u64, command: ControllerCommand) {
        self.send(ControllerMsg::Command {
            envelope: CommandEnvelope {
                command_id: id.clone(),
                controller_epoch: epoch,
                sent_at: UnixMillis::now(),
                command,
            },
        });
    }

    pub fn wait_ack(&mut self, id: &CommandId) -> CommandResult {
        self.wait("a command acknowledgement", |ctl| {
            ctl.acks
                .iter()
                .rev()
                .find(|(acked, _)| acked == id)
                .map(|(_, result)| result.clone())
        })
    }

    pub fn wait_reply(&mut self, id: &CommandId) -> OfferDecision {
        self.wait("an offer reply", |ctl| {
            ctl.replies
                .iter()
                .rev()
                .find(|(replied, _, _)| replied == id)
                .map(|(_, _, decision)| decision.clone())
        })
    }

    pub fn offer(&mut self, offer: &Offer) -> OfferDecision {
        let id = self.command(ControllerCommand::Offer {
            offer: Box::new(offer.clone()),
        });
        self.wait_reply(&id)
    }

    pub fn start(&mut self, attempt: &AttemptRef, authority: ExecutionAuthority) -> CommandResult {
        let id = self.command(ControllerCommand::Start {
            attempt: attempt.clone(),
            authority,
        });
        self.wait_ack(&id)
    }

    pub fn cancel(&mut self, attempt: &AttemptRef, grace_secs: u64) -> CommandResult {
        let id = self.command(ControllerCommand::Cancel {
            attempt: attempt.clone(),
            reason: "cancelled by the test".to_string(),
            grace_secs,
        });
        self.wait_ack(&id)
    }

    pub fn events_of(&self, attempt: &AttemptId) -> Vec<&AttemptEvent> {
        self.events
            .iter()
            .filter(|event| &event.attempt.attempt_id == attempt)
            .map(|event| &event.event)
            .collect()
    }

    pub fn wait_event<T>(
        &mut self,
        attempt: &AttemptId,
        what: &str,
        mut pick: impl FnMut(&AttemptEvent) -> Option<T>,
    ) -> T {
        let attempt = attempt.clone();
        self.wait(what, |ctl| {
            ctl.events_of(&attempt).into_iter().find_map(&mut pick)
        })
    }

    pub fn wait_released(&mut self, attempt: &AttemptId) -> ReleaseProof {
        self.wait_event(attempt, "the release", |event| match event {
            AttemptEvent::Released { proof } => Some(proof.clone()),
            _ => None,
        })
    }

    pub fn outcome(&self, attempt: &AttemptId) -> Option<Outcome> {
        self.events_of(attempt)
            .into_iter()
            .find_map(|event| match event {
                AttemptEvent::Exited { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
    }

    pub fn wait_outcome(&mut self, attempt: &AttemptId) -> Outcome {
        self.wait_event(attempt, "the outcome", |event| match event {
            AttemptEvent::Exited { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
    }

    /// Every state `step` went through, in order.
    pub fn step_states(&self, attempt: &AttemptId, step: &str) -> Vec<StepState> {
        self.events_of(attempt)
            .into_iter()
            .filter_map(|event| match event {
                AttemptEvent::StepChanged {
                    step: name, state, ..
                } if name == step => Some(*state),
                _ => None,
            })
            .collect()
    }

    pub fn wait_step(&mut self, attempt: &AttemptId, step: &str, state: StepState) {
        let (attempt, step) = (attempt.clone(), step.to_string());
        self.wait(&format!("step {step} to be {state:?}"), |ctl| {
            ctl.step_states(&attempt, &step)
                .contains(&state)
                .then_some(())
        });
    }

    pub fn log(&self, attempt: &AttemptId, step: &str, stream: LogStream) -> String {
        self.logs
            .get(&(attempt.clone(), step.to_string(), stream))
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    }

    /// Wait for a heartbeat sent after this call that satisfies `check`.
    pub fn wait_heartbeat(
        &mut self,
        what: &str,
        mut check: impl FnMut(&Heartbeat) -> bool,
    ) -> Heartbeat {
        let from = self.heartbeats.len();
        self.wait(what, |ctl| {
            ctl.heartbeats[from.min(ctl.heartbeats.len())..]
                .iter()
                .find(|heartbeat| check(heartbeat))
                .cloned()
        })
    }
}

// ----------------------------------------------------------------- harness

pub struct Options {
    pub manual_clock: bool,
    pub tuning: Tuning,
    pub policy: MachinePolicy,
    pub allow_connections: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            manual_clock: false,
            tuning: test_tuning(),
            policy: test_policy(),
            allow_connections: true,
        }
    }
}

type Running = (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), WorkerError>>,
);

/// One enrolled worker in a temporary directory and its scripted controller.
pub struct Harness {
    pub rt: tokio::runtime::Runtime,
    pub dir: tempfile::TempDir,
    pub remote: Arc<MemStore>,
    /// While set, the controller's store refuses every upload.
    pub refuse_uploads: Arc<AtomicBool>,
    pub clock: Arc<dyn Clock>,
    pub manual: Option<ManualClock>,
    pub telemetry: Arc<Mutex<Telemetry>>,
    pub samples: Arc<AtomicU64>,
    pub connector: Arc<FakeConnector>,
    pub ctl: Ctl,
    pub tuning: Tuning,
    pub machine: MachineId,
    /// Containment for the next start; `None` uses the native adapter.
    pub containment: Option<Box<dyn Containment>>,
    pub boot_id: Option<BootId>,
    running: Option<Running>,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Options::default())
    }

    pub fn with(options: Options) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (clock, manual): (Arc<dyn Clock>, Option<ManualClock>) = if options.manual_clock {
            let manual = ManualClock::at(UnixMillis(1_800_000_000_000));
            (Arc::new(manual.clone()), Some(manual))
        } else {
            (Arc::new(SystemClock), None)
        };
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();
        let connector = Arc::new(FakeConnector {
            accept: accept_tx,
            allow: AtomicBool::new(options.allow_connections),
            attempts: AtomicU64::new(0),
        });
        let fleet = FleetId::generate();
        let machine = MachineId::generate();
        let harness = Self {
            ctl: Ctl {
                rt: rt.handle().clone(),
                accept: accept_rx,
                conn: None,
                epoch: 7,
                fleet: fleet.clone(),
                policy: options.policy.clone(),
                auto_ack: true,
                hello: None,
                events: Vec::new(),
                event_seq: BTreeMap::new(),
                acked_seq: BTreeMap::new(),
                logs: BTreeMap::new(),
                acked_log: BTreeMap::new(),
                truncated: BTreeSet::new(),
                heartbeats: Vec::new(),
                acks: Vec::new(),
                replies: Vec::new(),
                pongs: Vec::new(),
                resent_events: 0,
                resent_log_bytes: 0,
                received_on_connection: 0,
                conn_events: BTreeSet::new(),
                conn_log_end: BTreeMap::new(),
                strict: true,
            },
            rt,
            dir,
            remote: Arc::new(MemStore::new()),
            refuse_uploads: Arc::new(AtomicBool::new(false)),
            clock,
            manual,
            telemetry: Arc::new(Mutex::new(calm_telemetry())),
            samples: Arc::new(AtomicU64::new(0)),
            connector,
            tuning: options.tuning,
            machine: machine.clone(),
            containment: None,
            boot_id: None,
            running: None,
        };
        bella_worker::store_enrollment(
            &harness.data_dir(),
            &Enrollment {
                fleet_id: fleet,
                machine_id: machine,
                controller_url: CONTROLLER_URL.to_string(),
                fingerprint: "sha256:test".to_string(),
                policy: options.policy,
            },
            TOKEN,
        )
        .unwrap();
        harness
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("d")
    }

    pub fn work_root(&self) -> PathBuf {
        self.dir.path().join("w")
    }

    /// A path outside every workspace, for what steps leave for the test.
    pub fn outside(&self, name: &str) -> String {
        let dir = self.dir.path().join("out");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_string_lossy().into_owned()
    }

    pub fn now(&self) -> UnixMillis {
        self.clock.now()
    }

    pub fn config(&mut self) -> WorkerConfig {
        let containment = self
            .containment
            .take()
            .unwrap_or_else(|| bella_contain::native().expect("a containment adapter"));
        let inventory = test_inventory(containment.as_ref());
        WorkerConfig {
            data_dir: self.data_dir(),
            work_root: self.work_root(),
            remote: Arc::new(GatedStore {
                inner: self.remote.clone(),
                refuse_puts: self.refuse_uploads.clone(),
            }),
            connector: self.connector.clone(),
            containment: Some(containment),
            clock: self.clock.clone(),
            tuning: self.tuning.clone(),
            worker_version: "0.0.0-test".to_string(),
            hooks: Hooks {
                telemetry: Some(Box::new(FakeTelemetry {
                    shared: self.telemetry.clone(),
                    samples: self.samples.clone(),
                })),
                inventory: Some(inventory),
                boot_id: self.boot_id.clone(),
            },
        }
    }

    /// Start the daemon.
    pub fn start(&mut self) {
        assert!(self.running.is_none(), "the worker is already running");
        let config = self.config();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = self.rt.spawn(bella_worker::run(config, async move {
            let _ = stop_rx.await;
        }));
        self.running = Some((stop_tx, task));
    }

    /// Start the daemon and complete the handshake.
    pub fn start_connected(&mut self) -> Hello {
        self.start();
        self.ctl.connect()
    }

    /// Shut the daemon down and return what `run` returned.
    pub fn stop(&mut self) -> Result<(), WorkerError> {
        let (stop, task) = self.running.take().expect("the worker is not running");
        let _ = stop.send(());
        // Keep reading so a worker blocked on a full channel can finish.
        let deadline = Instant::now() + WAIT;
        while !task.is_finished() {
            assert!(Instant::now() < deadline, "the worker did not shut down");
            self.ctl.pump(Duration::from_millis(20));
        }
        self.ctl.drop_connection();
        self.rt.block_on(task).expect("the worker task panicked")
    }

    /// What `run` returned, if it already returned.
    pub fn finished(&mut self) -> Option<Result<(), WorkerError>> {
        if !self.running.as_ref()?.1.is_finished() {
            return None;
        }
        Some(self.join())
    }

    /// Wait for `run` to return on its own.
    pub fn join(&mut self) -> Result<(), WorkerError> {
        let (_stop, task) = self.running.take().expect("the worker is not running");
        self.rt.block_on(task).expect("the worker task panicked")
    }

    /// The journal, opened next to the daemon the way a tool would.
    pub fn journal(&self) -> Journal {
        Journal::open_existing(
            self.data_dir().join(bella_worker::JOURNAL_FILE),
            self.clock.clone(),
        )
        .unwrap()
    }

    /// Store a snapshot of `files` in the controller's store.
    pub fn snapshot(&self, files: &[(&str, &[u8])]) -> (SnapshotId, Digest) {
        let entries = files
            .iter()
            .map(|(path, bytes)| Entry::File {
                path: path.to_string(),
                digest: put_bytes(self.remote.as_ref(), bytes).unwrap(),
                size: bytes.len() as u64,
                executable: false,
            })
            .collect();
        let manifest = Manifest::new(vec![RootManifest {
            mount: ".".to_string(),
            entries,
        }]);
        let digest = put_bytes(self.remote.as_ref(), &manifest.canonical_bytes()).unwrap();
        (SnapshotId(digest.clone()), digest)
    }

    /// An offer for `spec` on a one-file snapshot.
    pub fn offer(&self, spec: JobSpec) -> Offer {
        let (snapshot_id, manifest_digest) = self.snapshot(&[("input.txt", b"input\n")]);
        self.offer_on(spec, snapshot_id, manifest_digest)
    }

    pub fn offer_on(
        &self,
        spec: JobSpec,
        snapshot_id: SnapshotId,
        manifest_digest: Digest,
    ) -> Offer {
        Offer {
            attempt: AttemptRef {
                attempt_id: AttemptId::generate(),
                generation: 1,
            },
            job_id: JobId::generate(),
            project_id: ProjectId::derive(b"project"),
            snapshot_id,
            manifest_digest,
            reservation: spec.envelope.peak.clone(),
            spec,
            policy_revision: self.ctl.policy.revision,
            expires_at: self.now().plus_secs(15),
        }
    }

    /// An authority valid for a minute with an hour to the hard deadline.
    pub fn authority(&self) -> ExecutionAuthority {
        ExecutionAuthority {
            valid_until: self.now().plus_secs(60),
            hard_deadline: self.now().plus_secs(3600),
            disconnected_continuation_secs: 0,
        }
    }

    /// Offer and start `spec`; returns the offer once both were accepted.
    pub fn launch(&mut self, spec: JobSpec) -> Offer {
        let offer = self.offer(spec);
        self.launch_offer(&offer);
        offer
    }

    pub fn launch_offer(&mut self, offer: &Offer) {
        assert_eq!(self.ctl.offer(offer), OfferDecision::Accepted);
        let authority = self.authority();
        assert_eq!(
            self.ctl.start(&offer.attempt, authority),
            CommandResult::Applied
        );
    }

    /// Nothing is left in the work root.
    pub fn assert_no_workspaces(&self) {
        let left: Vec<PathBuf> = std::fs::read_dir(self.work_root())
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "workspaces left behind: {left:?}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.running.is_some() && !std::thread::panicking() {
            let _ = self.stop();
        } else if let Some((stop, _task)) = self.running.take() {
            // After a failed assertion: still stop the worker so it cleans
            // up its processes, without asserting anything further.
            self.ctl.strict = false;
            let _ = stop.send(());
            let deadline = Instant::now() + Duration::from_secs(30);
            while !_task.is_finished() && Instant::now() < deadline {
                self.ctl.pump(Duration::from_millis(20));
            }
        }
    }
}

/// Does the file at `path` keep growing? Sampled over `window`.
pub fn still_growing(path: &str, window: Duration) -> bool {
    let size = |path: &str| std::fs::metadata(Path::new(path)).map_or(0, |meta| meta.len());
    let before = size(path);
    std::thread::sleep(window);
    size(path) > before
}

pub fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn unverified_enforcement() -> EnforcementReport {
    EnforcementReport::none()
}

// ------------------------------------------------------------- tampering

/// Switches that make [`Tampered`] scopes misreport what they see.
#[derive(Clone, Default)]
pub struct Tamper {
    /// `members()` answers `Unknown` while set.
    pub membership_unknown: Arc<AtomicBool>,
    /// `limit_hits()` reports this many memory limit hits when non-zero.
    pub memory_hits: Arc<AtomicU64>,
    /// `usage()` reports this much current memory when non-zero.
    pub memory_bytes: Arc<AtomicU64>,
}

/// The native adapter with scopes that report what [`Tamper`] says.
pub struct Tampered {
    pub inner: Box<dyn Containment>,
    pub tamper: Tamper,
}

impl Tampered {
    pub fn native(tamper: &Tamper) -> Box<dyn Containment> {
        Box::new(Self {
            inner: bella_contain::native().expect("a containment adapter"),
            tamper: tamper.clone(),
        })
    }
}

impl Containment for Tampered {
    fn kind(&self) -> bella_core::protocol::ScopeKind {
        self.inner.kind()
    }
    fn mechanism(&self) -> &'static str {
        self.inner.mechanism()
    }
    fn enforcement(&self) -> EnforcementReport {
        self.inner.enforcement()
    }
    fn self_test(&self) -> Result<EnforcementReport, bella_contain::ContainError> {
        self.inner.self_test()
    }
    fn create(
        &self,
        name: &str,
        limits: &bella_contain::Limits,
    ) -> Result<Box<dyn bella_contain::Scope>, bella_contain::ContainError> {
        Ok(Box::new(TamperedScope {
            inner: self.inner.create(name, limits)?,
            tamper: self.tamper.clone(),
        }))
    }
    fn adopt(
        &self,
        identity: &bella_core::protocol::ScopeIdentity,
    ) -> Result<Option<Box<dyn bella_contain::Scope>>, bella_contain::ContainError> {
        Ok(self.inner.adopt(identity)?.map(|inner| {
            Box::new(TamperedScope {
                inner,
                tamper: self.tamper.clone(),
            }) as Box<dyn bella_contain::Scope>
        }))
    }
}

struct TamperedScope {
    inner: Box<dyn bella_contain::Scope>,
    tamper: Tamper,
}

impl bella_contain::Scope for TamperedScope {
    fn identity(&self) -> bella_core::protocol::ScopeIdentity {
        self.inner.identity()
    }
    fn spawn(
        &mut self,
        spec: &bella_contain::ProcessSpec,
    ) -> Result<bella_contain::Spawned, bella_contain::ContainError> {
        self.inner.spawn(spec)
    }
    fn usage(&self) -> Result<bella_core::protocol::UsageSummary, bella_contain::ContainError> {
        let mut usage = self.inner.usage()?;
        let fake = self.tamper.memory_bytes.load(Ordering::SeqCst);
        if fake > 0 {
            usage.memory_current_bytes = fake;
            usage.memory_peak_bytes = usage.memory_peak_bytes.max(fake);
        }
        Ok(usage)
    }
    fn members(&self) -> Result<bella_contain::Membership, bella_contain::ContainError> {
        let real = self.inner.members()?;
        if self.tamper.membership_unknown.load(Ordering::SeqCst) {
            return Ok(bella_contain::Membership::Unknown);
        }
        Ok(real)
    }
    fn limit_hits(&self) -> Result<Option<bella_contain::LimitHits>, bella_contain::ContainError> {
        let hits = self.tamper.memory_hits.load(Ordering::SeqCst);
        if hits > 0 {
            return Ok(Some(bella_contain::LimitHits {
                memory: hits,
                processes: 0,
            }));
        }
        self.inner.limit_hits()
    }
    fn set_limits(
        &mut self,
        limits: &bella_contain::Limits,
    ) -> Result<(), bella_contain::ContainError> {
        self.inner.set_limits(limits)
    }
    fn terminate(&mut self) -> Result<(), bella_contain::ContainError> {
        self.inner.terminate()
    }
    fn kill(&mut self) -> Result<(), bella_contain::ContainError> {
        self.inner.kill()
    }
    fn destroy(self: Box<Self>) -> Result<String, bella_contain::ContainError> {
        self.inner.destroy()
    }
}

// --------------------------------------------------------- stray processes

/// Start the helper in `mode` as a plain child of the test, outside every
/// scope.
pub fn spawn_helper(mode: &str) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(helper_args())
        .env(HELPER_ENV, mode)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().expect("the helper starts")
}

/// Forcibly end the process `pid` that a step left behind.
pub fn kill_pid(pid: u32) {
    #[cfg(unix)]
    let mut command = std::process::Command::new("kill");
    #[cfg(unix)]
    command.args(["-9", &pid.to_string()]);
    #[cfg(windows)]
    let mut command = std::process::Command::new("taskkill");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command
            .args(["/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The number after `prefix` on a line of `text`.
pub fn pid_after(text: &str, prefix: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(prefix)?.trim().parse().ok())
}

/// The controller's store as the worker sees it, with uploads that can be
/// made to fail.
struct GatedStore {
    inner: Arc<MemStore>,
    refuse_puts: Arc<AtomicBool>,
}

impl bella_core::store::ObjectStore for GatedStore {
    fn contains(&self, digest: &Digest) -> Result<bool, bella_core::store::StoreError> {
        self.inner.contains(digest)
    }
    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), bella_core::store::StoreError> {
        if self.refuse_puts.load(Ordering::SeqCst) {
            return Err(bella_core::store::StoreError::Unavailable(
                "the test store refuses uploads".to_string(),
            ));
        }
        self.inner.put(reader, expected)
    }
    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, bella_core::store::StoreError> {
        self.inner.open(digest)
    }
    fn size(&self, digest: &Digest) -> Result<Option<u64>, bella_core::store::StoreError> {
        self.inner.size(digest)
    }
}
