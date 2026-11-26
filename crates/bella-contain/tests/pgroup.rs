//! The process-group adapter against real processes.
//!
//! Every scope and helper process is owned by a guard that kills it when the
//! test ends, whether it passes or panics.

#![cfg(unix)]

use bella_contain::pgroup::{
    LOW_PRIORITY_NICE, ProcessGroups, ProcessRecord, SCOPE_ENV, ScopeDetail,
};
use bella_contain::{
    Child, ContainError, Containment, ExitInfo, Limits, Membership, ProcessSpec, Scope, Spawned,
    stop_scope,
};
use bella_core::protocol::{ScopeIdentity, ScopeKind};
use bella_core::resources::Enforcement;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin";

/// Environment variable that turns this test binary into a helper process.
const ROLE_ENV: &str = "BELLA_TEST_ROLE";

/// Leaves the leader's process group but not its session, then loses its
/// parent. Prints its own PID once it has left the group.
const ESCAPE_GROUP: &str =
    r#"fork and exit; setpgrp(0, 0); print "PID $$\n"; close STDOUT; sleep 120"#;

/// Forks, starts a new session in the child, forks again and leaves the
/// grandchild running as an orphan outside the leader's session. The middle
/// process prints the grandchild's PID. The environment is kept.
const ESCAPE_SESSION: &str = r#"use POSIX; fork and exit; POSIX::setsid(); my $p = fork; if ($p) { print "PID $p\n"; exit } sleep 120"#;

/// The same escape, but the orphan also replaces its environment, and the
/// start is delayed so that nothing observes the scope while it happens.
const ESCAPE_SESSION_NO_TAG: &str = r#"use POSIX; select(undef, undef, undef, 0.4); fork and exit; POSIX::setsid(); my $p = fork; if ($p) { print "PID $p\n"; exit } %ENV = (); exec "/bin/sleep", "120""#;

/// A child that starts its own session and clears its environment while its
/// parent, the leader, stays alive.
const HIDE_UNDER_LIVE_PARENT: &str = r#"use POSIX; $| = 1; my $p = fork; if (!$p) { POSIX::setsid(); %ENV = (); exec "/bin/sleep", "120" } print "PID $p\n"; sleep 120"#;

