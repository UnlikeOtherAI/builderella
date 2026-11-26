//! cgroup v2 adapter against a real kernel.
//!
//! Every test needs a delegated, writable cgroup v2 subtree and skips with a
//! message when [`CgroupV2::detect`] reports `Unavailable`. Set
//! `BELLA_REQUIRE_CGROUP=1` to turn a skip into a failure where the subtree
//! is expected, for example as root in a container with a private cgroup
//! namespace.
#![cfg(target_os = "linux")]

use bella_contain::linux::{CgroupScope, CgroupV2, oom_kills};
use bella_contain::{
    ContainError, Containment, ExitInfo, Limits, Membership, ProcessSpec, Scope, Spawned,
    stop_scope,
};
use bella_core::protocol::{ScopeIdentity, ScopeKind};
use bella_core::resources::Enforcement;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const HELPER_MODE: &str = "BELLA_CONTAIN_HELPER_MODE";
const HELPER_BASE: &str = "BELLA_CONTAIN_HELPER_BASE";
const NOBODY: u32 = 65534;

/// The tests start processes, force PIDs and measure CPU time, so they run
/// one at a time whatever `--test-threads` says.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The adapter, or `None` after printing why the test is skipped.
fn adapter() -> Option<CgroupV2> {
    match CgroupV2::detect() {
        Ok(adapter) => Some(adapter),
        Err(ContainError::Unavailable(reason)) => {
            assert!(
                std::env::var_os("BELLA_REQUIRE_CGROUP").is_none(),
                "cgroup v2 containment is required here but unavailable: {reason}"
            );
            // Written directly so the message is visible without --nocapture.
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED: cgroup v2 containment is unavailable: {reason}"
            );
            None
        }
        Err(other) => panic!("detect failed: {other}"),
    }
}

fn unique(tag: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "t{}-{}-{tag}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Kills and removes a scope when a test ends, however it ends.
struct Janitor(ScopeIdentity);

impl Drop for Janitor {
    fn drop(&mut self) {
        let Ok(adapter) = CgroupV2::detect() else {
            return;
        };
        if let Ok(Some(mut scope)) = adapter.adopt(&self.0) {
            let _ = scope.kill();
            wait_until(Duration::from_secs(5), || {
                matches!(scope.members(), Ok(Membership::Empty))
            });
            let _ = scope.destroy();
        }
    }
}

fn new_scope(adapter: &CgroupV2, tag: &str, limits: &Limits) -> (CgroupScope, Janitor) {
    let scope = adapter
        .create_scope(&unique(tag), limits)
        .expect("create scope");
    let janitor = Janitor(scope.identity());
    (scope, janitor)
}

fn sh(script: &str) -> ProcessSpec {
    ProcessSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: PathBuf::from("/"),
        env: BTreeMap::from([("PATH".to_string(), PATH.to_string())]),
    }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if done() {
            return true;
        }
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn expect_empty(scope: &dyn Scope) {
    assert!(
        wait_until(Duration::from_secs(5), || {
            matches!(scope.members(), Ok(Membership::Empty))
        }),
        "scope is still {:?}",
        scope.members()
    );
}

/// Drain both pipes, then wait for the root process.
fn finish(spawned: Spawned) -> (ExitInfo, String, String) {
    let Spawned {
        mut child,
        mut stdout,
        mut stderr,
    } = spawned;
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).expect("read stderr");
        text
    });
    let mut output = String::new();
    stdout.read_to_string(&mut output).expect("read stdout");
    let errors = errors.join().expect("stderr thread");
    (child.wait().expect("wait"), output, errors)
}

fn wait_exit(spawned: &mut Spawned, limit: Duration) -> ExitInfo {
    let mut exit = None;
    wait_until(limit, || {
        exit = spawned.child.try_wait().expect("try_wait");
        exit.is_some()
    });
    exit.expect("process did not exit in time")
}

/// Fields of `/proc/<pid>/stat` after the command name, or `None` when the
/// process no longer exists.
fn proc_stat(pid: u32) -> Option<Vec<String>> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1;
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Running or sleeping, as opposed to gone or a zombie nobody reaped.
fn is_alive(pid: u32) -> bool {
    proc_stat(pid).is_some_and(|fields| !matches!(fields[0].as_str(), "Z" | "X"))
}

fn session_of(pid: u32) -> Option<u32> {
    proc_stat(pid)?.get(3)?.parse().ok()
}

fn member_pids(scope: &CgroupScope) -> Vec<u32> {
    let mut pids: Vec<u32> = fs::read_to_string(scope.path().join("cgroup.procs"))
        .expect("cgroup.procs")
        .lines()
        .map(|line| line.parse().expect("pid"))
        .collect();
    pids.sort_unstable();
    pids
}

fn control(scope: &CgroupScope, file: &str) -> String {
    fs::read_to_string(scope.path().join(file))
        .unwrap_or_else(|error| panic!("{file}: {error}"))
        .trim()
        .to_string()
}

fn keyed(scope: &CgroupScope, file: &str, key: &str) -> u64 {
    control(scope, file)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.parse().ok()).flatten()
        })
        .unwrap_or_else(|| panic!("{file} has no {key}"))
}

fn destroy(scope: CgroupScope) -> Result<String, ContainError> {
    Box::new(scope).destroy()
}

