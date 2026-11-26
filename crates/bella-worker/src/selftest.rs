//! Commissioning check of a machine, without a controller.

use crate::env;
use bella_contain::{Containment, Limits, Membership, ProcessSpec};
use bella_core::machine::{Hardware, OsInfo, Telemetry};
use bella_core::platform::Capability;
use bella_core::resources::EnforcementReport;
use bella_core::BootId;
use bella_probe::{ManagedUsage, Sampler};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// Text the managed command of the self-test prints.
const MARKER: &str = "bella-self-test";
/// How long the managed command may take.
const COMMAND_LIMIT: Duration = Duration::from_secs(30);

/// Result of running one tiny command the way a step runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCheck {
    /// The command printed the marker, exited with status 0, its scope was
    /// verified empty afterwards and its scratch directory was removed.
    pub ok: bool,
    /// What happened, including what failed.
    pub detail: String,
}

/// What [`self_test`] found.
#[derive(Clone, Debug)]
pub struct SelfTestReport {
    /// Mechanism of the containment adapter: `cgroup-v2`, `job-object`,
    /// `process-group`.
    pub mechanism: String,
    /// The adapter's own commissioning result, or why it failed.
    pub enforcement: Result<EnforcementReport, String>,
    pub hardware: Hardware,
    pub os: OsInfo,
    pub boot_id: BootId,
    pub capabilities: Vec<Capability>,
    /// One telemetry sample of the host.
    pub telemetry: Telemetry,
    pub command: CommandCheck,
}

impl SelfTestReport {
    /// The adapter verified itself and a managed command ran end to end.
    pub fn passed(&self) -> bool {
        self.command.ok
            && self
                .enforcement
                .as_ref()
                .is_ok_and(|report| report.verified)
    }
}

/// Check that this machine can do a worker's job: run the containment
/// adapter's self-test, probe the machine, and run one small command in a
/// scope with a step environment, from spawn to verified-empty scope.
///
/// `work_root` is where attempt workspaces would live; a scratch directory
/// is created below it and removed again. `containment` of `None` uses
/// [`bella_contain::native`]. Nothing is journaled and no controller is
/// contacted.
pub fn self_test(work_root: &Path, containment: Option<&dyn Containment>) -> SelfTestReport {
    let native;
    let (mechanism, enforcement, command) = match containment {
        Some(containment) => check(work_root, containment),
        None => match bella_contain::native() {
            Ok(adapter) => {
                native = adapter;
                check(work_root, native.as_ref())
            }
            Err(error) => (
                "none".to_string(),
                Err(error.to_string()),
                CommandCheck {
                    ok: false,
                    detail: format!("no containment adapter is available: {error}"),
                },
            ),
        },
    };
    let mut sampler = Sampler::new(work_root);
    SelfTestReport {
        mechanism,
        enforcement,
        hardware: bella_probe::hardware(),
        os: bella_probe::os_info(),
        boot_id: bella_probe::boot_id(),
        capabilities: bella_probe::detect_capabilities(&bella_probe::ProbeOptions::default()),
        telemetry: sampler.sample(ManagedUsage::default()),
        command,
    }
}

fn check(
    work_root: &Path,
    containment: &dyn Containment,
) -> (String, Result<EnforcementReport, String>, CommandCheck) {
    let enforcement = containment.self_test().map_err(|error| error.to_string());
    let command = match managed_command(work_root, containment) {
        Ok(detail) => CommandCheck { ok: true, detail },
        Err(detail) => CommandCheck { ok: false, detail },
    };
    (containment.mechanism().to_string(), enforcement, command)
}

fn managed_command(work_root: &Path, containment: &dyn Containment) -> Result<String, String> {
    let scratch = work_root.join(format!("self-test-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("the work root is not writable: {error}"))?;
    let result = run_in_scope(&scratch, containment);
    let removed = bella_materialize::remove_tree(&scratch);
    let detail = result?;
    removed.map_err(|error| format!("the scratch directory could not be removed: {error}"))?;
    Ok(detail)
}

fn run_in_scope(scratch: &Path, containment: &dyn Containment) -> Result<String, String> {
    let mut environment = env::system_env();
    let tmp = scratch.to_string_lossy().into_owned();
    for name in if cfg!(windows) {
        &["TEMP", "TMP"][..]
    } else {
        &["TMPDIR"][..]
    } {
        environment.insert((*name).to_string(), tmp.clone());
    }
    let argv: Vec<String> = if cfg!(windows) {
        vec!["cmd.exe".into(), "/d".into(), "/c".into(), format!("echo {MARKER}")]
    } else {
        vec!["/bin/sh".into(), "-c".into(), format!("echo {MARKER}")]
    };
    let name = format!("self-test-{}", std::process::id());
    let mut scope = containment
        .create(&name, &Limits::default())
        .map_err(|error| format!("a scope could not be created: {error}"))?;
    let spawned = scope.spawn(&ProcessSpec {
        argv,
        cwd: scratch.to_path_buf(),
        env: environment,
    });
    let outcome = match spawned {
        Ok(mut spawned) => {
            let mut stdout = spawned.stdout;
            let mut stderr = spawned.stderr;
            let reader = std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stdout.read_to_string(&mut text);
                text
            });
            let drain = std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = stderr.read_to_end(&mut sink);
            });
            let started = Instant::now();
            let exit = loop {
                match spawned.child.try_wait() {
                    Ok(Some(exit)) => break Ok(exit),
                    Ok(None) if started.elapsed() >= COMMAND_LIMIT => {
                        break Err("the command did not finish in time".to_string());
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(error) => break Err(format!("the command could not be waited for: {error}")),
                }
            };
            let stopped =
                bella_contain::stop_scope(scope.as_mut(), Duration::from_secs(2), Duration::from_secs(10));
            let text = reader.join().unwrap_or_default();
            let _ = drain.join();
            exit.and_then(|exit| {
                if !exit.success() {
                    return Err(format!("the command exited with {:?}", exit.code));
                }
                if !text.contains(MARKER) {
                    return Err("the command's output was not captured".to_string());
                }
                match stopped {
                    Ok(Membership::Empty) => Ok(()),
                    Ok(Membership::Alive(count)) => {
                        Err(format!("{count} process(es) were still alive in the scope"))
                    }
                    Ok(Membership::Unknown) => {
                        Err("the adapter could not tell whether the scope was empty".to_string())
                    }
                    Err(error) => Err(format!("the scope could not be stopped: {error}")),
                }
            })
        }
        Err(error) => {
            let _ =
                bella_contain::stop_scope(scope.as_mut(), Duration::ZERO, Duration::from_secs(10));
            Err(format!("the command could not be started: {error}"))
        }
    };
    let destroyed = scope
        .destroy()
        .map_err(|error| format!("the scope could not be removed: {error}"));
    outcome?;
    let method = destroyed?;
    Ok(format!(
        "a managed command ran, its output was captured and its scope was verified empty ({method})"
    ))
}