/// Uses 0.3 s of user CPU time, however long that takes.
const BURN: &str = "my $t = (times)[0]; 1 while (times)[0] - $t < 0.3;";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn unique_name(label: &str) -> String {
    format!(
        "t-{label}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A scope that is killed when the test ends.
struct Held {
    scope: Option<Box<dyn Scope>>,
}

impl Held {
    fn new(label: &str) -> Self {
        Self::with_limits(label, &Limits::default())
    }

    fn with_limits(label: &str, limits: &Limits) -> Self {
        let scope = ProcessGroups::new()
            .create(&unique_name(label), limits)
            .unwrap();
        Self { scope: Some(scope) }
    }

    fn adopted(scope: Box<dyn Scope>) -> Self {
        Self { scope: Some(scope) }
    }

    fn take(&mut self) -> Box<dyn Scope> {
        self.scope.take().unwrap()
    }
}

impl std::ops::Deref for Held {
    type Target = dyn Scope;
    fn deref(&self) -> &Self::Target {
        self.scope.as_deref().unwrap()
    }
}

impl std::ops::DerefMut for Held {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.scope.as_deref_mut().unwrap()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.as_mut() {
            let _ = scope.kill();
            let _ = scope.members();
        }
    }
}

/// A process outside every scope, standing in for an unrelated program.
struct Bystander(std::process::Child);

impl Bystander {
    fn start() -> Self {
        Self(
            std::process::Command::new("/bin/sleep")
                .arg("120")
                .spawn()
                .unwrap(),
        )
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.0.id()).unwrap()
    }

    fn is_running(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Kills a process the test learned about from a script's output, if it is
/// still running in the process group it was first seen in.
struct PidGuard {
    pid: i32,
    pgid: i32,
}

impl PidGuard {
    fn new(pid: i32) -> Self {
        Self {
            pid,
            pgid: pgid_of(pid),
        }
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if self.pgid > 1 && is_running(self.pid) && pgid_of(self.pid) == self.pgid {
            // SAFETY: kill only sends a signal; the PID was printed by this
            // test's own script and is still in the group it started in.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
}

fn pgid_of(pid: i32) -> i32 {
    // SAFETY: getpgid only reads the process group of a PID.
    unsafe { libc::getpgid(pid) }
}

/// Alive and not a zombie.
fn is_running(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Orphans stay zombies when PID 1 of a container does not reap.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_ascii_whitespace().next());
        !matches!(state, None | Some("Z" | "X"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // `kill(pid, 0)` also succeeds for a zombie, so ask `ps` for the
        // state: no output means no such process.
        let output = std::process::Command::new("/bin/ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        !state.trim().is_empty() && !state.trim().starts_with('Z')
    }
}

fn nice_of(pid: u32) -> i32 {
    // SAFETY: getpriority only reads a scheduling value.
    unsafe { libc::getpriority(libc::PRIO_PROCESS as _, pid) }
}

fn spec(argv: &[&str]) -> ProcessSpec {
    ProcessSpec {
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: PathBuf::from("/"),
        env: BTreeMap::from([("PATH".to_owned(), PATH.to_owned())]),
    }
}

fn sh(script: &str) -> ProcessSpec {
    spec(&["/bin/sh", "-c", script])
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_exit(child: &mut dyn Child, limit: Duration) -> ExitInfo {
    let start = Instant::now();
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            return exit;
        }
        assert!(start.elapsed() < limit, "process did not exit in time");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_all(mut reader: impl Read) -> String {
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    text
}

/// Read the `PID <n>` line a helper printed, skipping other output.
fn read_pid(spawned: &mut Spawned) -> i32 {
    let mut reader = BufReader::new(&mut spawned.stdout);
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap();
        if let Some(pid) = line.trim().strip_prefix("PID ") {
            return pid.parse().unwrap();
        }
        assert!(read > 0, "no PID line in helper output: {seen:?}");
        seen.push_str(&line);
    }
}

/// This test binary, started again as a helper. Unlike `/bin/sh` or
/// `/usr/bin/perl` it is not an Apple system binary, so macOS lets its
/// environment, and with it the scope tag, be read.
fn helper(role: &str) -> ProcessSpec {
    let exe = std::env::current_exe().unwrap();
    ProcessSpec {
        argv: vec![
            exe.to_str().unwrap().to_owned(),
            "helper_process".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
            "--test-threads=1".to_owned(),
        ],
        cwd: PathBuf::from("/"),
        env: BTreeMap::from([(ROLE_ENV.to_owned(), role.to_owned())]),
    }
}

/// Not a test of the adapter: the body of the helper processes that other
/// tests start with [`helper`]. Without a role it does nothing.
#[test]
fn helper_process() {
    match std::env::var(ROLE_ENV).as_deref() {
        Ok("sleep") => std::thread::sleep(Duration::from_secs(120)),
        Ok("escape") => escape_into_own_session(),
        _ => {}
    }
}

/// Fork, start a new session, fork again, print the grandchild's PID and
/// exit, leaving the grandchild to run this binary as a sleeping helper
/// with the scope tag still in its environment. Does not return.
fn escape_into_own_session() -> ! {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let exe = CString::new(std::env::current_exe().unwrap().as_os_str().as_bytes()).unwrap();
    let args = [
        "helper_process",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]
    .map(|arg| CString::new(arg).unwrap());
    let mut argv = vec![exe.as_ptr()];
    argv.extend(args.iter().map(|arg| arg.as_ptr()));
    argv.push(std::ptr::null());
    let env = [
        CString::new(format!("{ROLE_ENV}=sleep")).unwrap(),
        CString::new(format!("{SCOPE_ENV}={}", std::env::var(SCOPE_ENV).unwrap())).unwrap(),
    ];
    let envp = [env[0].as_ptr(), env[1].as_ptr(), std::ptr::null()];

    // SAFETY: everything the forked children need was allocated above; after
    // each fork only async-signal-safe functions are called (setsid, fork,
    // write, execve, _exit), on buffers that outlive the calls.
    unsafe {
        let first = libc::fork();
        if first != 0 {
            libc::_exit(if first > 0 { 0 } else { 1 });
        }
        libc::setsid();
        let second = libc::fork();
        if second > 0 {
            // The harness has printed a partial line; start a fresh one.
            let mut line = [0u8; 32];
            let mut len = 0;
            for byte in b"\nPID " {
                line[len] = *byte;
                len += 1;
            }
            let mut digits = [0u8; 12];
            let mut count = 0;
            let mut value = second;
            while value > 0 {
                digits[count] = b'0' + (value % 10) as u8;
                count += 1;
                value /= 10;
            }
            while count > 0 {
                count -= 1;
                line[len] = digits[count];
                len += 1;
            }
            line[len] = b'\n';
            len += 1;
            libc::write(1, line.as_ptr().cast(), len);
            libc::_exit(0);
        }
        if second == 0 {
            libc::execve(exe.as_ptr(), argv.as_ptr(), envp.as_ptr());
        }
        libc::_exit(1)
    }
}

fn alive(scope: &dyn Scope) -> u32 {
    match scope.members().unwrap() {
        Membership::Empty => 0,
        Membership::Alive(count) => count,
        Membership::Unknown => panic!("the process table could not be read"),
    }
}

fn detail_of(identity: &ScopeIdentity) -> ScopeDetail {
    serde_json::from_str(&identity.detail).unwrap()
}

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn adapter_reports_its_kind_and_never_hard_enforcement() {
    let adapter = ProcessGroups::new();
    assert_eq!(adapter.kind(), ScopeKind::UnixProcessGroup);
    assert_eq!(adapter.mechanism(), "process-group");

    let report = adapter.enforcement();
    assert!(!report.verified);
    assert_eq!(report.cpu, Enforcement::Advisory);
    assert_eq!(report.memory, Enforcement::Advisory);
    assert_eq!(report.process_tree, Enforcement::Advisory);
    assert_eq!(report.process_count, Enforcement::None);
    assert_eq!(report.disk, Enforcement::None);
    for dimension in [
        "cpu:",
        "memory:",
        "process_tree:",
        "process_count:",
        "disk:",
    ] {
        assert!(
            report.notes.iter().any(|note| note.starts_with(dimension)),
            "no note explains {dimension}"
        );
    }
}

#[test]
fn a11_every_spawn_in_a_scope_dies_with_one_kill() {
    let mut held = Held::new("a11");
    let mut children: Vec<Spawned> = (0..3)
        .map(|_| held.spawn(&sh("sleep 120 & sleep 120 & wait")).unwrap())
        .collect();
    // Three shells, each with two background sleepers.
    wait_until("nine members", WAIT, || alive(&*held) >= 9);

    let detail = detail_of(&held.identity());
    assert_eq!(detail.leaders.len(), 3);
    for (spawned, leader) in children.iter().zip(&detail.leaders) {
        assert_eq!(i64::from(leader.pid), i64::from(spawned.child.pid()));
        assert_eq!(leader.pgid, leader.pid, "each spawn leads its own group");
        assert_ne!(leader.start_time, 0);
    }
    assert!(detail.observed.len() >= 9);
    let observed: Vec<i32> = detail.observed.iter().map(|rec| rec.pid).collect();

    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);
    for spawned in &mut children {
        let exit = wait_exit(spawned.child.as_mut(), WAIT);
        assert_eq!(exit.signal, Some(libc::SIGKILL));
        assert_eq!(exit.code, None);
    }
    for pid in observed {
        assert!(!is_running(pid), "process {pid} survived the kill");
    }
}

#[test]
fn a16_adopt_drops_a_recorded_process_that_has_died() {
    let adapter = ProcessGroups::new();
    let mut held = Held::new("a16-dead");
    let mut spawned = held.spawn(&spec(&["/bin/sleep", "120"])).unwrap();
    let identity = held.identity();
    assert_eq!(identity.kind, ScopeKind::UnixProcessGroup);
    assert_eq!(detail_of(&identity).leaders.len(), 1);

    // While the process lives, the identity leads back to it.
    let mut adopted = Held::adopted(adapter.adopt(&identity).unwrap().unwrap());
    assert_eq!(alive(&*adopted), 1);
    adopted.kill().unwrap();
    assert_eq!(adopted.members().unwrap(), Membership::Empty);
    assert_eq!(
        wait_exit(spawned.child.as_mut(), WAIT).signal,
        Some(libc::SIGKILL)
    );

    // Once it is dead, the same identity matches nothing.
    assert!(adapter.adopt(&identity).unwrap().is_none());
}

#[test]
fn a16_stale_identity_never_signals_the_process_now_holding_the_pid() {
    let adapter = ProcessGroups::new();
    let mut bystander = Bystander::start();
    let own_pid = i32::try_from(std::process::id()).unwrap();
    // Records as a journal would hold them after the PIDs were reused: the
    // right PIDs, the start times of processes that no longer exist.
    let stale = [
        ProcessRecord {
            pid: bystander.pid(),
            pgid: bystander.pid(),
            start_time: 1,
        },
        ProcessRecord {
            pid: own_pid,
            pgid: pgid_of(own_pid),
            start_time: 1,
        },
    ];

    // Nothing but stale records: there is no scope to adopt.
    let dead = ScopeIdentity {
        kind: ScopeKind::UnixProcessGroup,
        name: unique_name("a16-stale"),
        detail: serde_json::to_string(&ScopeDetail {
            leaders: stale.to_vec(),
            observed: stale.to_vec(),
        })
        .unwrap(),
    };
    assert!(adapter.adopt(&dead).unwrap().is_none());

    // A live scope whose journal also holds the stale records.
    let mut held = Held::new("a16-mixed");
    let mut spawned = held.spawn(&spec(&["/bin/sleep", "120"])).unwrap();
    let mut identity = held.identity();
    let mut detail = detail_of(&identity);
    detail.leaders.extend(stale);
    detail.observed.extend(stale);
    identity.detail = serde_json::to_string(&detail).unwrap();

    let mut adopted = Held::adopted(adapter.adopt(&identity).unwrap().unwrap());
    assert_eq!(alive(&*adopted), 1, "only the real member is adopted");
    let kept = detail_of(&adopted.identity());
    for rec in kept.leaders.iter().chain(&kept.observed) {
        assert_ne!(rec.pid, bystander.pid());
        assert_ne!(rec.pid, own_pid);
    }

    adopted.terminate().unwrap();
    adopted.kill().unwrap();
    assert_eq!(adopted.members().unwrap(), Membership::Empty);
    assert!(
        wait_exit(spawned.child.as_mut(), WAIT).signal.is_some(),
        "the real member was signalled"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(bystander.is_running(), "an unrelated process was signalled");
}

/// A worker that dies between `spawn` and journaling the new identity has
/// only the scope name. Programs whose environment can be read are found
/// again by the tag; on macOS that excludes Apple's own binaries, which is
/// why the identity is journaled again after every spawn.
#[test]
fn adopt_finds_a_scope_journaled_before_its_first_spawn() {
    let adapter = ProcessGroups::new();
    let mut held = Held::new("early");
    let journaled = held.identity();
    assert_eq!(detail_of(&journaled), ScopeDetail::default());
    assert!(adapter.adopt(&journaled).unwrap().is_none());

    let mut spawned = held.spawn(&helper("sleep")).unwrap();
    assert_eq!(alive(&*held), 1);

    // The journal holds no PID at all: the scope tag is the only lead.
    let mut found = None;
    wait_until("the helper to be adopted by its tag", WAIT, || {
        found = adapter.adopt(&journaled).unwrap();
        found.is_some()
    });
    let mut adopted = Held::adopted(found.unwrap());
    assert_eq!(alive(&*adopted), 1);
    adopted.kill().unwrap();
    assert_eq!(adopted.members().unwrap(), Membership::Empty);
    assert_eq!(
        wait_exit(spawned.child.as_mut(), WAIT).signal,
        Some(libc::SIGKILL)
    );
}

/// The worker drops its scope object and child handle, as it would lose
/// them in a restart, then adopts the scope again from the identity.
#[test]
fn a_dropped_scope_can_be_adopted_killed_and_destroyed() {
    let adapter = ProcessGroups::new();
    let mut held = Held::new("dropped");
    let spawned = held.spawn(&sh("sleep 120 & wait")).unwrap();
    let leader = i32::try_from(spawned.child.pid()).unwrap();
    wait_until("both members", WAIT, || alive(&*held) == 2);
    let identity = held.identity();
    drop(spawned);
    drop(held.take());
    assert!(is_running(leader), "dropping a scope must not stop it");

    let mut adopted = Held::adopted(adapter.adopt(&identity).unwrap().unwrap());
    assert_eq!(alive(&*adopted), 2);
    adopted.kill().unwrap();
    assert_eq!(adopted.members().unwrap(), Membership::Empty);
    assert!(!is_running(leader));
    // No handle owns the dead leader any more, so the adopted scope reaps
    // it where the OS shows zombies. macOS does not; there it stays a
    // zombie until this process waits for it or exits.
    #[cfg(target_os = "linux")]
    wait_until("the leader to be reaped", WAIT, || {
        adopted.members().unwrap() == Membership::Empty
            && !Path::new(&format!("/proc/{leader}")).exists()
    });
    #[cfg(not(target_os = "linux"))]
    {
        let mut status = 0;
        // SAFETY: `status` is a valid out-pointer; the PID is this test's
        // own child, whose handle was dropped above.
        unsafe { libc::waitpid(leader, &mut status, libc::WNOHANG) };
    }
    assert!(adopted.take().destroy().is_ok());
}

#[test]
fn adopt_rejects_other_kinds_and_unreadable_detail() {
    let adapter = ProcessGroups::new();
    let mut identity = ScopeIdentity {
        kind: ScopeKind::LinuxCgroup,
        name: unique_name("kind"),
        detail: String::new(),
    };
    assert!(matches!(
        adapter.adopt(&identity),
        Err(ContainError::Other(_))
    ));
    identity.kind = ScopeKind::UnixProcessGroup;
    identity.detail = "not json".into();
    assert!(matches!(
        adapter.adopt(&identity),
        Err(ContainError::Other(_))
    ));
}

#[test]
fn create_rejects_names_outside_the_scope_alphabet() {
    let adapter = ProcessGroups::new();
    for name in ["", "Has Upper", "a=b", "a/b"] {
        assert!(
            adapter.create(name, &Limits::default()).is_err(),
            "{name:?} was accepted"
        );
    }
}

#[test]
fn a24_daemon_that_outlives_its_root_shell_keeps_the_scope_alive() {
    let adapter = ProcessGroups::new();
    let mut held = Held::new("a24");
    let mut spawned = held
        .spawn(&sh("nohup sleep 120 >/dev/null 2>&1 & exit 0"))
        .unwrap();
    assert_eq!(wait_exit(spawned.child.as_mut(), WAIT).code, Some(0));

    assert_eq!(held.members().unwrap(), Membership::Alive(1));
    let identity = held.identity();
    let daemon = detail_of(&identity).observed[0].pid;
    assert!(is_running(daemon));

    // The root is gone, the scope is not empty, so it cannot be destroyed.
    assert!(matches!(
        held.take().destroy(),
        Err(ContainError::NotEmpty(_))
    ));
    assert!(is_running(daemon), "a refused destroy must not kill");

    let mut adopted = Held::adopted(adapter.adopt(&identity).unwrap().unwrap());
    assert_eq!(adopted.members().unwrap(), Membership::Alive(1));
    adopted.kill().unwrap();
    assert_eq!(adopted.members().unwrap(), Membership::Empty);
    assert!(!is_running(daemon));
    let evidence = adopted.take().destroy().unwrap();
    assert!(
        evidence.contains("no process in tracked groups"),
        "{evidence}"
    );
}

/// A24: the daemon has no scope tag and no living parent. Only the process
/// group of the exited, not yet reaped shell leads to it.
#[test]
fn a24_untagged_daemon_is_found_in_the_process_group_of_its_dead_root() {
    let script = "/bin/sleep 120 & exit 0";

    // The worker waits for the shell.
    let mut held = Held::new("a24-group");
    let mut spawned = held
        .spawn(&spec(&["/usr/bin/env", "-i", "/bin/sh", "-c", script]))
        .unwrap();
    assert_eq!(wait_exit(spawned.child.as_mut(), WAIT).code, Some(0));
    assert_eq!(held.members().unwrap(), Membership::Alive(1));
    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);

    // Nobody waits for the shell: the scope reaps it itself.
    let mut held = Held::new("a24-unwaited");
    drop(
        held.spawn(&spec(&["/usr/bin/env", "-i", "/bin/sh", "-c", script]))
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..3 {
        assert_eq!(held.members().unwrap(), Membership::Alive(1));
    }
    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);
    assert!(held.take().destroy().is_ok());
}

/// A66: the orphan left the leader's process group, lost its parent and, on
/// macOS, runs a system binary whose environment cannot be read. It is still
/// in the leader's session, and that is enough.
#[test]
fn a66_orphan_that_left_its_process_group_is_found_in_its_session() {
    let mut held = Held::new("a66-group");
    let mut spawned = held.spawn(&spec(&["perl", "-e", ESCAPE_GROUP])).unwrap();
    let leader = i32::try_from(spawned.child.pid()).unwrap();
    let escapee = read_pid(&mut spawned);
    let _guard = PidGuard::new(escapee);
    assert_eq!(wait_exit(spawned.child.as_mut(), WAIT).code, Some(0));
    assert!(is_running(escapee));
    assert_eq!(pgid_of(escapee), escapee, "it leads its own group");
    assert_ne!(pgid_of(escapee), leader);

    assert_eq!(held.members().unwrap(), Membership::Alive(1));
    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);
    assert!(!is_running(escapee));
}

/// Start a daemon that double-forks into its own session and is orphaned,
/// and check that it really left. Returns the daemon's PID.
fn start_session_escape(held: &mut Held, process: &ProcessSpec) -> (i32, PidGuard) {
    let mut spawned = held.spawn(process).unwrap();
    let leader = i32::try_from(spawned.child.pid()).unwrap();
    let escapee = read_pid(&mut spawned);
    let guard = PidGuard::new(escapee);
    assert_eq!(wait_exit(spawned.child.as_mut(), WAIT).code, Some(0));
    assert!(is_running(escapee));
    let middle = pgid_of(escapee);
    assert_ne!(middle, leader, "it left the leader's process group");
    wait_until("the middle process to exit", WAIT, || !is_running(middle));
    (escapee, guard)
}

/// A66: a daemon whose environment can be read is found by its scope tag
/// after it double-forked into its own session and lost its parent.
#[test]
fn a66_orphan_in_its_own_session_is_found_by_its_tag_and_killed() {
    let mut held = Held::new("a66-tag");
    let (escapee, _guard) = start_session_escape(&mut held, &helper("escape"));

    wait_until("the daemon to be found", WAIT, || alive(&*held) == 1);
    assert!(
        detail_of(&held.identity())
            .observed
            .iter()
            .any(|rec| rec.pid == escapee)
    );
    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);
    assert!(!is_running(escapee));
}

/// A66 with a system binary as the daemon. Linux shows every environment
/// of the same user, so the tag finds it. macOS with System Integrity
/// Protection does not show the environment of Apple's own binaries: there
/// this daemon escapes, the assertions fail, and the test is ignored to keep
/// the limit on record. Run it with `--ignored` to see the escape.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "documented limit: macOS hides the environment of Apple system binaries, so an orphaned one in its own session is not tracked"
)]
fn a66_system_binary_orphan_in_its_own_session_is_found_by_its_tag() {
    let mut held = Held::new("a66-system");
    let (escapee, _guard) = start_session_escape(&mut held, &spec(&["perl", "-e", ESCAPE_SESSION]));

    assert_eq!(held.members().unwrap(), Membership::Alive(1));
    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);
    assert!(!is_running(escapee));
}