fn is_root() -> bool {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn a11_one_scope_owns_every_spawn_and_one_kill_ends_them_all() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "a11", &Limits::default());
    let (mut other, _other_janitor) = new_scope(&adapter, "a11-other", &Limits::default());

    let mut children: Vec<Spawned> = (0..3)
        .map(|_| scope.spawn(&sh("exec sleep 300")).expect("spawn"))
        .collect();
    let mut bystander = other.spawn(&sh("exec sleep 300")).expect("spawn");

    let mut expected: Vec<u32> = children.iter().map(|spawned| spawned.child.pid()).collect();
    expected.sort_unstable();
    assert_eq!(member_pids(&scope), expected);
    assert_eq!(scope.members().unwrap(), Membership::Alive(3));
    assert_eq!(other.members().unwrap(), Membership::Alive(1));

    scope.kill().unwrap();
    for spawned in &mut children {
        let exit = wait_exit(spawned, Duration::from_secs(5));
        assert_eq!(exit.signal, Some(libc::SIGKILL));
        assert_eq!(exit.code, None);
    }
    expect_empty(&scope);

    // The neighbouring scope is a separate tree.
    assert_eq!(other.members().unwrap(), Membership::Alive(1));
    assert!(bystander.child.try_wait().unwrap().is_none());

    assert_eq!(
        destroy(scope).unwrap(),
        "cgroup.procs empty; cgroup removed"
    );
    other.kill().unwrap();
    wait_exit(&mut bystander, Duration::from_secs(5));
}

/// A16: the journal names a scope whose cgroup is gone and whose root PID now
/// belongs to an unrelated process. Adoption must report "gone" and leave
/// that process alone.
#[test]
fn a16_adopting_a_vanished_scope_returns_none_and_signals_nothing() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "a16", &Limits::default());
    let identity = scope.identity();
    let mut member = scope.spawn(&sh("exec sleep 300")).expect("spawn");
    let old_pid = member.child.pid();

    scope.kill().unwrap();
    wait_exit(&mut member, Duration::from_secs(5));
    expect_empty(&scope);
    destroy(scope).unwrap();

    // With enough privilege the kernel can be told which PID to hand out
    // next, which makes the reuse exact rather than hoped for.
    let mut bystander = None;
    let mut forced = false;
    for _ in 0..5 {
        forced = fs::write("/proc/sys/kernel/ns_last_pid", (old_pid - 1).to_string()).is_ok();
        let child = Command::new("sleep").arg("300").spawn().expect("bystander");
        if !forced || child.id() == old_pid {
            bystander = Some(child);
            break;
        }
        let mut child = child;
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let mut bystander = bystander.expect("the old PID could not be reused in five attempts");
    if forced {
        assert_eq!(bystander.id(), old_pid);
    } else {
        let _ = writeln!(
            std::io::stderr(),
            "note: PID reuse was not forced (cannot write ns_last_pid); the bystander has a different PID"
        );
    }

    assert!(adapter.adopt(&identity).unwrap().is_none());
    assert!(adapter.adopt_scope(&identity).unwrap().is_none());
    assert_eq!(oom_kills(&identity), None);
    std::thread::sleep(Duration::from_millis(100));
    assert!(bystander.try_wait().unwrap().is_none(), "bystander died");
    assert!(is_alive(bystander.id()));

    bystander.kill().unwrap();
    bystander.wait().unwrap();
}

/// A16 on a live scope: signals go to current members only, so an outside
/// process is untouched even while the scope is terminated and killed.
#[test]
fn a16_terminate_and_kill_reach_members_only() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "a16-live", &Limits::default());
    let mut member = scope.spawn(&sh("exec sleep 300")).expect("spawn");
    let mut outsider = Command::new("sleep").arg("300").spawn().expect("outsider");

    let mut adopted = adapter
        .adopt(&scope.identity())
        .unwrap()
        .expect("live scope");
    adopted.terminate().unwrap();
    assert_eq!(
        wait_exit(&mut member, Duration::from_secs(5)).signal,
        Some(libc::SIGTERM)
    );
    adopted.kill().unwrap();
    expect_empty(adopted.as_ref());
    assert!(outsider.try_wait().unwrap().is_none(), "outsider died");

    outsider.kill().unwrap();
    outsider.wait().unwrap();
    adopted.destroy().unwrap();
}

#[test]
fn a24_daemon_outlives_root_and_still_holds_the_scope() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "a24", &Limits::default());
    let identity = scope.identity();

    // The subshell starts the daemon in the background and exits, so the
    // daemon is an orphan before the root shell returns.
    let root = scope
        .spawn(&sh(
            "(while :; do :; done </dev/null >/dev/null 2>&1 &)\nexit 0",
        ))
        .expect("spawn");
    let root_pid = root.child.pid();
    let (exit, _, _) = finish(root);
    assert!(exit.success());

    assert_eq!(scope.members().unwrap(), Membership::Alive(1));
    let daemon = member_pids(&scope)[0];
    assert_ne!(daemon, root_pid);
    assert!(is_alive(daemon));

    // Usage keeps counting the daemon after the root is gone.
    let before = scope.usage().unwrap();
    assert_eq!(before.processes, 1);
    assert!(wait_until(Duration::from_secs(5), || {
        scope.usage().unwrap().cpu_time_ms >= before.cpu_time_ms + 100
    }));
    if control(&scope, "cgroup.controllers").contains("memory") {
        assert!(scope.usage().unwrap().memory_current_bytes > 0);
    }

    // Cleanup is refused while the daemon lives.
    match destroy(scope) {
        Err(ContainError::NotEmpty(name)) => assert_eq!(name, identity.name),
        other => panic!("destroy returned {other:?}"),
    }
    assert!(is_alive(daemon));

    let mut scope = adapter
        .adopt_scope(&identity)
        .unwrap()
        .expect("still there");
    assert_eq!(scope.members().unwrap(), Membership::Alive(1));
    scope.kill().unwrap();
    expect_empty(&scope);
    assert!(wait_until(Duration::from_secs(5), || !is_alive(daemon)));
    assert_eq!(
        destroy(scope).unwrap(),
        "cgroup.procs empty; cgroup removed"
    );
    assert!(!Path::new(&identity.detail).exists());
}

