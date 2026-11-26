//! Bounded execution of external probe commands.
//!
//! Every command runs with standard input closed, a hard deadline, captured
//! and size-limited output, and its own process tree so that a timeout kills
//! helpers the tool started. The environment of this process is never
//! modified; extra search directories only affect the child.

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How capability probes run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeOptions {
    /// Hard limit for each external command. A command that exceeds it is
    /// killed together with the processes it started.
    pub timeout: Duration,
    /// Directories searched after `PATH` when locating tools. They are also
    /// appended to the `PATH` the probed tools see. `PATH` of this process
    /// is left untouched.
    pub extra_path: Vec<PathBuf>,
}

impl ProbeOptions {
    /// Default per-command timeout.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            timeout: Self::DEFAULT_TIMEOUT,
            extra_path: Vec::new(),
        }
    }
}

/// Largest amount kept from each output stream. More is read and dropped so
/// the tool never blocks on a full pipe.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// How long to wait for the output pipes to reach end-of-file after the
/// command has exited. A helper that inherited the pipes can keep them open.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Settings applied to every child so that asking a tool for its version
/// cannot install, download or prompt.
const CHILD_ENV: &[(&str, &str)] = &[
    // rustup proxies must not install a missing toolchain.
    ("RUSTUP_AUTO_INSTALL", "0"),
    // corepack shims for pnpm/yarn must not download a package manager.
    ("COREPACK_ENABLE_NETWORK", "0"),
    ("NO_UPDATE_NOTIFIER", "1"),
    ("DOTNET_NOLOGO", "1"),
    ("DOTNET_CLI_TELEMETRY_OPTOUT", "1"),
    ("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1"),
];

/// Captured result of a command that exited by itself.
#[derive(Clone, Debug, Default)]
pub(crate) struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// Standard output followed by standard error. Several tools print
    /// their version on either stream depending on the release.
    pub fn combined(&self) -> String {
        let mut text = self.stdout.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&self.stderr);
        text
    }

    /// Short description of the exit status for diagnostics.
    pub fn status_text(&self) -> String {
        match self.code {
            Some(code) => format!("exit status {code}"),
            None => "terminated by a signal".to_string(),
        }
    }
}

/// Why a command produced no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunFailure {
    Spawn(String),
    TimedOut(Duration),
    Wait(String),
}

impl fmt::Display for RunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "could not start: {e}"),
            Self::TimedOut(limit) => write!(f, "timed out after {} ms", limit.as_millis()),
            Self::Wait(e) => write!(f, "could not wait for exit: {e}"),
        }
    }
}

/// Locates and runs probe commands with the options of one detection pass.
pub(crate) struct Runner {
    timeout: Duration,
    dirs: Vec<PathBuf>,
    /// `PATH` for children; `None` inherits this process's value unchanged.
    child_path: Option<OsString>,
    cwd: Option<PathBuf>,
}

impl Runner {
    pub fn new(options: &ProbeOptions) -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        let child_path = if options.extra_path.is_empty() {
            None
        } else {
            dirs.extend(options.extra_path.iter().cloned());
            std::env::join_paths(dirs.iter()).ok()
        };
        Self::build(options.timeout, dirs, child_path)
    }

    /// A runner that searches only `dirs`. Children still inherit `PATH`.
    #[cfg(test)]
    pub fn with_dirs(timeout: Duration, dirs: Vec<PathBuf>) -> Self {
        Self::build(timeout, dirs, None)
    }

    fn build(timeout: Duration, dirs: Vec<PathBuf>, child_path: Option<OsString>) -> Self {
        // Probes run from a neutral directory so that per-directory tool
        // configuration (rust-toolchain.toml, global.json, .nvmrc, ...) of
        // whatever directory the worker was started in does not apply.
        let cwd = Some(std::env::temp_dir()).filter(|dir| dir.is_dir());
        Self {
            timeout,
            dirs: dirs
                .into_iter()
                .filter(|d| !d.as_os_str().is_empty())
                .collect(),
            child_path,
            cwd,
        }
    }

    /// First executable called `name` in the search directories.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        self.dirs.iter().find_map(|dir| executable_in(dir, name))
    }

    /// Run `program` with `args` under this runner's timeout.
    pub fn run(&self, program: &Path, args: &[&str]) -> Result<CommandOutput, RunFailure> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(path) = &self.child_path {
            command.env("PATH", path);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        run_command(command, self.timeout)
    }
}