#[test]
fn a66_untagged_child_in_its_own_session_is_found_through_its_parent() {
    let mut held = Held::new("a66-parent");
    let mut spawned = held
        .spawn(&spec(&["perl", "-e", HIDE_UNDER_LIVE_PARENT]))
        .unwrap();
    let leader = i32::try_from(spawned.child.pid()).unwrap();
    let hidden = read_pid(&mut spawned);
    let _guard = PidGuard::new(hidden);
    wait_until("the child to start its session", WAIT, || {
        pgid_of(hidden) == hidden
    });
    assert_ne!(pgid_of(hidden), leader);

    wait_until("both members", WAIT, || alive(&*held) == 2);
    held.kill().unwrap();
    assert_eq!(held.members().unwrap(), Membership::Empty);
    assert!(!is_running(hidden));
    assert_eq!(
        wait_exit(spawned.child.as_mut(), WAIT).signal,
        Some(libc::SIGKILL)
    );
}

/// A66, the part no version of this adapter can do. The orphan starts its
/// own session, replaces its environment and loses its parent while nothing
/// observes the scope, so no session, parent link or tag leads to it. The
/// assertions state what containment would require; they fail, which is why
/// `process_tree` is reported as advisory and never hard. Run with
/// `--ignored` to see the escape.
#[test]
#[ignore = "documented limit: an orphan that leaves its session and drops BELLA_SCOPE between two observations is not tracked"]
fn a66_orphan_that_also_drops_its_tag_between_observations_is_found() {
    let mut held = Held::new("a66-gap");
    let (escapee, _guard) =
        start_session_escape(&mut held, &spec(&["perl", "-e", ESCAPE_SESSION_NO_TAG]));
    // Let it replace its environment before the scope looks again.
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(held.members().unwrap(), Membership::Alive(1));
    held.kill().unwrap();
    assert!(!is_running(escapee));
}