#[test]
fn a66_work_hidden_behind_nohup_setsid_and_double_fork_is_contained() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "a66", &Limits::default());

    let root = scope
        .spawn(&sh(concat!(
            "nohup sh -c 'while :; do :; done' >/dev/null 2>&1 &\n",
            "setsid sh -c 'while :; do :; done' >/dev/null 2>&1 &\n",
            "(sh -c 'while :; do :; done' >/dev/null 2>&1 &)\n",
            "(setsid nohup sh -c 'sleep 300 & exec sleep 300' >/dev/null 2>&1 &)\n",
            "exit 0",
        )))
        .expect("spawn");
    let root_pid = root.child.pid();
    let (exit, _, errors) = finish(root);
    assert!(exit.success(), "root failed: {errors}");

    // setsid(1) forks before it calls setsid(2), so give the tree a moment.
    assert!(wait_until(Duration::from_secs(5), || {
        scope.members().unwrap() == Membership::Alive(5)
            && member_pids(&scope)
                .iter()
                .any(|pid| session_of(*pid).is_some_and(|sid| sid != root_pid))
    }));
    let hidden = member_pids(&scope);
    assert!(!hidden.contains(&root_pid));
    assert!(hidden.iter().all(|pid| is_alive(*pid)));
    // At least one of them really left the root's session.
    assert!(
        hidden
            .iter()
            .any(|pid| session_of(*pid).is_some_and(|sid| sid != root_pid)),
        "no descendant changed session"
    );

    // The hidden work is charged to the scope.
    let before = scope.usage().unwrap().cpu_time_ms;
    assert!(wait_until(Duration::from_secs(5), || {
        scope.usage().unwrap().cpu_time_ms >= before + 200
    }));

    scope.kill().unwrap();
    expect_empty(&scope);
    assert!(wait_until(Duration::from_secs(5), || {
        hidden.iter().all(|pid| !is_alive(*pid))
    }));
    destroy(scope).unwrap();
}

