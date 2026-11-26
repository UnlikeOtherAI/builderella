//! The Job Object adapter, exercised with real processes.
//!
//! Several tests start this test binary again as a child (`helper_child`) to
//! get a process whose arguments, environment, allocations and reaction to
//! `CTRL_BREAK` are known exactly. Every long-running child ends by itself
//! within a minute, and every scope is killed when its guard is dropped, so
//! a failing test leaves nothing behind.
#![cfg(windows)]

use bella_contain::windows::{
    CTRL_BREAK_EXIT_CODE, JobObjects, JobScope, KILL_EXIT_CODE, command_line, quote_arg,
};
use bella_contain::{
    Child, ContainError, Containment, ExitInfo, Limits, Membership, ProcessSpec, Scope, Spawned,
    native, stop_scope,
};
use bella_core::protocol::{ScopeIdentity, ScopeKind};
use bella_core::resources::Enforcement;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::ops::{Deref, DerefMut};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant};

const HELPER: &str = "BELLA_CONTAIN_TEST_HELPER";
const HELPER_ARGS: [&str; 5] = [
    "--exact",
    "--nocapture",
    "--test-threads=1",
    "--",
    "helper_child",
];
const WAIT: Duration = Duration::from_secs(60);
const DETACHED_PROCESS: u32 = 0x0000_0008;
const REFUSED: i32 = 42;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(tag: &str) -> String {
    format!(
        "test-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn adapter() -> JobObjects {
    JobObjects::new().expect("job objects are available")
}

/// Owns a scope for the length of a test and kills it on the way out.
struct Guard(Option<JobScope>);

impl Guard {
    fn new(tag: &str, limits: &Limits) -> Self {
        Self(Some(adapter().create_job(&unique(tag), limits).unwrap()))
    }

    /// Hands the scope back without killing it.
    fn release(mut self) -> JobScope {
        self.0.take().unwrap()
    }
}

impl Deref for Guard {
    type Target = JobScope;
    fn deref(&self) -> &JobScope {
        self.0.as_ref().unwrap()
    }
}

impl DerefMut for Guard {
    fn deref_mut(&mut self) -> &mut JobScope {
        self.0.as_mut().unwrap()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut scope) = self.0.take() {
            let _ = scope.kill();
            let _ = becomes_empty(&scope);
        }
    }
}

/// The same safety net for a scope held through the trait object.
struct BoxGuard(Option<Box<dyn Scope>>);

impl Drop for BoxGuard {
    fn drop(&mut self) {
        if let Some(mut scope) = self.0.take() {
            let _ = scope.kill();
            let started = Instant::now();
            while scope.members().ok() != Some(Membership::Empty) && started.elapsed() < WAIT {
                sleep(Duration::from_millis(20));
            }
            let _ = scope.destroy();
        }
    }
}

/// Kills a process started outside any scope when the test ends.
struct Bystander(std::process::Child);

impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn becomes_empty(scope: &JobScope) -> bool {
    let started = Instant::now();
    loop {
        if scope.members().unwrap() == Membership::Empty {
            return true;
        }
        if started.elapsed() >= WAIT {
            return false;
        }
        sleep(Duration::from_millis(20));
    }
}

fn exit_within(child: &mut dyn Child, limit: Duration) -> Option<ExitInfo> {
    let started = Instant::now();
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            return Some(exit);
        }
        if started.elapsed() >= limit {
            return None;
        }
        sleep(Duration::from_millis(20));
    }
}

fn wait_exit(child: &mut dyn Child) -> ExitInfo {
    exit_within(child, WAIT).expect("the process exits in time")
}

fn alive(scope: &JobScope) -> u32 {
    match scope.members().unwrap() {
        Membership::Alive(count) => count,
        other => panic!("expected live members, found {other:?}"),
    }
}

fn work_dir() -> PathBuf {
    std::env::temp_dir()
}

/// The environment of the test process, for commands that need a normal one.
fn system_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// `cmd.exe /d /c <command>` with a normal environment.
fn shell(command: &str) -> ProcessSpec {
    ProcessSpec {
        argv: ["cmd.exe", "/d", "/c", command].map(String::from).to_vec(),
        cwd: work_dir(),
        env: system_env(),
    }
}