#[test]
fn self_test_exercises_the_tree_and_stays_advisory() {
    let report = ProcessGroups::new().self_test().unwrap();
    assert!(report.verified);
    assert_eq!(report.cpu, Enforcement::Advisory, "{:#?}", report.notes);
    assert_eq!(report.memory, Enforcement::Advisory, "{:#?}", report.notes);
    assert_eq!(
        report.process_tree,
        Enforcement::Advisory,
        "{:#?}",
        report.notes
    );
    assert_eq!(report.process_count, Enforcement::None);
    assert_eq!(report.disk, Enforcement::None);

    let has = |needle: &str| report.notes.iter().any(|note| note.contains(needle));
    assert!(!has("FAILED"), "{:#?}", report.notes);
    assert!(has("found in its process group and killed"));
    assert!(has(
        "left its process group but not its session was found and killed"
    ));
    assert!(has("low_priority started a process at nice"));
    assert!(has("CPU time and memory of a busy process were measured"));
    // What escapes is demonstrated on this machine, never left unmentioned:
    // a system binary whose tag cannot be read (macOS), or a daemon that
    // dropped the tag (anywhere the tag can be read).
    if has("found by its BELLA_SCOPE value and killed") {
        assert!(
            has("clears BELLA_SCOPE and is orphaned between two observations is NOT tracked")
                || has("depends on timing"),
            "{:#?}",
            report.notes
        );
    } else {
        assert!(
            has("does not let the environment of its own binaries be read"),
            "{:#?}",
            report.notes
        );
    }
}