#[test]
fn child_is_in_the_scope_and_its_own_session_before_it_runs() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "born", &Limits::default());
    let name = scope.identity().name;

    // The very first thing the program does is report where it is.
    let spawned = scope
        .spawn(&ProcessSpec {
            argv: vec!["/bin/cat".into(), "/proc/self/cgroup".into()],
            cwd: PathBuf::from("/"),
            env: BTreeMap::new(),
        })
        .expect("spawn");
    let (exit, output, _) = finish(spawned);
    assert!(exit.success());
    let line = output.lines().find(|line| line.starts_with("0::")).unwrap();
    assert!(line.ends_with(&format!("/bella-{name}")), "{line}");

    let spawned = scope
        .spawn(&sh(
            "read -r pid comm state ppid pgrp sid rest < /proc/self/stat\necho \"$pid $pgrp $sid\"",
        ))
        .expect("spawn");
    let pid = spawned.child.pid();
    let (_, output, _) = finish(spawned);
    assert_eq!(output.trim(), format!("{pid} {pid} {pid}"));

    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn memory_limit_kills_the_allocator_and_the_oom_kill_is_visible() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    if adapter.enforcement().memory != Enforcement::Hard {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: memory controller is not enabled"
        );
        return;
    }
    const LIMIT: u64 = 64 << 20;
    let limits = Limits {
        memory_bytes: Some(LIMIT),
        ..Limits::default()
    };
    let (mut scope, _janitor) = new_scope(&adapter, "mem", &limits);
    assert_eq!(scope.oom_kills(), Some(0));

    // Doubles a string up to 512 MiB, eight times the limit, then gives up.
    let spawned = scope
        .spawn(&sh(
            "x=$(printf %01024d 0)\ni=0\nwhile [ \"$i\" -lt 19 ]; do x=\"$x$x\"; i=$((i+1)); done\necho survived",
        ))
        .expect("spawn");
    let started = Instant::now();
    let (exit, output, _) = finish(spawned);
    assert_eq!(exit.signal, Some(libc::SIGKILL), "exit {exit:?}");
    assert!(!output.contains("survived"));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the kill took {:?}",
        started.elapsed()
    );

    assert!(scope.oom_kills().unwrap() >= 1);
    assert_eq!(oom_kills(&scope.identity()), scope.oom_kills());
    let usage = scope.usage().unwrap();
    assert!(usage.memory_peak_bytes > LIMIT / 2, "{usage:?}");
    assert!(usage.memory_peak_bytes <= LIMIT, "{usage:?}");

    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn process_limit_refuses_forks_beyond_it() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    if adapter.enforcement().process_count != Enforcement::Hard {
        let _ = writeln!(std::io::stderr(), "SKIPPED: pids controller is not enabled");
        return;
    }
    const LIMIT: u32 = 16;
    let limits = Limits {
        max_processes: Some(LIMIT),
        ..Limits::default()
    };
    let (mut scope, _janitor) = new_scope(&adapter, "pids", &limits);
    let mut root = scope
        .spawn(&sh(
            "i=0\nwhile [ \"$i\" -lt 100 ]; do sleep 300 </dev/null >/dev/null 2>&1 & i=$((i+1)); done\nwait",
        ))
        .expect("spawn");

    let mut highest = 0;
    assert!(
        wait_until(Duration::from_secs(10), || {
            highest = highest.max(scope.usage().unwrap().processes);
            keyed(&scope, "pids.events", "max") > 0
        }),
        "no fork was refused; {highest} tasks seen"
    );
    for _ in 0..20 {
        highest = highest.max(scope.usage().unwrap().processes);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(highest <= LIMIT, "{highest} tasks under a limit of {LIMIT}");
    assert!(highest >= LIMIT / 2, "only {highest} tasks were started");
    match scope.members().unwrap() {
        Membership::Alive(count) => assert!(count <= LIMIT),
        other => panic!("{other:?}"),
    }

    scope.kill().unwrap();
    wait_exit(&mut root, Duration::from_secs(5));
    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn cpu_limit_throttles_busy_loops() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    if adapter.enforcement().cpu != Enforcement::Hard {
        let _ = writeln!(std::io::stderr(), "SKIPPED: cpu controller is not enabled");
        return;
    }
    let limits = Limits {
        cpu_milli: Some(250),
        ..Limits::default()
    };
    let (mut scope, _janitor) = new_scope(&adapter, "cpu", &limits);
    let started = Instant::now();
    let mut loops: Vec<Spawned> = (0..2)
        .map(|_| scope.spawn(&sh("while :; do :; done")).expect("spawn"))
        .collect();
    std::thread::sleep(Duration::from_millis(1500));
    let used = scope.usage().unwrap().cpu_time_ms;
    let elapsed = started.elapsed().as_millis() as u64;

    // Two unthrottled loops would use about 2000 ms per second; a quarter of
    // one CPU allows about 250.
    let allowed = elapsed / 4;
    assert!(
        used <= allowed * 16 / 10 + 100,
        "{used} ms of CPU in {elapsed} ms"
    );
    assert!(used >= allowed * 3 / 10, "{used} ms of CPU in {elapsed} ms");
    assert!(keyed(&scope, "cpu.stat", "nr_throttled") > 0);
    assert!(keyed(&scope, "cpu.stat", "throttled_usec") > 0);

    scope.kill().unwrap();
    for spawned in &mut loops {
        wait_exit(spawned, Duration::from_secs(5));
    }
    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn limits_are_written_at_creation_and_rewritten_on_a_live_scope() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let report = adapter.enforcement();
    let limits = Limits {
        cpu_milli: Some(1500),
        memory_bytes: Some(200 << 20),
        max_processes: Some(77),
        low_priority: true,
    };
    let (mut scope, _janitor) = new_scope(&adapter, "limits", &limits);
    let mut member = scope.spawn(&sh("exec sleep 300")).expect("spawn");

    let check = |scope: &CgroupScope, cpu: &str, weight: &str, memory: [&str; 2], pids: &str| {
        if report.cpu == Enforcement::Hard {
            assert_eq!(control(scope, "cpu.max"), cpu);
            assert_eq!(control(scope, "cpu.weight"), weight);
        }
        if report.memory == Enforcement::Hard {
            assert_eq!(control(scope, "memory.max"), memory[0]);
            assert_eq!(control(scope, "memory.high"), memory[1]);
            if scope.path().join("memory.swap.max").exists() {
                let swap = if memory[0] == "max" { "max" } else { "0" };
                assert_eq!(control(scope, "memory.swap.max"), swap);
            }
        }
        if report.process_count == Enforcement::Hard {
            assert_eq!(control(scope, "pids.max"), pids);
        }
    };
    check(
        &scope,
        "150000 100000",
        "10",
        ["209715200", "199229440"],
        "77",
    );

    scope
        .set_limits(&Limits {
            cpu_milli: Some(500),
            memory_bytes: Some(100 << 20),
            max_processes: Some(9),
            low_priority: false,
        })
        .unwrap();
    check(
        &scope,
        "50000 100000",
        "100",
        ["104857600", "99614720"],
        "9",
    );

    scope.set_limits(&Limits::default()).unwrap();
    check(&scope, "max 100000", "100", ["max", "max"], "max");
    assert!(member.child.try_wait().unwrap().is_none());

    scope.kill().unwrap();
    wait_exit(&mut member, Duration::from_secs(5));
    expect_empty(&scope);
    let identity = scope.identity();
    destroy(scope).unwrap();

    // A handle to a removed scope cannot pretend to apply limits.
    let (mut gone, _gone_janitor) = new_scope(&adapter, "limits-gone", &Limits::default());
    fs::remove_dir(gone.path()).unwrap();
    assert!(gone.set_limits(&limits).is_err());
    assert_eq!(gone.members().unwrap(), Membership::Empty);
    assert!(adapter.adopt(&identity).unwrap().is_none());
}