/// `dir/name` if it is an executable file, trying the platform's executable
/// extensions when `name` has none.
pub(crate) fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        if Path::new(name).extension().is_some() {
            let candidate = dir.join(name);
            return is_executable(&candidate).then_some(candidate);
        }
        ["exe", "cmd", "bat", "com"].iter().find_map(|ext| {
            let candidate = dir.join(format!("{name}.{ext}"));
            is_executable(&candidate).then_some(candidate)
        })
    } else {
        let candidate = dir.join(name);
        is_executable(&candidate).then_some(candidate)
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    // App execution aliases are reparse points that cannot be opened like a
    // file, so existence is judged without following the link.
    std::fs::symlink_metadata(path)
        .map(|meta| !meta.is_dir())
        .unwrap_or(false)
}

/// Run a prepared command with closed standard input and a hard deadline.
pub(crate) fn run_command(
    mut command: Command,
    timeout: Duration,
) -> Result<CommandOutput, RunFailure> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in CHILD_ENV {
        command.env(key, value);
    }
    tree::prepare(&mut command);

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| RunFailure::Spawn(e.to_string()))?;
    let guard = tree::Guard::attach(&child);
    let stdout = Capture::start(child.stdout.take());
    let stderr = Capture::start(child.stderr.take());

    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill_tree(&mut child, &guard);
                return Err(RunFailure::Wait(e.to_string()));
            }
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            kill_tree(&mut child, &guard);
            return Err(RunFailure::TimedOut(timeout));
        }
        std::thread::sleep(pause.min(timeout - elapsed));
        pause = (pause * 2).min(Duration::from_millis(20));
    };

    let drain_until = Instant::now() + DRAIN_GRACE;
    let stdout = stdout.finish(drain_until);
    let stderr = stderr.finish(drain_until);
    // `guard` is released on return, which also ends anything the command
    // left running in its job on Windows.
    Ok(CommandOutput {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

fn kill_tree(child: &mut Child, guard: &tree::Guard) {
    guard.kill(child);
    // The direct child may already be gone; either way it is reaped so no
    // zombie outlives the probe.
    let _ = child.kill();
    let _ = child.wait();
}

/// Collects one output stream on a helper thread.
struct Capture {
    buffer: Arc<Mutex<Vec<u8>>>,
    done: Option<mpsc::Receiver<()>>,
}

impl Capture {
    fn start<R: Read + Send + 'static>(stream: Option<R>) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let Some(mut stream) = stream else {
            return Self { buffer, done: None };
        };
        let (tx, rx) = mpsc::channel();
        let sink = Arc::clone(&buffer);
        let spawned = std::thread::Builder::new()
            .name("bella-probe-capture".into())
            .spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let Ok(mut held) = sink.lock() else { break };
                            let room = MAX_CAPTURE_BYTES.saturating_sub(held.len());
                            held.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                }
                let _ = tx.send(());
            });
        Self {
            buffer,
            done: spawned.is_ok().then_some(rx),
        }
    }

    /// Text captured so far once the stream ends or `until` passes.
    fn finish(self, until: Instant) -> String {
        if let Some(done) = &self.done {
            let _ = done.recv_timeout(until.saturating_duration_since(Instant::now()));
        }
        let bytes = self
            .buffer
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(unix)]
mod tree {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    /// Put the child in its own process group so the whole group can be
    /// signalled on timeout.
    pub fn prepare(command: &mut Command) {
        command.process_group(0);
    }

    pub struct Guard;

    impl Guard {
        pub fn attach(_child: &Child) -> Self {
            Guard
        }

