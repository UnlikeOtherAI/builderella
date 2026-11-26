//! A whole fleet on loopback, made of the real `bella` binary: one
//! controller with TLS on and its certificate pinned, real workers enrolled
//! with pairing codes, temporary data directories, credentials and home
//! directory. Every process is stopped and every directory removed when the
//! [`Fleet`] is dropped, also when a test fails.

#![allow(dead_code)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

pub const BELLA: &str = env!("CARGO_BIN_EXE_bella");

/// How long a condition may take to become true before a test fails. An
/// upper bound for slow, loaded hosts; conditions normally hold in seconds.
pub const PATIENCE: Duration = Duration::from_secs(120);
/// How long one `bella` client command may take.
const COMMAND_LIMIT: Duration = Duration::from_secs(300);
/// Fleets that may run at once, so that enrollment probes and workers of
/// parallel tests do not starve each other. `BELLA_E2E_PARALLEL` overrides.
const DEFAULT_PARALLEL_FLEETS: usize = 4;

/// A counting semaphore limiting how many fleets run at once.
struct Slots {
    used: Mutex<usize>,
    freed: Condvar,
}

static SLOTS: Slots = Slots {
    used: Mutex::new(0),
    freed: Condvar::new(),
};

/// One of the fleet slots, held for the lifetime of a [`Fleet`].
pub struct Slot(());