#[test]
fn environment_cwd_and_stdin_are_exactly_what_was_passed() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "env", &Limits::default());
    // Something the worker has that the job must not inherit.
    // SAFETY: tests in this file hold the serial lock, so no other thread
    // reads or writes the environment while it is changed.
    unsafe { std::env::set_var("BELLA_TEST_WORKER_ONLY", "leak") };
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();

    let env = BTreeMap::from([
        ("ALPHA".to_string(), "1".to_string()),
        ("SPACED".to_string(), "two words".to_string()),
        ("EMPTY".to_string(), String::new()),
    ]);
    let spawned = scope
        .spawn(&ProcessSpec {
            argv: vec!["/usr/bin/env".into()],
            cwd: cwd.clone(),
            env: env.clone(),
        })
        .expect("spawn");
    let (exit, output, _) = finish(spawned);
    assert!(exit.success());
    let mut seen: Vec<&str> = output.lines().collect();
    seen.sort_unstable();
    assert_eq!(seen, ["ALPHA=1", "EMPTY=", "SPACED=two words"]);

    let spawned = scope
        .spawn(&ProcessSpec {
            argv: vec!["/bin/pwd".into()],
            cwd: cwd.clone(),
            env: BTreeMap::new(),
        })
        .expect("spawn");
    let (_, output, _) = finish(spawned);
    assert_eq!(Path::new(output.trim()), cwd);

    // stdin is closed: a reader sees end of input at once.
    let spawned = scope
        .spawn(&sh("cat; echo \"stdin-bytes=$(wc -c </dev/stdin)\""))
        .expect("spawn");
    let (exit, output, _) = finish(spawned);
    assert!(exit.success());
    assert_eq!(output.trim(), "stdin-bytes=0");

    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn stdout_and_stderr_are_captured_separately() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "pipes", &Limits::default());
    let spawned = scope
        .spawn(&sh("echo to-out; echo to-err >&2; printf 'no newline'"))
        .expect("spawn");
    let (exit, output, errors) = finish(spawned);
    assert!(exit.success());
    assert_eq!(output, "to-out\nno newline");
    assert_eq!(errors, "to-err\n");
    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn exit_codes_and_signals_are_reported() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "exit", &Limits::default());

    let (exit, _, _) = finish(scope.spawn(&sh("exit 0")).unwrap());
    assert_eq!((exit.code, exit.signal), (Some(0), None));
    assert!(exit.success());

    let (exit, _, _) = finish(scope.spawn(&sh("exit 7")).unwrap());
    assert_eq!((exit.code, exit.signal), (Some(7), None));
    assert!(!exit.success());

    let (exit, _, _) = finish(scope.spawn(&sh("kill -TERM $$")).unwrap());
    assert_eq!((exit.code, exit.signal), (None, Some(libc::SIGTERM)));
    assert!(!exit.success());

    // try_wait reports nothing while the process runs.
    let mut running = scope.spawn(&sh("exec sleep 300")).unwrap();
    assert!(running.child.try_wait().unwrap().is_none());
    scope.kill().unwrap();
    assert_eq!(
        wait_exit(&mut running, Duration::from_secs(5)).signal,
        Some(libc::SIGKILL)
    );

    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn a_program_that_cannot_start_is_a_spawn_error_and_leaves_nothing_behind() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "nostart", &Limits::default());

    let missing = ProcessSpec {
        argv: vec!["/nonexistent/bella-test-program".into()],
        cwd: PathBuf::from("/"),
        env: BTreeMap::new(),
    };
    match scope.spawn(&missing) {
        Err(ContainError::Spawn { program, source }) => {
            assert_eq!(program, "/nonexistent/bella-test-program");
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        Ok(_) => panic!("spawn succeeded"),
        Err(other) => panic!("{other}"),
    }
    let bad_cwd = ProcessSpec {
        cwd: PathBuf::from("/nonexistent/bella-test-dir"),
        ..sh("exit 0")
    };
    assert!(matches!(
        scope.spawn(&bad_cwd),
        Err(ContainError::Spawn { .. })
    ));
    let empty = ProcessSpec {
        argv: Vec::new(),
        ..sh("exit 0")
    };
    assert!(matches!(
        scope.spawn(&empty),
        Err(ContainError::Spawn { .. })
    ));

    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn terminate_delivers_sigterm_to_every_member() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "term", &Limits::default());
    let spawned = scope
        .spawn(&sh(concat!(
            "trap 'echo parent-term; exit 0' TERM\n",
            "sh -c 'trap \"echo child-term; exit 0\" TERM; echo ready; while :; do sleep 0.05; done' &\n",
            "echo ready\n",
            "while :; do sleep 0.05; done",
        )))
        .expect("spawn");
    let Spawned {
        mut child, stdout, ..
    } = spawned;
    let mut lines = BufReader::new(stdout).lines();
    // Both shells have installed their traps once both have said so.
    assert_eq!(lines.next().unwrap().unwrap(), "ready");
    assert_eq!(lines.next().unwrap().unwrap(), "ready");

    scope.terminate().unwrap();
    let mut rest: Vec<String> = lines.map(|line| line.unwrap()).collect();
    rest.sort_unstable();
    assert_eq!(rest, ["child-term", "parent-term"]);
    assert_eq!(child.wait().unwrap().code, Some(0));

    expect_empty(&scope);
    destroy(scope).unwrap();
}