#[test]
fn environment_is_exactly_the_spec_plus_the_scope_tag() {
    let mut held = Held::new("env");
    let name = held.identity().name;
    let mut process = spec(&["/usr/bin/env"]);
    process.env = BTreeMap::from([
        ("ALPHA".to_owned(), "1".to_owned()),
        ("BETA".to_owned(), "two words".to_owned()),
        // A spec cannot choose its own tag.
        (SCOPE_ENV.to_owned(), "spoofed".to_owned()),
    ]);
    let mut spawned = held.spawn(&process).unwrap();
    let mut lines: Vec<String> = read_all(&mut spawned.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "ALPHA=1".to_owned(),
            format!("{SCOPE_ENV}={name}"),
            "BETA=two words".to_owned(),
        ]
    );
    assert!(wait_exit(spawned.child.as_mut(), WAIT).success());
}

#[test]
fn stdin_is_empty_and_the_working_directory_is_the_spec_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mut held = Held::new("cwd");
    let mut process = sh("cat; /bin/pwd");
    process.cwd = dir.path().to_path_buf();
    let mut spawned = held.spawn(&process).unwrap();
    let output = read_all(&mut spawned.stdout);
    assert!(wait_exit(spawned.child.as_mut(), WAIT).success());
    assert_eq!(
        Path::new(output.trim()).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[test]
fn output_exit_codes_and_signals_are_reported() {
    let mut held = Held::new("exit");

    let mut coded = held.spawn(&sh("echo out; echo err >&2; exit 7")).unwrap();
    assert_eq!(read_all(&mut coded.stdout), "out\n");
    assert_eq!(read_all(&mut coded.stderr), "err\n");
    let exit = coded.child.wait().unwrap();
    assert_eq!((exit.code, exit.signal), (Some(7), None));
    assert!(!exit.success());
    // The status stays available.
    assert_eq!(coded.child.try_wait().unwrap(), Some(exit));
    assert_eq!(coded.child.wait().unwrap(), exit);

    let mut signalled = held.spawn(&sh("kill -TERM $$; sleep 120")).unwrap();
    let exit = signalled.child.wait().unwrap();
    assert_eq!((exit.code, exit.signal), (None, Some(libc::SIGTERM)));

    let mut running = held.spawn(&spec(&["/bin/sleep", "120"])).unwrap();
    assert_eq!(running.child.try_wait().unwrap(), None);
    assert!(is_running(i32::try_from(running.child.pid()).unwrap()));
    held.terminate().unwrap();
    assert_eq!(running.child.wait().unwrap().signal, Some(libc::SIGTERM));
}

#[test]
fn a_program_that_cannot_start_is_a_spawn_error() {
    let mut held = Held::new("missing");
    match held.spawn(&spec(&["/nonexistent/bella-program"])) {
        Err(ContainError::Spawn { program, .. }) => {
            assert_eq!(program, "/nonexistent/bella-program")
        }
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("a missing program started"),
    }
    assert!(matches!(
        held.spawn(&spec(&[])),
        Err(ContainError::Spawn { .. })
    ));
    assert_eq!(held.members().unwrap(), Membership::Empty);
}

#[test]
fn an_exited_child_is_not_a_member_and_keeps_its_status() {
    let mut held = Held::new("zombie");
    let mut spawned = held.spawn(&sh("exit 3")).unwrap();
    // Nobody waits: the scope itself must stop counting the dead process.
    wait_until("the scope to empty", WAIT, || alive(&*held) == 0);
    assert_eq!(held.members().unwrap(), Membership::Empty);
    assert_eq!(spawned.child.wait().unwrap().code, Some(3));
    let evidence = held.take().destroy().unwrap();
    assert!(
        evidence.contains("no process in tracked groups"),
        "{evidence}"
    );
}

#[test]
fn usage_counts_a_busy_child_and_cpu_time_never_goes_back() {
    let mut held = Held::new("usage");
    assert_eq!(held.usage().unwrap().processes, 0);
    let mut spawned = held.spawn(&sh("while :; do :; done")).unwrap();

    let mut last = held.usage().unwrap();
    let start = Instant::now();
    while last.cpu_time_ms < 100 {
        assert!(start.elapsed() < WAIT, "no CPU time was measured: {last:?}");
        std::thread::sleep(Duration::from_millis(30));
        let now = held.usage().unwrap();
        assert!(now.cpu_time_ms >= last.cpu_time_ms, "{last:?} then {now:?}");
        assert!(now.memory_peak_bytes >= last.memory_peak_bytes);
        assert!(now.memory_peak_bytes >= now.memory_current_bytes);
        last = now;
    }
    assert_eq!(last.processes, 1);
    assert!(last.memory_current_bytes > 0);
    assert_eq!(last.disk_bytes, 0);

    held.kill().unwrap();
    assert_eq!(
        wait_exit(spawned.child.as_mut(), WAIT).signal,
        Some(libc::SIGKILL)
    );
    let after = held.usage().unwrap();
    assert_eq!(after.processes, 0);
    assert_eq!(after.memory_current_bytes, 0);
    assert!(
        after.cpu_time_ms >= last.cpu_time_ms,
        "{last:?} then {after:?}"
    );
    assert!(after.memory_peak_bytes >= last.memory_peak_bytes);
}

/// Two children each use 0.3 s of CPU and exit before the scope is sampled
/// once. Their time is still counted, through the shell that waited for them.
#[test]
fn usage_includes_children_that_were_waited_for_between_samples() {
    let mut held = Held::new("rollup");
    let mut process = sh(r#"perl -e "$BURN"; perl -e "$BURN"; echo done; sleep 120"#);
    process.env.insert("BURN".to_owned(), BURN.to_owned());
    let mut spawned = held.spawn(&process).unwrap();
    let mut line = String::new();
    BufReader::new(&mut spawned.stdout)
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "done\n");

    let usage = held.usage().unwrap();
    assert!(usage.cpu_time_ms >= 550, "{usage:?}");
    assert!(usage.cpu_time_ms < 5_000, "{usage:?}");
}

#[test]
fn stop_scope_terminates_then_kills_what_ignores_the_request() {
    let mut held = Held::new("stop");
    assert_eq!(
        stop_scope(
            &mut *held,
            Duration::from_millis(10),
            Duration::from_millis(10)
        )
        .unwrap(),
        Membership::Empty
    );

    let mut polite = held.spawn(&spec(&["/bin/sleep", "120"])).unwrap();
    let mut stubborn = held
        .spawn(&sh(
            r#"trap "" TERM; echo ready; while :; do sleep 1; done"#,
        ))
        .unwrap();
    let mut line = String::new();
    BufReader::new(&mut stubborn.stdout)
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");

    let result = stop_scope(
        &mut *held,
        Duration::from_millis(500),
        Duration::from_secs(5),
    );
    assert_eq!(result.unwrap(), Membership::Empty);
    assert_eq!(
        wait_exit(polite.child.as_mut(), WAIT).signal,
        Some(libc::SIGTERM)
    );
    assert_eq!(
        wait_exit(stubborn.child.as_mut(), WAIT).signal,
        Some(libc::SIGKILL)
    );
    assert!(held.take().destroy().is_ok());
}

#[test]
fn terminate_reaches_a_stopped_process() {
    let mut held = Held::new("stopped");
    let mut spawned = held.spawn(&sh("kill -STOP $$; sleep 120")).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(spawned.child.try_wait().unwrap(), None);
    held.terminate().unwrap();
    assert_eq!(
        wait_exit(spawned.child.as_mut(), WAIT).signal,
        Some(libc::SIGTERM)
    );
}

#[test]
fn low_priority_lowers_new_and_running_processes() {
    let own_nice = nice_of(0);
    let expected = own_nice.max(LOW_PRIORITY_NICE);

    let low = Limits {
        low_priority: true,
        ..Limits::default()
    };
    let mut held = Held::with_limits("nice", &low);
    let spawned = held.spawn(&sh("sleep 120 & wait")).unwrap();
    assert_eq!(nice_of(spawned.child.pid()), expected);
    // Descendants inherit it.
    wait_until("the background child", WAIT, || alive(&*held) == 2);
    for rec in detail_of(&held.identity()).observed {
        assert_eq!(nice_of(u32::try_from(rec.pid).unwrap()), expected);
    }

    let mut held = Held::new("renice");
    let spawned = held.spawn(&spec(&["/bin/sleep", "120"])).unwrap();
    assert_eq!(nice_of(spawned.child.pid()), own_nice);
    // Limits that are recorded but not enforced are accepted.
    held.set_limits(&Limits {
        cpu_milli: Some(2_000),
        memory_bytes: Some(1 << 30),
        max_processes: Some(64),
        low_priority: false,
    })
    .unwrap();
    assert_eq!(nice_of(spawned.child.pid()), own_nice);
    held.set_limits(&low).unwrap();
    assert_eq!(nice_of(spawned.child.pid()), expected);
}
