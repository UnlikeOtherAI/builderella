//! Round trips against the host's real service manager. They install a
//! throwaway service and so run only with `BELLA_SERVICE_LIVE_TESTS=1`: on
//! Windows from an elevated prompt, on macOS for a user with a graphical
//! session. The service is removed again even when the test fails.

#![cfg(any(windows, target_os = "macos"))]

use bella_service::{
    InstallOptions, InstallOutcome, Role, Scope, ServiceError, ServiceId, ServiceSpec,
    ServiceStatus, install, start, status, stop, uninstall,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(90);
const SCOPE: Scope = if cfg!(windows) {
    Scope::System
} else {
    Scope::User
};

/// Arguments that have to survive every layer of quoting.
const AWKWARD: [&str; 7] = [
    "two words",
    "quo\"ted",
    "ünïcode",
    r"trailing slash\",
    "",
    "100% $HOME ${USER} %s",
    "<a & 'b'>;",
];

/// Removes the service when the test ends, however it ends.
struct Installation(ServiceId);

impl Drop for Installation {
    fn drop(&mut self) {
        if let Err(error) = uninstall(&self.0) {
            eprintln!("could not remove test service {}: {error}", self.0.name);
        }
    }
}

fn probe() -> PathBuf {
    // target/<profile>/deps/live-… → target/<profile>/examples/service_probe
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("examples");
    path.push(format!("service_probe{}", std::env::consts::EXE_SUFFIX));
    assert!(path.is_file(), "build the examples first: {path:?}");
    path
}

fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(200));
    }
}

fn running_pid(id: &ServiceId) -> u32 {
    eventually("a running service", || match status(id).unwrap() {
        ServiceStatus::Running { pid: Some(pid) } => Some(pid),
        _ => None,
    })
}

/// The probe's record once the process `pid` has written it.
fn record_of(path: &Path, pid: u32) -> String {
    eventually("the probe's record", || {
        fs::read_to_string(path)
            .ok()
            .filter(|text| text.starts_with(&format!("pid={pid}\n")))
    })
}

fn kill(pid: u32) {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new("taskkill.exe");
        command
            .args(["/F", "/PID"])
            .creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("/bin/kill");
        command.arg("-9");
        command
    };
    assert!(command.arg(pid.to_string()).status().unwrap().success());
}

#[test]
fn live_install_run_restart_stop_uninstall() {
    if std::env::var("BELLA_SERVICE_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("not run: set BELLA_SERVICE_LIVE_TESTS=1 to install a real service");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    // `canonicalize` yields a verbatim path on Windows, which services dislike.
    let base = PathBuf::from(base.to_str().unwrap().trim_start_matches(r"\\?\"));
    let record = base.join("record file.txt");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let mut spec = ServiceSpec::new(
        Role::Controller,
        SCOPE,
        probe(),
        base.join("work dir ü"),
        base.join("logs"),
    );
    spec.name = format!("builderella-test-{}-{nonce:08x}", std::process::id());
    spec.args = std::iter::once(record.to_str().unwrap().to_owned())
        .chain(AWKWARD.map(str::to_owned))
        .collect();
    let value = "a b & <c> \"d\" 'e' ü 100% $HOME \\";
    spec.env.insert("PROBE_VALUE".into(), value.into());
    let id = spec.id();
    assert_eq!(status(&id).unwrap(), ServiceStatus::NotInstalled);

    let installation = Installation(id.clone());
    let installed = match install(&spec, &InstallOptions::default()) {
        Err(error @ ServiceError::NoGuiSession { .. }) => {
            eprintln!("not run: {error}");
            return;
        }
        other => other.unwrap(),
    };
    eprintln!("installed {}: {}", installed.name, installed.status);
    assert_eq!(installed.outcome, InstallOutcome::Created);
    let first = running_pid(&id);
    let text = record_of(&record, first);
    for argument in AWKWARD {
        assert!(
            text.contains(&format!("arg={argument}\n")),
            "argument {argument:?} missing from {text}"
        );
    }
    assert_eq!(text.matches("arg=").count(), AWKWARD.len(), "{text}");
    assert!(text.contains(&format!("value={value}\n")), "{text}");
    assert!(text.contains("work dir ü\n"), "{text}");
    if cfg!(windows) {
        let log_dir = format!("log_dir={}\n", spec.log_dir.display());
        assert!(text.contains(&log_dir), "{text}");
    }

    // Identical: nothing changes and the process keeps running.
    let again = install(&spec, &InstallOptions::default()).unwrap();
    assert_eq!(again.outcome, InstallOutcome::Unchanged);
    assert_eq!(running_pid(&id), first);

    // Different: refused without `replace`, applied and restarted with it.
    spec.env.insert("PROBE_VALUE".into(), "changed".into());
    let refused = install(&spec, &InstallOptions::default()).unwrap_err();
    assert!(
        matches!(refused, ServiceError::AlreadyInstalled { .. }),
        "{refused}"
    );
    assert_eq!(running_pid(&id), first);
    let replace = InstallOptions {
        replace: true,
        ..InstallOptions::default()
    };
    let replaced = install(&spec, &replace).unwrap();
    assert_eq!(replaced.outcome, InstallOutcome::Replaced);
    let second = running_pid(&id);
    assert_ne!(second, first);
    assert!(record_of(&record, second).contains("value=changed\n"));

    // A crash is followed by a restart.
    kill(second);
    let third = eventually("a restart after the crash", || match status(&id).unwrap() {
        ServiceStatus::Running { pid: Some(pid) } if pid != second => Some(pid),
        _ => None,
    });
    record_of(&record, third);
    eprintln!("restarted after a crash: pid {second} -> {third}");

    // So is a body that returns an error, which on Windows is a reported stop
    // rather than a crash.
    #[cfg(windows)]
    {
        fs::write(record.with_extension("fail"), "fail").unwrap();
        let fourth = eventually("a restart after the error", || match status(&id).unwrap() {
            ServiceStatus::Running { pid: Some(pid) } if pid != third => Some(pid),
            _ => None,
        });
        record_of(&record, fourth);
        eprintln!("restarted after an error: pid {third} -> {fourth}");
    }

    stop(&id).unwrap();
    assert_eq!(status(&id).unwrap(), ServiceStatus::Stopped);
    stop(&id).unwrap();
    #[cfg(windows)]
    assert!(
        record.with_extension("stopped").is_file(),
        "the body did not see the stop request"
    );
    start(&id).unwrap();
    running_pid(&id);
    start(&id).unwrap();
    stop(&id).unwrap();

    assert!(uninstall(&id).unwrap());
    assert_eq!(status(&id).unwrap(), ServiceStatus::NotInstalled);
    assert!(!uninstall(&id).unwrap());
    if !cfg!(windows) {
        assert!(!installed.definition.exists());
    }
    drop(installation);
    eprintln!("stopped, restarted and removed {}", id.name);
}