#[test]
fn stop_scope_terminates_then_kills_what_ignores_it() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };

    // Nothing running: nothing to do.
    let (mut idle, _idle_janitor) = new_scope(&adapter, "stop-idle", &Limits::default());
    assert_eq!(
        stop_scope(&mut idle, Duration::from_secs(5), Duration::from_secs(5)).unwrap(),
        Membership::Empty
    );
    destroy(idle).unwrap();

    // A cooperative tree ends on SIGTERM, well inside the grace period.
    let (mut polite, _polite_janitor) = new_scope(&adapter, "stop-polite", &Limits::default());
    let mut sleeper = polite.spawn(&sh("sleep 300 & exec sleep 300")).unwrap();
    let started = Instant::now();
    assert_eq!(
        stop_scope(&mut polite, Duration::from_secs(20), Duration::from_secs(5)).unwrap(),
        Membership::Empty
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        wait_exit(&mut sleeper, Duration::from_secs(5)).signal,
        Some(libc::SIGTERM)
    );
    destroy(polite).unwrap();

    // A tree that ignores SIGTERM survives the grace period and is killed.
    let (mut stubborn, _stubborn_janitor) =
        new_scope(&adapter, "stop-stubborn", &Limits::default());
    let spawned = stubborn
        .spawn(&sh(concat!(
            "trap '' TERM\n",
            "(trap '' TERM; while :; do sleep 0.05; done) &\n",
            "echo ready\n",
            "while :; do sleep 0.05; done",
        )))
        .unwrap();
    let Spawned {
        mut child, stdout, ..
    } = spawned;
    let mut lines = BufReader::new(stdout).lines();
    assert_eq!(lines.next().unwrap().unwrap(), "ready");
    let grace = Duration::from_millis(400);
    let started = Instant::now();
    assert_eq!(
        stop_scope(&mut stubborn, grace, Duration::from_secs(5)).unwrap(),
        Membership::Empty
    );
    assert!(started.elapsed() >= grace, "killed before the grace period");
    assert_eq!(child.wait().unwrap().signal, Some(libc::SIGKILL));
    assert_eq!(
        destroy(stubborn).unwrap(),
        "cgroup.procs empty; cgroup removed"
    );
}

/// The path used on kernels without `cgroup.kill`.
#[test]
fn kill_by_signal_empties_a_scope_that_keeps_forking() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let limits = Limits {
        max_processes: Some(64),
        ..Limits::default()
    };
    let (mut scope, _janitor) = new_scope(&adapter, "sigkill", &limits);
    let mut roots: Vec<Spawned> = (0..3)
        .map(|_| {
            scope
                .spawn(&sh(
                    "while :; do (sleep 300 </dev/null >/dev/null 2>&1 &); done 2>/dev/null",
                ))
                .expect("spawn")
        })
        .collect();
    assert!(wait_until(Duration::from_secs(5), || {
        matches!(scope.members(), Ok(Membership::Alive(count)) if count >= 10)
    }));

    scope.kill_by_signal().unwrap();
    expect_empty(&scope);
    if scope.path().join("cgroup.freeze").exists() {
        assert_eq!(control(&scope, "cgroup.freeze"), "0");
    }
    for spawned in &mut roots {
        wait_exit(spawned, Duration::from_secs(5));
    }
    destroy(scope).unwrap();
}

#[test]
fn usage_outlives_the_processes_it_measured() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "usage", &Limits::default());
    assert_eq!(scope.members().unwrap(), Membership::Empty);
    assert_eq!(scope.usage().unwrap().processes, 0);

    let spawned = scope
        .spawn(&sh("i=0; while [ \"$i\" -lt 200000 ]; do i=$((i+1)); done"))
        .expect("spawn");
    let (exit, _, _) = finish(spawned);
    assert!(exit.success());

    expect_empty(&scope);
    let usage = scope.usage().unwrap();
    assert!(usage.cpu_time_ms > 0, "{usage:?}");
    assert_eq!(usage.processes, 0);
    assert_eq!(usage.disk_bytes, 0);
    if adapter.enforcement().memory == Enforcement::Hard {
        assert!(usage.memory_peak_bytes > 0, "{usage:?}");
        assert!(usage.memory_peak_bytes >= usage.memory_current_bytes);
    }
    destroy(scope).unwrap();
}

/// A job with write access to its own cgroup can make child cgroups. They
/// are still part of the scope.
#[test]
fn child_cgroups_made_by_a_job_are_counted_killed_and_removed() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    let (mut scope, _janitor) = new_scope(&adapter, "nested", &Limits::default());
    let mut spec = sh(concat!(
        "mkdir \"$SCOPE/inner\" \"$SCOPE/inner/deeper\" || exit 1\n",
        "sleep 300 &\n",
        "echo $! > \"$SCOPE/inner/deeper/cgroup.procs\" || exit 1\n",
        "echo moved\n",
        "wait",
    ));
    spec.env.insert(
        "SCOPE".to_string(),
        scope.path().to_string_lossy().into_owned(),
    );
    let spawned = scope.spawn(&spec).expect("spawn");
    let Spawned {
        mut child, stdout, ..
    } = spawned;
    let mut lines = BufReader::new(stdout).lines();
    assert_eq!(lines.next().unwrap().unwrap(), "moved");

    // One process directly in the scope, one two levels down.
    assert_eq!(member_pids(&scope).len(), 1);
    assert_eq!(scope.members().unwrap(), Membership::Alive(2));
    match destroy(adapter.adopt_scope(&scope.identity()).unwrap().unwrap()) {
        Err(ContainError::NotEmpty(_)) => {}
        other => panic!("destroy returned {other:?}"),
    }

    scope.terminate().unwrap();
    assert_eq!(child.wait().unwrap().signal, Some(libc::SIGTERM));
    expect_empty(&scope);
    let path = scope.path().to_path_buf();
    assert!(path.join("inner/deeper").is_dir());
    destroy(scope).unwrap();
    assert!(!path.exists());
}