impl Slot {
    fn acquire() -> Slot {
        let limit = std::env::var("BELLA_E2E_PARALLEL")
            .ok()
            .and_then(|text| text.parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_PARALLEL_FLEETS);
        let mut used = SLOTS.used.lock().unwrap_or_else(PoisonError::into_inner);
        while *used >= limit {
            used = SLOTS
                .freed
                .wait(used)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *used += 1;
        Slot(())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut used = SLOTS.used.lock().unwrap_or_else(PoisonError::into_inner);
        *used -= 1;
        SLOTS.freed.notify_one();
    }
}

/// What a finished `bella` command printed and returned.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    #[track_caller]
    pub fn ok(self) -> Run {
        assert_eq!(
            self.code, 0,
            "expected success\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not one JSON document ({error})\nstdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

/// Poll `check` until it returns `Some`, or fail after `limit`.
#[track_caller]
pub fn eventually<T>(what: &str, limit: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            start.elapsed() < limit,
            "timed out after {limit:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A command for a step, run without the recipe's default shell so the
/// same file works everywhere: `/bin/sh -c` on Unix, `cmd.exe /d /c` on
/// Windows. Returns a TOML array.
pub fn script(unix: &str, windows: &str) -> String {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let cmd = format!(r"{root}\System32\cmd.exe");
        format!(
            "[{}, \"/d\", \"/c\", {}]",
            toml_str(&cmd),
            toml_str(windows)
        )
    } else {
        format!("[\"/bin/sh\", \"-c\", {}]", toml_str(unix))
    }
}

/// A command that sleeps for `secs` seconds.
pub fn sleep_script(secs: u32, before: &str) -> String {
    script(
        &format!("echo {before}; sleep {secs}"),
        &format!("echo {before} & ping -n {} 127.0.0.1 > nul", secs + 1),
    )
}

/// A TOML basic string.
pub fn toml_str(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Environment every spawned process gets: a private home and configuration
/// directory, and the fleet's credentials file.
fn isolate(command: &mut Command, root: &Path) {
    let home = root.join("home");
    command
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("BELLA_CREDENTIALS", root.join("credentials.json"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("BELLA_SERVICE_NAME")
        .env_remove("RUST_LOG");
}

/// Run a command to completion within [`COMMAND_LIMIT`], with `stdin` as
/// its input.
pub fn run_command(mut command: Command, stdin: Option<&str>) -> Run {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("the command starts");
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).expect("stdin is written");
    }
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the command can be waited for") {
            break status;
        }
        if start.elapsed() > COMMAND_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{command:?} did not finish within {COMMAND_LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Run {
        code: status.code().unwrap_or(-1),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    }
}

/// A long-running `bella` process with its output in files.
pub struct Daemon {
    pub name: String,
    child: Option<Child>,
    pub log: PathBuf,
}

impl Daemon {
    fn spawn(name: &str, root: &Path, args: &[&str]) -> Daemon {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let log = root.join("logs").join(format!(
            "{name}-{}.log",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(BELLA);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file);
        isolate(&mut command, root);
        let child = command.spawn().expect("the daemon starts");
        Daemon {
            name: name.into(),
            child: Some(child),
            log,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or_default()
    }

    pub fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// End the process at once, the way a crash or power loss would.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Ask the process to stop and wait for it; kill it if it takes too
    /// long. On Windows there is no polite signal for a console process
    /// started this way, so it is killed.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if cfg!(unix) {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Send `SIGINT`, as Ctrl-C in a terminal does.
    #[cfg(unix)]
    pub fn interrupt(&self) {
        if let Some(child) = &self.child {
            let _ = Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status();
        }
    }

    /// Wait for the process to exit on its own and return its status.
    pub fn wait_exit(&mut self, limit: Duration) -> Option<i32> {
        let child = self.child.as_mut()?;
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Ok(Some(status)) = child.try_wait() {
                self.child = None;
                return Some(status.code().unwrap_or(-1));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Worker {
    pub name: String,
    pub data_dir: PathBuf,
    pub work_root: PathBuf,
    pub daemon: Option<Daemon>,
}

/// One controller, its workers and everything they write, in one temporary
/// directory.
pub struct Fleet {
    pub controller: Option<Daemon>,
    pub workers: Vec<Worker>,
    pub url: String,
    pub fingerprint: String,
    pub port: u16,
    pub controller_dir: PathBuf,
    /// Declared last: removed after every process above has stopped.
    pub root: tempfile::TempDir,
    _slot: Slot,
}

pub struct Options {
    pub workers: usize,
    pub profile: &'static str,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            workers: 1,
            profile: "dedicated-builder",
        }
    }
}

impl Fleet {
    pub fn start(options: Options) -> Fleet {
        let slot = Slot::acquire();
        let root = tempfile::Builder::new()
            .prefix("bella-e2e-")
            .tempdir()
            .expect("a temporary directory");
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        let controller_dir = root.path().join("controller");
        let mut fleet = Fleet {
            controller: None,
            workers: Vec::new(),
            url: String::new(),
            fingerprint: String::new(),
            port: 0,
            controller_dir,
            root,
            _slot: slot,
        };
        fleet.start_controller("127.0.0.1:0");
        fleet
            .bella(&[
                "login",
                &fleet.url.clone(),
                "--fingerprint",
                &fleet.fingerprint.clone(),
                "--token-file",
                &fleet
                    .controller_dir
                    .join("admin-token")
                    .display()
                    .to_string(),
            ])
            .ok();
        for index in 0..options.workers {
            fleet.add_worker(&format!("worker-{index}"), options.profile);
        }
        fleet.wait_for_connected(options.workers);
        fleet
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    fn start_controller(&mut self, listen: &str) {
        let ready = self.path().join("ready.json");
        let _ = std::fs::remove_file(&ready);
        let daemon = Daemon::spawn(
            "controller",
            self.root.path(),
            &[
                "controller",
                "run",
                "--data-dir",
                &self.controller_dir.display().to_string(),
                "--listen",
                listen,
                "--ready-file",
                &ready.display().to_string(),
            ],
        );
        self.controller = Some(daemon);
        let document: Value = eventually("the controller's ready file", PATIENCE, || {
            let text = std::fs::read_to_string(&ready).ok()?;
            serde_json::from_str(&text).ok()
        });
        self.url = document["url"].as_str().unwrap().to_string();
        self.fingerprint = document["fingerprint"].as_str().unwrap().to_string();
        self.port = self
            .url
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .unwrap();
    }

    /// Kill the controller and start it again on the same address and data
    /// directory.
    pub fn restart_controller(&mut self) {
        if let Some(mut controller) = self.controller.take() {
            controller.kill();
        }
        let listen = format!("127.0.0.1:{}", self.port);
        self.start_controller(&listen);
    }

    pub fn stop_controller(&mut self) {
        if let Some(mut controller) = self.controller.take() {
            controller.kill();
        }
    }

    /// Pair, enroll and start a worker.
    pub fn add_worker(&mut self, name: &str, profile: &str) -> usize {
        let enrollment = self
            .bella(&["controller", "pair", "--profile", profile])
            .ok()
            .stdout
            .trim()
            .to_string();
        let data_dir = self.path().join("workers").join(name);
        let work_root = self.path().join("w").join(name);
        self.bella(&[
            "worker",
            "enroll",
            &enrollment,
            "--data-dir",
            &data_dir.display().to_string(),
            "--name",
            name,
        ])
        .ok();
        self.workers.push(Worker {
            name: name.into(),
            data_dir,
            work_root,
            daemon: None,
        });
        let index = self.workers.len() - 1;
        self.start_worker(index);
        index
    }

    pub fn start_worker(&mut self, index: usize) {
        let worker = &self.workers[index];
        let daemon = Daemon::spawn(
            &worker.name,
            self.root.path(),
            &[
                "worker",
                "run",
                "--data-dir",
                &worker.data_dir.display().to_string(),
                "--work-root",
                &worker.work_root.display().to_string(),
            ],
        );
        self.workers[index].daemon = Some(daemon);
    }

    pub fn kill_worker(&mut self, index: usize) {
        if let Some(mut daemon) = self.workers[index].daemon.take() {
            daemon.kill();
        }
    }

    pub fn stop_worker(&mut self, index: usize) {
        if let Some(mut daemon) = self.workers[index].daemon.take() {
            daemon.stop();
        }
    }

    pub fn worker_dir(&self, index: usize) -> String {
        self.workers[index].data_dir.display().to_string()
    }

    /// The fleet view, as JSON.
    pub fn fleet_view(&self) -> Value {
        self.bella_json(&["fleet"])
    }

    pub fn wait_for_connected(&self, count: usize) {
        eventually("workers to connect", PATIENCE, || {
            let fleet = self.bella(&["--json", "fleet"]);
            if fleet.code != 0 {
                return None;
            }
            let view = fleet.json();
            let connected = view["machines"]
                .as_array()?
                .iter()
                .filter(|machine| machine["connected"] == Value::Bool(true))
                .count();
            (connected >= count && view["reconciling"] == Value::Bool(false)).then_some(())
        });
    }

    /// Start `bella` in the background with the fleet's environment.
    pub fn spawn(&self, name: &str, args: &[&str]) -> Daemon {
        Daemon::spawn(name, self.path(), args)
    }

    /// Run `bella` with the fleet's environment in the fleet's directory.
    pub fn bella(&self, args: &[&str]) -> Run {
        self.bella_in(self.path(), args, None)
    }

    pub fn bella_in(&self, dir: &Path, args: &[&str], stdin: Option<&str>) -> Run {
        let mut command = Command::new(BELLA);
        command.args(args).current_dir(dir);
        isolate(&mut command, self.path());
        run_command(command, stdin)
    }

    /// `bella --json <args>`, which must succeed.
    #[track_caller]
    pub fn bella_json(&self, args: &[&str]) -> Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        self.bella(&full).ok().json()
    }

    pub fn job(&self, id: &str) -> Value {
        self.bella_json(&["job", id])
    }

    /// Wait until the job's phase is `phase`.
    pub fn wait_for_phase(&self, id: &str, phases: &[&str]) -> Value {
        eventually(&format!("job {id} to reach {phases:?}"), PATIENCE, || {
            let job = self.job(id);
            phases.contains(&job["phase"].as_str()?).then_some(job)
        })
    }

    /// A git repository below the fleet directory with these files
    /// committed.
    pub fn repo(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.path().join("repos").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q"]);
        for (path, content) in files {
            write(&dir.join(path), content);
        }
        self.git(&dir, &["add", "-A"]);
        self.git(
            &dir,
            &[
                "-c",
                "user.name=bella-e2e",
                "-c",
                "user.email=e2e@example.invalid",
                "commit",
                "-qm",
                "initial",
            ],
        );
        dir
    }

    pub fn git(&self, dir: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.args(args).current_dir(dir);
        isolate(&mut command, self.path());
        let run = run_command(command, None);
        assert_eq!(run.code, 0, "git {args:?} failed: {}", run.stderr);
    }

    /// Everything the daemons wrote, for a failure message.
    pub fn daemon_logs(&self) -> String {
        let mut text = String::new();
        if let Some(controller) = &self.controller {
            text.push_str(&tail("controller", &controller.output()));
        }
        for worker in &self.workers {
            if let Some(daemon) = &worker.daemon {
                text.push_str(&tail(&worker.name, &daemon.output()));
            }
        }
        text
    }
}

fn tail(name: &str, text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(40);
    format!("--- {name} ---\n{}\n", lines[start..].join("\n"))
}

impl Drop for Fleet {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", self.daemon_logs());
        }
        // Workers first, so they can still report their cleanup.
        for worker in &mut self.workers {
            if let Some(mut daemon) = worker.daemon.take() {
                daemon.stop();
            }
        }
        if let Some(mut controller) = self.controller.take() {
            controller.stop();
        }
        // For debugging: keep everything the fleet wrote.
        if std::env::var_os("BELLA_E2E_KEEP").is_some() {
            let kept = std::mem::replace(
                &mut self.root,
                tempfile::tempdir().expect("a temporary directory"),
            );
            eprintln!("kept {}", kept.keep().display());
        }
    }
}

pub fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// A `bella mcp` process driven over its standard streams.
pub struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
    /// Every line seen on standard output.
    pub transcript: Vec<String>,
}

impl McpSession {
    pub fn start(fleet: &Fleet, root: &Path) -> McpSession {
        let mut command = Command::new(BELLA);
        command
            .args(["mcp", "--root", &root.display().to_string()])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        isolate(&mut command, fleet.path());
        let mut child = command.spawn().expect("bella mcp starts");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        McpSession {
            child,
            stdin,
            lines,
            next_id: 1,
            transcript: Vec::new(),
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&message);
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request and return its `result`, failing on a JSON-RPC
    /// error. Every line read must be a JSON-RPC message.
    #[track_caller]
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&message);
        let start = Instant::now();
        loop {
            let remaining = Duration::from_secs(90).saturating_sub(start.elapsed());
            let line = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("no answer to {method}"));
            self.transcript.push(line.clone());
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("stdout carried a non-JSON line `{line}`: {error}"));
            assert_eq!(value["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
            if value["id"] == serde_json::json!(id) {
                if let Some(error) = value.get("error") {
                    panic!("{method} failed: {error}");
                }
                return value["result"].clone();
            }
        }
    }

    /// `tools/call`, returning the structured result; fails when the tool
    /// reports an error.
    #[track_caller]
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": arguments }),
        );
        assert_ne!(
            result["isError"],
            Value::Bool(true),
            "{tool} failed: {result}"
        );
        result["structuredContent"].clone()
    }

    /// Close standard input and wait for the server to exit.
    pub fn close(mut self) -> i32 {
        drop(self.stdin.take());
        let start = Instant::now();
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code().unwrap_or(-1);
            }
            if start.elapsed() > Duration::from_secs(30) {
                let _ = self.child.kill();
                panic!("bella mcp did not exit after its input closed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