/// This test binary in helper mode, with nothing else in its environment.
fn helper(mode: &str, payload: &[&str]) -> ProcessSpec {
    let exe = std::env::current_exe().unwrap();
    let mut argv = vec![exe.to_str().unwrap().to_owned()];
    argv.extend(HELPER_ARGS.map(String::from));
    argv.extend(payload.iter().map(|arg| (*arg).to_owned()));
    ProcessSpec {
        argv,
        cwd: work_dir(),
        env: BTreeMap::from([(HELPER.to_owned(), mode.to_owned())]),
    }
}

fn read_all(mut reader: Box<dyn Read + Send>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

struct Output {
    exit: ExitInfo,
    stdout: String,
    stderr: String,
}

/// Runs a process that leaves no descendants holding its pipes.
fn run(scope: &mut JobScope, spec: &ProcessSpec) -> Output {
    let Spawned {
        mut child,
        stdout,
        stderr,
    } = scope.spawn(spec).unwrap();
    let (stdout, stderr) = (read_all(stdout), read_all(stderr));
    let exit = wait_exit(child.as_mut());
    Output {
        exit,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

/// Starts a helper that prints `READY` once it runs, and waits for that.
fn spawn_ready(scope: &mut JobScope, mode: &str, payload: &[&str]) -> Box<dyn Child> {
    let Spawned { child, stdout, .. } = scope.spawn(&helper(mode, payload)).unwrap();
    let mut lines = BufReader::new(stdout).lines();
    assert!(
        lines.any(|line| line.unwrap().trim() == "READY"),
        "the helper never became ready"
    );
    child
}

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> String {
    let bytes: Vec<u8> = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect();
    String::from_utf8(bytes).unwrap()
}

fn images(scope: &JobScope, file_name: &str) -> Vec<u32> {
    scope
        .processes()
        .unwrap()
        .into_iter()
        .filter(|process| {
            process
                .image
                .as_deref()
                .and_then(|image| image.file_name())
                .is_some_and(|name| name.eq_ignore_ascii_case(file_name))
        })
        .map(|process| process.pid)
        .collect()
}

/// Not a test of its own: the body other tests run in a child process.
#[test]
fn helper_child() {
    let Ok(mode) = std::env::var(HELPER) else {
        return;
    };
    let payload: Vec<String> = std::env::args()
        .skip_while(|arg| arg != "helper_child")
        .skip(1)
        .collect();
    // With one test thread the harness has already printed `test <name> ... `
    // on this line; start a fresh one so every marker stands alone.
    println!();
    match mode.as_str() {
        "argv" => {
            for arg in &payload {
                println!("ARG:{}", hex(arg));
            }
        }
        "env" => {
            for (name, value) in std::env::vars() {
                println!("ENV:{}:{}", hex(&name), hex(&value));
            }
        }
        "sleep" => {
            println!("READY");
            sleep(Duration::from_secs(payload[0].parse().unwrap()));
        }
        "spin" => {
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(payload[0].parse().unwrap()) {
                std::hint::spin_loop();
            }
            println!("READY");
            sleep(Duration::from_secs(60));
        }
        "alloc" => {
            let total_mib: usize = payload[0].parse().unwrap();
            let mut blocks: Vec<Vec<u8>> = Vec::new();
            for done in (0..total_mib).step_by(16) {
                let mut block = Vec::new();
                if block.try_reserve_exact(16 << 20).is_err() {
                    // Give some memory back so reporting cannot fail too.
                    blocks.pop();
                    println!("REFUSED {done}");
                    std::process::exit(REFUSED);
                }
                block.push(1u8);
                blocks.push(block);
            }
            std::hint::black_box(&blocks);
            println!("ALLOCATED {total_mib}");
        }
        "worker" => detached_worker(),
        other => panic!("unknown helper mode {other}"),
    }
}

/// Plays a worker that has no console, like a service, and reports whether
/// `CTRL_BREAK` reached a console child.
fn detached_worker() {
    let mut scope = Guard::new("detached", &Limits::default());
    let mut child = spawn_ready(&mut scope, "sleep", &["60"]);
    let report = scope.send_ctrl_break().unwrap();
    println!("CONSOLES {}", report.consoles);
    println!("SKIPPED {:?}", report.skipped);
    println!("LOST {}", report.console_lost);
    let exit = exit_within(child.as_mut(), Duration::from_secs(20));
    println!("EXIT {:?}", exit.and_then(|exit| exit.code));
    // Reaching this line shows the standard handles survived the attachment.
    println!("DONE");
}

#[test]
fn a11_one_scope_owns_every_spawn_and_kill_ends_them_all() {
    let mut scope = Guard::new("a11", &Limits::default());
    let mut children: Vec<Box<dyn Child>> = (0..3)
        .map(|_| spawn_ready(&mut scope, "sleep", &["60"]))
        .collect();

    let listed = scope.processes().unwrap();
    for child in &children {
        let member = listed.iter().find(|process| process.pid == child.pid());
        let member = member.expect("every spawned process is a member");
        assert_eq!(
            member.image.as_deref(),
            Some(std::env::current_exe().unwrap().as_path())
        );
    }
    assert!(alive(&scope) >= 3);
    assert!(scope.usage().unwrap().processes >= 3);

    scope.kill().unwrap();
    for child in &mut children {
        let exit = wait_exit(child.as_mut());
        assert_eq!(exit.code, Some(KILL_EXIT_CODE as i32));
        assert_eq!(exit.signal, None);
    }
    assert!(becomes_empty(&scope));
    let evidence = Box::new(scope.release()).destroy().unwrap();
    assert!(evidence.contains("ActiveProcesses=0"), "{evidence}");
    assert!(evidence.contains("name released"), "{evidence}");
}

/// A scope dropped while empty takes its keeper with it. A handle that is
/// still open starts a new one with its next spawn, so the scope stays
/// adoptable after every handle is gone.
#[test]
fn a16_keeper_is_restored_before_the_next_spawn() {
    let adapter = adapter();
    let mut first = Guard::new("a16-keeper", &Limits::default());
    let identity = first.identity();
    drop(adapter.adopt_job(&identity).unwrap().expect("a live job"));

    let mut sleeper = spawn_ready(&mut first, "sleep", &["60"]);
    drop(first.release());
    let adopted = adapter.adopt_job(&identity).unwrap();
    let mut adopted = Guard(Some(adopted.expect("the keeper holds the name")));
    adopted.kill().unwrap();
    assert!(wait_exit(sleeper.as_mut()).code.is_some());
    assert!(becomes_empty(&adopted));
    Box::new(adopted.release()).destroy().unwrap();
    assert!(adapter.adopt(&identity).unwrap().is_none());
}

/// A scope that is dropped without ever being destroyed leaves nothing
/// behind once it is empty.
#[test]
fn dropping_an_empty_scope_removes_the_job() {
    let adapter = adapter();
    let mut scope = Guard::new("drop-empty", &Limits::default());
    let identity = scope.identity();
    assert!(run(&mut scope, &shell("exit 0")).exit.success());
    assert!(becomes_empty(&scope));
    drop(scope.release());
    wait_until("the job name to go", || {
        adapter.adopt_job(&identity).unwrap().is_none()
    });
}

/// A16: a stale identity never leads to a process. The job is found by name
/// only, so once it is gone nothing can be signalled, whatever now runs.
#[test]
fn a16_adopting_a_vanished_job_returns_none_and_harms_nobody() {
    let adapter = adapter();
    let mut scope = Guard::new("a16-gone", &Limits::default());
    let identity = scope.identity();
    assert_eq!(identity.kind, ScopeKind::WindowsJob);
    assert_eq!(identity.detail, scope.object_name());
    assert_eq!(identity.detail, format!(r"Local\bella-{}", identity.name));

    assert!(run(&mut scope, &shell("exit 0")).exit.success());
    assert!(becomes_empty(&scope));
    Box::new(scope.release()).destroy().unwrap();

    // Something unrelated is running when the old identity is used again.
    let mut bystander = Bystander(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(HELPER_ARGS)
            .arg("60")
            .env(HELPER, "sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(adapter.adopt(&identity).unwrap().is_none());
    assert!(adapter.adopt_job(&identity).unwrap().is_none());
    sleep(Duration::from_millis(200));
    assert!(
        bystander.0.try_wait().unwrap().is_none(),
        "adopting a vanished scope must not stop any process"
    );

    let wrong_kind = ScopeIdentity {
        kind: ScopeKind::UnixProcessGroup,
        ..identity.clone()
    };
    assert!(adapter.adopt(&wrong_kind).is_err());
    let wrong_object = ScopeIdentity {
        detail: r"Local\something-else".into(),
        ..identity
    };
    assert!(adapter.adopt(&wrong_object).is_err());
}

/// A16: a live job is re-adopted by name, also after the handle that created
/// it is gone, which is what a worker restart looks like.
#[test]
fn a16_live_job_is_adopted_by_name_after_its_first_handle_closed() {
    let adapter = adapter();
    let mut first = Guard::new("a16-live", &Limits::default());
    let mut sleeper = spawn_ready(&mut first, "sleep", &["60"]);
    let identity = first.identity();

    let second = adapter.adopt_job(&identity).unwrap().expect("a live job");
    assert_eq!(second.identity(), identity);
    assert!(
        second
            .processes()
            .unwrap()
            .iter()
            .any(|process| process.pid == sleeper.pid())
    );
    println!(
        "limit events through a second handle: {:?}",
        second.limit_events()
    );
    drop(second);

    // Closing every handle stops nothing: the processes keep the job alive.
    drop(first.release());
    sleep(Duration::from_millis(300));
    assert!(sleeper.try_wait().unwrap().is_none());

    let adopted = adapter.adopt_job(&identity).unwrap();
    let mut adopted = Guard(Some(adopted.expect("the job outlives its handles")));
    assert!(alive(&adopted) >= 1);
    println!(
        "limit events after re-adoption: {:?}",
        adopted.limit_events()
    );
    adopted.kill().unwrap();
    assert_eq!(
        wait_exit(sleeper.as_mut()).code,
        Some(KILL_EXIT_CODE as i32)
    );
    assert!(becomes_empty(&adopted));
    Box::new(adopted.release()).destroy().unwrap();
    assert!(adapter.adopt(&identity).unwrap().is_none());
}

#[test]
fn a24_a_daemon_that_outlives_its_root_keeps_the_scope_alive() {
    let adapter = adapter();
    let mut scope = Guard::new("a24", &Limits::default());
    let spec = shell("start /b ping -n 60 127.0.0.1 >NUL 2>&1 & exit 5");
    let Spawned { mut child, .. } = scope.spawn(&spec).unwrap();
    assert_eq!(wait_exit(child.as_mut()).code, Some(5));

    wait_until("the daemon to show up", || {
        !images(&scope, "ping.exe").is_empty()
    });
    assert!(alive(&scope) >= 1);
    assert!(
        !scope
            .processes()
            .unwrap()
            .iter()
            .any(|process| process.pid == child.pid()),
        "the root is gone"
    );

    // destroy() refuses while the daemon lives, and stops nothing.
    let second = adapter.adopt(&scope.identity()).unwrap().unwrap();
    assert!(matches!(second.destroy(), Err(ContainError::NotEmpty(_))));
    assert!(!images(&scope, "ping.exe").is_empty());

    scope.kill().unwrap();
    assert!(becomes_empty(&scope));
    let identity = scope.identity();
    Box::new(scope.release()).destroy().unwrap();
    assert!(adapter.adopt(&identity).unwrap().is_none());
}

#[test]
fn a66_hidden_descendants_stay_in_the_job_and_die_with_it() {
    let mut scope = Guard::new("a66", &Limits::default());
    let script = "start /b ping -n 60 127.0.0.1 >NUL 2>&1 \
                  & powershell -NoProfile -NonInteractive -Command \
                  \"Start-Process ping -ArgumentList '-n','60','127.0.0.1' -WindowStyle Hidden\" \
                  & exit 0";
    let Spawned { mut child, .. } = scope.spawn(&shell(script)).unwrap();
    assert_eq!(wait_exit(child.as_mut()).code, Some(0));

    wait_until("both hidden processes to be members", || {
        images(&scope, "ping.exe").len() >= 2
    });
    let hidden = images(&scope, "ping.exe");

    scope.kill().unwrap();
    assert!(becomes_empty(&scope));
    // The job is empty, and the processes are gone for everybody else too.
    for pid in hidden {
        let listing = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout).to_ascii_lowercase();
        assert!(
            !(listing.contains("ping.exe") && listing.contains(&format!("\"{pid}\""))),
            "ping {pid} survived: {listing}"
        );
    }
    Box::new(scope.release()).destroy().unwrap();
}

#[test]
fn memory_limit_stops_an_allocator_and_is_reported() {
    const LIMIT: u64 = 256 << 20;
    let limits = Limits {
        memory_bytes: Some(LIMIT),
        ..Limits::default()
    };
    let mut scope = Guard::new("memory", &limits);
    let output = run(&mut scope, &helper("alloc", &["1024"]));
    assert_eq!(output.exit.code, Some(REFUSED), "{}", output.stdout);
    assert!(output.stdout.contains("REFUSED"), "{}", output.stdout);

    let events = scope.limit_events().unwrap();
    assert!(events.memory_limit_hits >= 1, "{events:?}");
    let usage = scope.usage().unwrap();
    // The job's peak also counts the block that was refused.
    assert!(usage.memory_peak_bytes <= LIMIT + (32 << 20), "{usage:?}");
    assert!(usage.memory_peak_bytes >= LIMIT / 2, "{usage:?}");

    // The same allocation succeeds where no limit applies.
    let mut free = Guard::new("memory-free", &Limits::default());
    let output = run(&mut free, &helper("alloc", &["512"]));
    assert_eq!(output.exit.code, Some(0), "{}", output.stdout);
    assert_eq!(free.limit_events().unwrap().memory_limit_hits, 0);
}

/// A restarted worker sees limit hits on the scope it adopted.
#[test]
fn limit_events_follow_the_adopting_handle() {
    let adapter = adapter();
    let limits = Limits {
        memory_bytes: Some(256 << 20),
        ..Limits::default()
    };
    let mut first = Guard::new("events-adopt", &limits);
    let _sleeper = spawn_ready(&mut first, "sleep", &["60"]);
    let identity = first.identity();
    drop(first.release());

    let adopted = adapter.adopt_job(&identity).unwrap();
    let mut adopted = Guard(Some(adopted.expect("a live job")));
    let output = run(&mut adopted, &helper("alloc", &["1024"]));
    assert_eq!(output.exit.code, Some(REFUSED), "{}", output.stdout);
    let events = adopted.limit_events().unwrap();
    assert!(events.memory_limit_hits >= 1, "{events:?}");
}

#[test]
fn process_limit_blocks_extra_roots() {
    const LIMIT: u32 = 4;
    let limits = Limits {
        max_processes: Some(LIMIT),
        ..Limits::default()
    };
    let mut scope = Guard::new("count", &limits);
    let mut started = Vec::new();
    let mut refused = 0;
    for _ in 0..8 {
        match scope.spawn(&helper("sleep", &["60"])) {
            Ok(spawned) => started.push(spawned),
            Err(error) => {
                refused += 1;
                println!("spawn at the limit: {error}");
            }
        }
    }
    // A root that got in but could not get its console host fails on its own.
    sleep(Duration::from_secs(1));
    let mut running = 0;
    for spawned in &mut started {
        if spawned.child.try_wait().unwrap().is_none() {
            running += 1;
        }
    }
    let members = alive(&scope);
    let listed = scope.processes().unwrap();
    println!("limit {LIMIT}: {running} roots running, {refused} refused, {members} members");
    for process in &listed {
        println!("  member {} {:?}", process.pid, process.image);
    }
    assert!((1..=LIMIT as usize).contains(&running));
    assert!(refused >= 1, "spawn must fail once the job is full");
    assert!(scope.limit_events().unwrap().process_limit_hits >= 1);
}

#[test]
fn process_limit_applies_to_descendants() {
    let chain = "cmd /c cmd /c cmd /c cmd /c cmd /c cmd /c exit 0";
    let mut free = Guard::new("chain-free", &Limits::default());
    assert_eq!(run(&mut free, &shell(chain)).exit.code, Some(0));

    let limits = Limits {
        max_processes: Some(3),
        ..Limits::default()
    };
    let mut scope = Guard::new("chain", &limits);
    let output = run(&mut scope, &shell(chain));
    assert_ne!(output.exit.code, Some(0), "{}", output.stderr);
    assert!(scope.limit_events().unwrap().process_limit_hits >= 1);
}

/// Nothing is inherited from the worker and Windows adds nothing either:
/// not even `SystemRoot` exists unless it is passed.
#[test]
fn environment_is_exactly_what_was_passed() {
    let mut scope = Guard::new("env", &Limits::default());
    let mut spec = helper("env", &[]);
    spec.env.insert("ALPHA".into(), "1".into());
    spec.env.insert("beta".into(), "two words".into());
    spec.env.insert("EMPTY".into(), String::new());
    spec.env.insert("Ünï".into(), "çødé = ✓".into());
    let output = run(&mut scope, &spec);
    assert!(output.exit.success(), "{}", output.stderr);

    let seen: BTreeMap<String, String> = output
        .stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ENV:"))
        .map(|entry| {
            let (name, value) = entry.split_once(':').unwrap();
            (unhex(name), unhex(value))
        })
        .collect();
    assert_eq!(seen, spec.env);
}

#[test]
fn stdout_stderr_and_exit_code_are_captured() {
    let mut scope = Guard::new("stdio", &Limits::default());
    let output = run(&mut scope, &shell("echo to-out& echo to-err 1>&2& exit 7"));
    assert_eq!(output.exit.code, Some(7));
    assert!(!output.exit.success());
    assert_eq!(output.stdout.trim(), "to-out");
    assert_eq!(output.stderr.trim(), "to-err");

    // Standard input is NUL: reading it ends at once instead of blocking.
    let output = run(&mut scope, &shell("more & exit 0"));
    assert_eq!(output.exit.code, Some(0));
}

#[test]
fn argv_round_trips_through_a_real_process() {
    let cases = [
        "plain",
        "",
        "two words",
        "tab\there",
        r#"quo"te"#,
        r#""fully quoted""#,
        r"trailing\",
        r"trailing after space \",
        r"C:\dir with space\",
        r#"\"mixed\\"#,
        r#"a\\"b"#,
        r"\\server\share\",
        "%PATH%",
        "a&b|c<d>e^f",
        "ünïcødé ✓",
        "line1\nline2",
        "--flag=value",
        "*.rs",
    ];
    let mut scope = Guard::new("argv", &Limits::default());
    let output = run(&mut scope, &helper("argv", &cases));
    assert!(output.exit.success(), "{}", output.stderr);
    let seen: Vec<String> = output
        .stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ARG:"))
        .map(unhex)
        .collect();
    assert_eq!(seen, cases);
}

#[test]
fn command_line_quoting_is_public_and_follows_the_msvc_rules() {
    assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
    assert_eq!(quote_arg(""), r#""""#);
    let argv = ["tool.exe", "a b", "", r"c\"].map(String::from);
    assert_eq!(command_line(&argv).unwrap(), r#"tool.exe "a b" "" c\"#);
    let shell = ["cmd.exe", "/c", r#"echo "x y""#].map(String::from);
    assert_eq!(
        command_line(&shell).unwrap(),
        r#"cmd.exe /s /c "echo "x y"""#
    );
}

#[test]
fn spawn_refuses_what_it_cannot_run_as_asked() {
    let mut scope = Guard::new("refuse", &Limits::default());
    let spec = |argv: &[&str]| ProcessSpec {
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: work_dir(),
        env: BTreeMap::new(),
    };

    assert!(scope.spawn(&spec(&[])).is_err());
    assert!(matches!(
        scope.spawn(&spec(&["no-such-program-bella-test"])),
        Err(ContainError::Spawn { .. })
    ));
    let mut relative = spec(&["cmd.exe", "/c", "exit 0"]);
    relative.cwd = PathBuf::from("relative-dir");
    assert!(scope.spawn(&relative).is_err());

    // Batch files need a shell, which the caller has to name.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("build.cmd");
    std::fs::write(&script, "@exit 0\r\n").unwrap();
    let batch_error = |result: Result<Spawned, ContainError>| match result {
        Err(ContainError::Other(message)) => assert!(message.contains("batch file"), "{message}"),
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("a batch file was started directly"),
    };
    batch_error(scope.spawn(&spec(&[script.to_str().unwrap()])));
    let mut by_name = spec(&["build"]);
    by_name
        .env
        .insert("PATH".into(), dir.path().to_str().unwrap().into());
    batch_error(scope.spawn(&by_name));
    assert_eq!(scope.members().unwrap(), Membership::Empty);

    // The same script through an explicit shell runs.
    let through_shell = format!("\"{}\"", script.display());
    let mut explicit = spec(&["cmd.exe", "/d", "/c", &through_shell]);
    explicit.env = system_env();
    assert_eq!(run(&mut scope, &explicit).exit.code, Some(0));
}

/// Programs are found without the worker's `PATH` or current directory.
#[test]
fn program_lookup_uses_the_process_environment_and_working_directory() {
    let mut scope = Guard::new("lookup", &Limits::default());
    let system = PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32");
    let spec = |program: &str| ProcessSpec {
        argv: [program, "/d", "/c", "exit 9"].map(String::from).to_vec(),
        cwd: system.clone(),
        env: BTreeMap::new(),
    };
    // A bare name with an empty environment falls back to the system directory.
    assert_eq!(run(&mut scope, &spec("cmd")).exit.code, Some(9));
    // A relative path is relative to the working directory of the new process.
    assert_eq!(run(&mut scope, &spec(r".\cmd.exe")).exit.code, Some(9));
    assert_eq!(run(&mut scope, &spec(r".\cmd")).exit.code, Some(9));
}

#[test]
fn scope_names_are_unique_and_checked() {
    let adapter = adapter();
    let name = unique("dup");
    let mut first = Guard(Some(adapter.create_job(&name, &Limits::default()).unwrap()));
    assert!(adapter.create_job(&name, &Limits::default()).is_err());
    assert!(adapter.create(&name, &Limits::default()).is_err());
    // The refusal did not disturb the scope that owns the name.
    assert_eq!(run(&mut first, &shell("exit 3")).exit.code, Some(3));
    assert!(adapter.create_job("Not Valid", &Limits::default()).is_err());
}

#[test]
fn limits_change_on_a_live_scope() {
    let mut scope = Guard::new("live", &Limits::default());
    let allocate = helper("alloc", &["384"]);
    assert_eq!(run(&mut scope, &allocate).exit.code, Some(0));

    scope
        .set_limits(&Limits {
            cpu_milli: Some(1500),
            memory_bytes: Some(128 << 20),
            max_processes: Some(32),
            low_priority: true,
        })
        .unwrap();
    assert_eq!(run(&mut scope, &allocate).exit.code, Some(REFUSED));

    scope.set_limits(&Limits::default()).unwrap();
    assert_eq!(run(&mut scope, &allocate).exit.code, Some(0));

    assert!(
        scope
            .set_limits(&Limits {
                memory_bytes: Some(0),
                ..Limits::default()
            })
            .is_err()
    );
}

#[test]
fn usage_reports_cpu_memory_and_processes() {
    let mut scope = Guard::new("usage", &Limits::default());
    assert_eq!(scope.usage().unwrap().processes, 0);
    let _child = spawn_ready(&mut scope, "spin", &["400"]);
    let usage = scope.usage().unwrap();
    println!("usage: {usage:?}");
    assert!(usage.cpu_time_ms >= 200, "{usage:?}");
    assert!(usage.processes >= 1, "{usage:?}");
    assert!(usage.memory_current_bytes > 0, "{usage:?}");
    assert!(usage.memory_peak_bytes > 0, "{usage:?}");
    assert_eq!(usage.disk_bytes, 0);
}

#[test]
fn stop_scope_terminates_then_kills_until_empty() {
    let mut scope = Guard::new("stop", &Limits::default());
    let mut polite = spawn_ready(&mut scope, "sleep", &["60"]);
    // ping answers CTRL_BREAK with statistics and carries on, so it needs the
    // kill. Its pipes stay open: it would stop if it could not write.
    let ping = ProcessSpec {
        argv: ["ping", "-n", "60", "127.0.0.1"].map(String::from).to_vec(),
        cwd: work_dir(),
        env: system_env(),
    };
    let Spawned {
        child: mut stubborn,
        stdout: _ping_stdout,
        stderr: _ping_stderr,
    } = scope.spawn(&ping).unwrap();
    wait_until("ping to start", || !images(&scope, "ping.exe").is_empty());

    let membership =
        stop_scope(&mut *scope, Duration::from_secs(2), Duration::from_secs(30)).unwrap();
    assert_eq!(membership, Membership::Empty);
    // A member stops counting when its exit begins, a moment before its
    // process handle is signalled.
    assert!(wait_exit(polite.as_mut()).code.is_some());
    assert_eq!(
        wait_exit(stubborn.as_mut()).code,
        Some(KILL_EXIT_CODE as i32)
    );
    // An empty scope is left alone.
    assert_eq!(
        stop_scope(&mut *scope, Duration::ZERO, Duration::ZERO).unwrap(),
        Membership::Empty
    );
    Box::new(scope.release()).destroy().unwrap();
}

/// In whatever console situation the tests run, `send_ctrl_break` either
/// delivers the event or says why it did not, and the process survives it.
#[test]
fn terminate_in_this_process_delivers_or_explains() {
    let mut scope = Guard::new("ctrl-here", &Limits::default());
    let mut child = spawn_ready(&mut scope, "sleep", &["60"]);
    let report = scope.send_ctrl_break().unwrap();
    println!("send_ctrl_break from the test process: {report:?}");
    assert!(!report.console_lost, "{report:?}");
    if report.consoles > 0 {
        assert_eq!(report.skipped, None);
        assert_eq!(wait_exit(child.as_mut()).code, Some(CTRL_BREAK_EXIT_CODE));
    } else {
        assert!(report.skipped.is_some(), "{report:?}");
        sleep(Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_none());
    }
}

/// A worker without a console, like a service, reaches console children.
#[test]
fn terminate_delivers_ctrl_break_from_a_worker_without_a_console() {
    let worker = std::process::Command::new(std::env::current_exe().unwrap())
        .args(HELPER_ARGS)
        .env_clear()
        .env(HELPER, "worker")
        .env("SystemRoot", std::env::var("SystemRoot").unwrap())
        .env("TEMP", work_dir())
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = worker.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("worker without a console:\n{stdout}");
    let has = |line: &str| stdout.lines().any(|seen| seen.trim() == line);
    assert!(has("CONSOLES 1"), "{stdout}\n{stderr}");
    assert!(has("SKIPPED None"), "{stdout}\n{stderr}");
    assert!(has("LOST false"), "{stdout}\n{stderr}");
    assert!(
        has(&format!("EXIT Some({CTRL_BREAK_EXIT_CODE})")),
        "{stdout}\n{stderr}"
    );
    assert!(has("DONE"), "{stdout}\n{stderr}");
    assert!(output.status.success(), "{stdout}\n{stderr}");
}

#[test]
fn native_adapter_runs_and_stops_a_process_through_the_traits() {
    let adapter = native().unwrap();
    assert_eq!(adapter.kind(), ScopeKind::WindowsJob);
    assert_eq!(adapter.mechanism(), "job-object");
    let detected = adapter.enforcement();
    println!("detected: {detected:#?}");
    assert!(!detected.verified);
    assert_eq!(detected.process_tree, Enforcement::Hard);
    assert_eq!(detected.disk, Enforcement::None);

    let scope = adapter
        .create(&unique("native"), &Limits::default())
        .unwrap();
    let identity = scope.identity();
    let mut guard = BoxGuard(Some(scope));
    let scope = guard.0.as_mut().unwrap();
    let Spawned { mut child, .. } = scope.spawn(&helper("sleep", &["60"])).unwrap();
    assert!(matches!(scope.members().unwrap(), Membership::Alive(_)));
    let membership = stop_scope(
        scope.as_mut(),
        Duration::from_millis(500),
        Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(membership, Membership::Empty);
    assert!(child.wait().unwrap().code.is_some());
    let evidence = guard.0.take().unwrap().destroy().unwrap();
    assert!(evidence.contains("ActiveProcesses=0"), "{evidence}");
    assert!(adapter.adopt(&identity).unwrap().is_none());
}

#[test]
fn self_test_verifies_every_dimension() {
    let report = adapter().self_test().unwrap();
    println!("self-test: {report:#?}");
    assert!(report.verified);
    assert_eq!(report.process_tree, Enforcement::Hard, "{:?}", report.notes);
    assert_eq!(
        report.process_count,
        Enforcement::Hard,
        "{:?}",
        report.notes
    );
    assert_eq!(report.memory, Enforcement::Hard, "{:?}", report.notes);
    assert_eq!(report.cpu, Enforcement::Hard, "{:?}", report.notes);
    assert_eq!(report.disk, Enforcement::None);
}