#[test]
fn names_and_identities_are_checked() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    for bad in ["", "Upper", "a/b", "..", "with space"] {
        assert!(
            adapter.create(bad, &Limits::default()).is_err(),
            "accepted {bad:?}"
        );
    }

    let (scope, _janitor) = new_scope(&adapter, "ident", &Limits::default());
    let identity = scope.identity();
    assert_eq!(identity.kind, ScopeKind::LinuxCgroup);
    assert_eq!(Path::new(&identity.detail), scope.path());
    assert_eq!(
        scope.path(),
        adapter.base().join(format!("bella-{}", identity.name))
    );
    assert!(scope.path().is_dir());

    // The same name cannot be created twice.
    assert!(adapter.create(&identity.name, &Limits::default()).is_err());
    assert!(scope.path().is_dir());

    // An identity of another adapter, or one that points somewhere else, is
    // refused rather than treated as gone.
    let foreign_kind = ScopeIdentity {
        kind: ScopeKind::UnixProcessGroup,
        ..identity.clone()
    };
    assert!(adapter.adopt(&foreign_kind).is_err());
    let elsewhere = ScopeIdentity {
        detail: adapter
            .base()
            .join("supervisor")
            .to_string_lossy()
            .into_owned(),
        ..identity.clone()
    };
    assert!(adapter.adopt(&elsewhere).is_err());
    let traversal = ScopeIdentity {
        name: "../supervisor".to_string(),
        ..identity.clone()
    };
    assert!(adapter.adopt(&traversal).is_err());

    // The journaled identity, and one journaled without detail, are adopted.
    assert!(adapter.adopt(&identity).unwrap().is_some());
    let bare = ScopeIdentity {
        detail: String::new(),
        ..identity.clone()
    };
    let adopted = adapter.adopt(&bare).unwrap().expect("adopted by name");
    assert_eq!(adopted.identity(), identity);
    adopted.destroy().unwrap();
    assert!(!scope.path().exists());
}

#[test]
fn self_test_exercises_and_verifies_every_enabled_dimension() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    assert_eq!(adapter.kind(), ScopeKind::LinuxCgroup);
    assert_eq!(adapter.mechanism(), "cgroup-v2");

    let detected = adapter.enforcement();
    assert!(!detected.verified);
    assert_eq!(detected.process_tree, Enforcement::Hard);
    assert_eq!(detected.disk, Enforcement::None);

    let before: Vec<_> = scope_dirs(adapter.base());
    let report = adapter.self_test().expect("self_test");
    let _ = writeln!(
        std::io::stderr(),
        "self_test on {}: {report:?}",
        adapter.base().display()
    );
    assert!(report.verified);
    assert_eq!(report.process_tree, Enforcement::Hard, "{:?}", report.notes);
    // Whatever was detected as enforceable must have survived the test.
    assert_eq!(report.cpu, detected.cpu, "{:?}", report.notes);
    assert_eq!(report.memory, detected.memory, "{:?}", report.notes);
    assert_eq!(
        report.process_count, detected.process_count,
        "{:?}",
        report.notes
    );
    assert_eq!(report.disk, Enforcement::None);
    // The self-test removes its own scopes.
    assert_eq!(scope_dirs(adapter.base()), before);

    // The worker itself is in the supervisor leaf, outside every scope.
    let own = fs::read_to_string("/proc/self/cgroup").unwrap();
    assert!(own.trim_end().ends_with("/supervisor"), "{own}");

    let native = bella_contain::native().expect("native");
    assert_eq!(native.kind(), ScopeKind::LinuxCgroup);
}

fn scope_dirs(base: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(base)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("bella-"))
        .collect();
    names.sort();
    names
}

/// A cgroup that stands in for a systemd unit, removed with everything in
/// it when the test ends.
struct FakeUnit(PathBuf);

impl FakeUnit {
    fn create(adapter: &CgroupV2, tag: &str) -> Self {
        // Make sure the base can have children with controllers first.
        adapter.enforcement();
        let path = adapter.base().join(format!("fake-{}", unique(tag)));
        fs::create_dir(&path).expect("create fake unit");
        Self(path)
    }

    /// What systemd does for `Delegate=yes`.
    fn mark_delegated(&self) -> std::io::Result<()> {
        let path = CString::new(self.0.as_os_str().as_bytes()).unwrap();
        // SAFETY: both strings are NUL-terminated and the value pointer is
        // readable for the one byte passed.
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                c"user.delegate".as_ptr(),
                b"1".as_ptr().cast(),
                1,
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// What systemd does for `Delegate=yes` together with `User=`.
    fn give_to(&self, uid: u32) {
        for path in [
            self.0.clone(),
            self.0.join("cgroup.procs"),
            self.0.join("cgroup.threads"),
            self.0.join("cgroup.subtree_control"),
        ] {
            std::os::unix::fs::chown(&path, Some(uid), Some(uid)).expect("chown");
        }
    }

    /// Re-run this test binary's helper with the unit as its cgroup.
    fn run_helper(&self, mode: &str, uid: Option<u32>) -> std::process::Output {
        let exe = std::env::current_exe().unwrap();
        let mut script = String::from("echo $$ > \"$1/cgroup.procs\" || exit 97\nshift\n");
        match uid {
            Some(uid) => script.push_str(&format!(
                "exec setpriv --reuid {uid} --regid {uid} --clear-groups \"$@\"\n"
            )),
            None => script.push_str("exec \"$@\"\n"),
        }
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .arg(&self.0)
            .arg(exe)
            .args([
                "helper_inside_a_fake_unit",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_MODE, mode)
            .env(HELPER_BASE, &self.0)
            .output()
            .expect("run helper")
    }
}

impl Drop for FakeUnit {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("cgroup.kill"), "1");
        wait_until(Duration::from_secs(5), || {
            fs::read_to_string(self.0.join("cgroup.events"))
                .is_ok_and(|text| text.contains("populated 0"))
        });
        remove_cgroups(&self.0);
    }
}