        pub fn kill(&self, child: &Child) {
            let Ok(pid) = libc::pid_t::try_from(child.id()) else {
                return;
            };
            if pid <= 1 {
                return;
            }
            // SAFETY: `kill` has no memory-safety preconditions. The child
            // has not been reaped yet, so its pid, which is also its process
            // group id, cannot have been reused by an unrelated process.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(windows)]
mod tree {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    /// Probes never open a console window, also when the worker has none.
    pub fn prepare(command: &mut Command) {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    /// A job object holding the child and everything it starts. Closing the
    /// job ends whatever is still running in it.
    pub struct Guard {
        job: HANDLE,
    }

    impl Guard {
        pub fn attach(child: &Child) -> Self {
            // SAFETY: both pointer arguments may be null: default security
            // attributes and an unnamed job.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Self { job };
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job` is a valid job handle owned by this guard,
            // `limits` is a live JOBOBJECT_EXTENDED_LIMIT_INFORMATION of the
            // size passed, and the child handle is valid while `child` lives.
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                AssignProcessToJobObject(job, child.as_raw_handle());
            }
            Self { job }
        }

        pub fn kill(&self, _child: &Child) {
            if !self.job.is_null() {
                // SAFETY: `job` is a valid job handle owned by this guard.
                unsafe {
                    TerminateJobObject(self.job, 1);
                }
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if !self.job.is_null() {
                // SAFETY: the handle was returned by CreateJobObjectW and is
                // closed exactly once, here.
                unsafe {
                    CloseHandle(self.job);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_runner(timeout: Duration) -> Runner {
        Runner::new(&ProbeOptions {
            timeout,
            extra_path: Vec::new(),
        })
    }

    #[test]
    fn missing_program_is_not_found() {
        let runner = system_runner(Duration::from_secs(5));
        assert!(runner.find("bella-probe-no-such-tool").is_none());
    }

    #[test]
    fn extra_path_is_searched_without_touching_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "extratool.cmd"
        } else {
            "extratool"
        };
        let path = dir.path().join(name);
        std::fs::write(&path, "exit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let before = std::env::var_os("PATH");
        let runner = Runner::new(&ProbeOptions {
            timeout: Duration::from_secs(5),
            extra_path: vec![dir.path().to_path_buf()],
        });
        assert_eq!(runner.find("extratool"), Some(path));
        assert_eq!(std::env::var_os("PATH"), before);
    }

    #[test]
    fn stdin_is_closed_so_readers_return_at_once() {
        let runner = system_runner(Duration::from_secs(20));
        let reader = if cfg!(windows) { "sort" } else { "cat" };
        let program = runner.find(reader).expect("stdin reader on PATH");
        let started = Instant::now();
        let output = runner.run(&program, &[]).expect("runs");
        assert!(output.success);
        assert_eq!(output.stdout.trim(), "");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_and_exit_status_are_captured() {
        let runner = system_runner(Duration::from_secs(20));
        let (shell, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/d", "/c", "echo out& echo err 1>&2& exit /b 3"])
        } else {
            ("sh", &["-c", "echo out; echo err >&2; exit 3"])
        };
        let program = runner.find(shell).expect("shell on PATH");
        let output = runner.run(&program, args).expect("runs");
        assert!(!output.success);
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");
        assert_eq!(output.combined().lines().count(), 2);
    }

    #[test]
    fn hung_command_is_killed_at_the_deadline() {
        let runner = system_runner(Duration::from_millis(400));
        let (shell, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/d", "/c", "ping -n 60 127.0.0.1 >NUL"])
        } else {
            ("sh", &["-c", "sleep 60 & wait"])
        };
        let program = runner.find(shell).expect("shell on PATH");
        let started = Instant::now();
        let result = runner.run(&program, args);
        assert_eq!(
            result.err(),
            Some(RunFailure::TimedOut(Duration::from_millis(400)))
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    /// A timeout ends the helpers the command started, not just the command.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let runner = system_runner(Duration::from_millis(500));
        let program = runner.find("sh").expect("sh on PATH");
        assert!(runner.run(&program, &["-c", &script]).is_err());
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .expect("the script recorded its helper")
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 only checks whether the process exists.
            let exists = unsafe { libc::kill(pid, 0) } == 0;
            // An orphan that nobody reaped yet still has a process entry; it
            // is dead when its state is `Z`.
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|stat| {
                    stat.rsplit(')')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .starts_with('Z')
                })
                .unwrap_or(false);
            if !exists || zombie {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "helper {pid} survived the timeout"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn unstartable_program_reports_a_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nothing-here");
        let result = run_command(Command::new(missing), Duration::from_secs(5));
        assert!(matches!(result, Err(RunFailure::Spawn(_))));
    }
}