fn remove_cgroups(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                remove_cgroups(&entry.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

fn describe(output: &std::process::Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Body of the re-executed helper; does nothing in an ordinary test run.
///
/// `usable`: the cgroup this process was placed in must be detected as the
/// base, and a scope below it must contain, limit and kill a daemonising
/// tree. `unavailable`: detection must refuse the cgroup as not delegated.
#[test]
fn helper_inside_a_fake_unit() {
    let Ok(mode) = std::env::var(HELPER_MODE) else {
        return;
    };
    let expected_base = PathBuf::from(std::env::var(HELPER_BASE).unwrap());
    match mode.as_str() {
        "unavailable" => match CgroupV2::detect() {
            Err(ContainError::Unavailable(reason)) => println!("HELPER-UNAVAILABLE: {reason}"),
            Ok(adapter) => panic!("detected {}", adapter.base().display()),
            Err(other) => panic!("{other}"),
        },
        "usable" => {
            let adapter = CgroupV2::detect().expect("detect");
            assert_eq!(adapter.base(), expected_base);
            let report = adapter.enforcement();
            assert_eq!(report.cpu, Enforcement::Hard, "{:?}", report.notes);
            assert_eq!(report.memory, Enforcement::Hard, "{:?}", report.notes);
            assert_eq!(
                report.process_count,
                Enforcement::Hard,
                "{:?}",
                report.notes
            );

            // The worker moved itself into the leaf, and a second detection
            // from there finds the same base.
            let own = fs::read_to_string("/proc/self/cgroup").unwrap();
            assert!(own.trim_end().ends_with("/supervisor"), "{own}");
            assert_eq!(
                CgroupV2::detect().expect("detect again").base(),
                expected_base
            );

            let limits = Limits {
                cpu_milli: Some(500),
                memory_bytes: Some(64 << 20),
                max_processes: Some(32),
                low_priority: false,
            };
            let mut scope = adapter.create_scope("helper", &limits).expect("create");
            assert_eq!(control(&scope, "pids.max"), "32");
            assert_eq!(control(&scope, "memory.max"), "67108864");
            assert_eq!(control(&scope, "cpu.max"), "50000 100000");
            let root = scope
                .spawn(&sh("(sleep 300 </dev/null >/dev/null 2>&1 &)\nexit 0"))
                .expect("spawn");
            let (exit, _, _) = finish(root);
            assert!(exit.success());
            assert_eq!(scope.members().unwrap(), Membership::Alive(1));
            scope.terminate().unwrap();
            expect_empty(&scope);
            destroy(scope).expect("destroy");

            let verified = adapter.self_test().expect("self_test");
            assert_eq!(
                verified.process_tree,
                Enforcement::Hard,
                "{:?}",
                verified.notes
            );
            assert_eq!(verified.memory, Enforcement::Hard, "{:?}", verified.notes);
            assert_eq!(
                verified.process_count,
                Enforcement::Hard,
                "{:?}",
                verified.notes
            );
            assert_eq!(verified.cpu, Enforcement::Hard, "{:?}", verified.notes);
            // SAFETY: geteuid has no arguments and cannot fail.
            println!("HELPER-USABLE uid={}", unsafe { libc::geteuid() });
        }
        other => panic!("unknown helper mode {other}"),
    }
}

/// A root worker in a cgroup that nobody delegated (no systemd attribute,
/// not a namespace root) must not take it over.
#[test]
fn an_undelegated_cgroup_is_reported_unavailable() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    if !is_root() {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: needs root to build a fake unit"
        );
        return;
    }
    let unit = FakeUnit::create(&adapter, "plain");
    let output = unit.run_helper("unavailable", None);
    assert!(output.status.success(), "{}", describe(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("HELPER-UNAVAILABLE"),
        "{}",
        describe(&output)
    );
    assert!(stdout.contains("not delegated"), "{}", describe(&output));
    // Nothing was created or moved in the refused cgroup.
    assert!(!unit.0.join("supervisor").exists());
}

/// The layout systemd produces for a root service with `Delegate=yes`.
#[test]
fn a_cgroup_marked_delegated_is_used_as_the_base() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    if !is_root() {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: needs root to build a fake unit"
        );
        return;
    }
    let unit = FakeUnit::create(&adapter, "delegated");
    if let Err(error) = unit.mark_delegated() {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: cannot set user.delegate on a cgroup: {error}"
        );
        return;
    }
    let output = unit.run_helper("usable", None);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("HELPER-USABLE uid=0"),
        "{}",
        describe(&output)
    );
    assert!(unit.0.join("supervisor").is_dir());
    assert!(!unit.0.join("bella-helper").exists());
}

/// The layout systemd produces for an unprivileged unit with `Delegate=yes`:
/// the cgroup and its control files belong to the worker's user.
#[test]
fn an_unprivileged_worker_uses_a_cgroup_it_owns() {
    let _serial = serial();
    let Some(adapter) = adapter() else { return };
    if !is_root() {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: needs root to build a fake unit"
        );
        return;
    }
    if Command::new("setpriv").arg("--version").output().is_err() {
        let _ = writeln!(std::io::stderr(), "SKIPPED: setpriv is not installed");
        return;
    }
    let unit = FakeUnit::create(&adapter, "owned");
    unit.give_to(NOBODY);
    let output = unit.run_helper("usable", Some(NOBODY));
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("HELPER-USABLE uid={NOBODY}")),
        "{}",
        describe(&output)
    );
    assert!(!unit.0.join("bella-helper").exists());
}
