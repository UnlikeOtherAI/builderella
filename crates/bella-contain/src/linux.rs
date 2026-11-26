//! cgroup v2 adapter.
//!
//! Each scope is one cgroup, `<base>/bella-<name>`, below the cgroup the
//! worker itself was started in. A process joins the scope after `fork` and
//! before `exec`, so every descendant is born inside it: double forks,
//! `setsid`, `nohup` and reparenting to init do not change cgroup
//! membership. Limits are the kernel's own (`cpu.max`, `memory.max`,
//! `pids.max`) and the whole tree is killed with `cgroup.kill`.
//!
//! # Where it works
//!
//! [`CgroupV2::detect`] accepts the worker's cgroup as the base when it is on
//! a pure cgroup v2 hierarchy, is writable, and was handed over to the
//! worker:
//!
//! * a systemd unit with `Delegate=yes`, recognised by the `delegate`
//!   attribute systemd puts on the unit's cgroup,
//! * for a worker that is not root, a cgroup owned by the worker's user,
//!   which is what systemd produces for `Delegate=yes` with `User=` and for
//!   the units of a user's own systemd instance, or
//! * the root of a private cgroup namespace, as in a container.
//!
//! A root worker can write to any cgroup, so for root only the attribute or
//! a namespace root counts. cgroup v1, the hybrid layout, a read-only mount
//! and a cgroup that some other manager still owns are all reported as
//! [`ContainError::Unavailable`] with the reason.
//!
//! # What the adapter changes
//!
//! cgroup v2 only lets controllers be enabled for the children of a cgroup
//! that has no processes of its own. On first use the adapter therefore
//! creates `<base>/supervisor`, moves every process of the base cgroup
//! (the worker and anything it had already started) into it, and enables the
//! `cpu`, `memory` and `pids` controllers for the base's children. A
//! controller that is missing or cannot be enabled is reported as
//! [`Enforcement::None`] with a note; the scope still contains the process
//! tree.
//!
//! # Guarantees and their limits
//!
//! * Membership cannot be lost by forking, daemonising or changing session.
//!   It can be changed by a job that deliberately writes to the cgroup
//!   filesystem with the worker's credentials. This is resource containment,
//!   not a security sandbox.
//! * `cpu.weight` only ranks a scope against its siblings below the base.
//!   Priority against the rest of the machine is set on the worker's unit
//!   (`CPUWeight=`), which the adapter does not own.
//! * `cgroup.kill` needs Linux 5.14. Older kernels are handled by freezing
//!   the scope (`cgroup.freeze`, Linux 5.2) and sending `SIGKILL` to its
//!   members until none is left.
//! * `SIGTERM` is a request, not a guarantee: a member that blocks or
//!   ignores it can keep forking. [`Scope::terminate`] signals members that
//!   appear shortly after their parent was signalled, but only `kill` is
//!   certain to reach everything.
//! * Signals go through pidfds (Linux 5.3) after checking the target still
//!   belongs to the scope, so a reused PID is never signalled. Before 5.3
//!   that check and the signal are two steps and a narrow reuse window
//!   remains.
//! * `memory.peak` needs Linux 5.19. Without it the reported peak is the
//!   highest `memory.current` seen by calls to [`Scope::usage`] on the same
//!   scope object, which can miss a spike between samples.
//!
//! # Out of memory
//!
//! A scope over `memory.max` has processes killed by the kernel with
//! `SIGKILL`, which looks like any other crash. [`CgroupScope::oom_kills`]
//! and [`oom_kills`] report the kernel's count so the worker can tell
//! resource exhaustion from an ordinary failure. Read it before the scope is
//! destroyed; the counter goes away with the cgroup.

use crate::{
    Child, ContainError, Containment, ExitInfo, Limits, Membership, ProcessSpec, Scope, Spawned,
};
use bella_core::protocol::{ScopeIdentity, ScopeKind, UsageSummary};
use bella_core::resources::{Enforcement, EnforcementReport};
use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const CGROUP_MOUNT: &str = "/sys/fs/cgroup";
const SUPERVISOR: &str = "supervisor";
const SCOPE_PREFIX: &str = "bella-";
const CPU_PERIOD_USEC: u64 = 100_000;
/// Smallest quota the kernel accepts in `cpu.max`.
const CPU_MIN_QUOTA_USEC: u64 = 1_000;
const CPU_WEIGHT_DEFAULT: &str = "100";
const CPU_WEIGHT_LOW: &str = "10";
const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;
const TMPFS_MAGIC: i64 = 0x0102_1994;
const MAX_NAME_LEN: usize = 128;
/// A shell loop that burns CPU for a while and then ends on its own, so a
/// self-test that fails to kill it leaves nothing running for long.
const BUSY_LOOP: &str = "i=0; while [ \"$i\" -lt 30000000 ]; do i=$((i+1)); done";
/// How long `terminate` keeps looking for members that were forked after
/// their parent was signalled.
const TERMINATE_WINDOW: Duration = Duration::from_millis(100);
/// Flags word of the pidfd system calls; none are used.
const NO_FLAGS: libc::c_uint = 0;

/// cgroup v2 containment rooted at the worker's delegated cgroup.
pub struct CgroupV2 {
    base: PathBuf,
    prepared: OnceLock<Prepared>,
}

/// Result of the one-time setup of the base cgroup.
struct Prepared {
    cpu: bool,
    memory: bool,
    pids: bool,
    notes: Vec<String>,
}

impl CgroupV2 {
    /// Locate the delegated cgroup this process runs in.
    ///
    /// Reads `/proc/self/cgroup` and inspects `/sys/fs/cgroup`; changes
    /// nothing. `Err(Unavailable)` carries the reason when the hierarchy is
    /// cgroup v1 or hybrid, is not writable, or was not delegated to this
    /// process.
    pub fn detect() -> Result<Self, ContainError> {
        let mount = Path::new(CGROUP_MOUNT);
        match fs_type(mount) {
            Ok(CGROUP2_SUPER_MAGIC) => {}
            Ok(TMPFS_MAGIC) => {
                return Err(unavailable(format!(
                    "{CGROUP_MOUNT} is a cgroup v1 or hybrid hierarchy; the unified cgroup v2 hierarchy is required"
                )));
            }
            Ok(other) => {
                return Err(unavailable(format!(
                    "{CGROUP_MOUNT} is not a cgroup v2 filesystem (type {other:#x})"
                )));
            }
            Err(error) => {
                return Err(unavailable(format!(
                    "cannot inspect {CGROUP_MOUNT}: {error}"
                )));
            }
        }
        let text = fs::read_to_string("/proc/self/cgroup")
            .map_err(|error| unavailable(format!("cannot read /proc/self/cgroup: {error}")))?;
        let relative = parse_own_cgroup(&text).map_err(unavailable)?;
        let own = match relative.trim_start_matches('/') {
            "" => mount.to_path_buf(),
            below => mount.join(below),
        };

        // A worker that already moved itself (or was restarted by a process
        // that had) sits in the supervisor leaf; the base is one level up.
        let mut candidates = Vec::new();
        if own != mount
            && own.file_name().is_some_and(|name| name == SUPERVISOR)
            && let Some(parent) = own.parent()
        {
            candidates.push(parent.to_path_buf());
        }
        candidates.push(own);

        let mut reason = String::new();
        for candidate in candidates {
            match usable_base(&candidate, mount) {
                Ok(()) => {
                    return Ok(Self {
                        base: candidate,
                        prepared: OnceLock::new(),
                    });
                }
                Err(why) => reason = why,
            }
        }
        Err(unavailable(reason))
    }

    /// The cgroup directory that holds the supervisor leaf and every scope.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Like [`Containment::create`], returning the concrete scope type.
    pub fn create_scope(&self, name: &str, limits: &Limits) -> Result<CgroupScope, ContainError> {
        validate_name(name)?;
        self.prepared();
        let path = self.scope_path(name);
        fs::create_dir(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                ContainError::Other(format!(
                    "scope `{name}` already exists at {}",
                    path.display()
                ))
            } else {
                ContainError::Other(format!("cannot create cgroup {}: {error}", path.display()))
            }
        })?;
        let scope = CgroupScope::bind(name, path)?;
        if let Err(error) = scope.apply(limits) {
            let _ = fs::remove_dir(&scope.path);
            return Err(error);
        }
        Ok(scope)
    }

    /// Like [`Containment::adopt`], returning the concrete scope type.
    ///
    /// Only a cgroup at the exact place this adapter would have created it is
    /// adopted, so a damaged journal cannot point the worker at somebody
    /// else's cgroup. Adopting sends no signal and reads no PID.
    pub fn adopt_scope(
        &self,
        identity: &ScopeIdentity,
    ) -> Result<Option<CgroupScope>, ContainError> {
        if identity.kind != ScopeKind::LinuxCgroup {
            return Err(ContainError::Other(format!(
                "scope `{}` was not created by the cgroup adapter",
                identity.name
            )));
        }
        validate_name(&identity.name)?;
        let path = self.scope_path(&identity.name);
        if !identity.detail.is_empty() && Path::new(&identity.detail) != path {
            return Err(ContainError::Other(format!(
                "scope `{}` was recorded at {} but this worker's cgroup would hold it at {}; refusing to adopt",
                identity.name,
                identity.detail,
                path.display()
            )));
        }
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(ContainError::Other(format!(
                    "{} is not a cgroup",
                    path.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let scope = CgroupScope::bind(&identity.name, path)?;
        // A worker that died in the middle of a kill may have left the scope
        // frozen, which would hold back SIGTERM delivery forever.
        scope.thaw();
        Ok(Some(scope))
    }

    fn scope_path(&self, name: &str) -> PathBuf {
        self.base.join(format!("{SCOPE_PREFIX}{name}"))
    }

    fn prepared(&self) -> &Prepared {
        self.prepared.get_or_init(|| prepare(&self.base))
    }

    /// Run `body` against a fresh scope and always kill and remove it.
    fn with_scope(
        &self,
        name: &str,
        limits: &Limits,
        body: impl FnOnce(&mut CgroupScope) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut scope = self
            .create_scope(name, limits)
            .map_err(|error| error.to_string())?;
        let outcome = body(&mut scope);
        let cleanup = scope
            .kill()
            .map_err(|error| error.to_string())
            .and_then(|()| {
                if poll_until(Duration::from_secs(5), || {
                    matches!(scope.members(), Ok(Membership::Empty))
                }) {
                    scope.remove().map(drop).map_err(|error| error.to_string())
                } else {
                    Err("the scope was not empty 5 s after it was killed".to_string())
                }
            });
        outcome.and(cleanup)
    }

    fn check_tree(&self, name: &str) -> Result<(), String> {
        self.with_scope(name, &Limits::default(), |scope| {
            // The subshell backgrounds a sleeper and exits, so the sleeper is
            // an orphan by the time the root shell returns.
            let mut root = scope
                .spawn(&shell("(sleep 300 </dev/null >/dev/null 2>&1 &)\nexit 0"))
                .map_err(|error| error.to_string())?;
            if wait_bounded(root.child.as_mut(), Duration::from_secs(5)).is_none() {
                return Err("the root shell did not exit".to_string());
            }
            match scope.members() {
                Ok(Membership::Alive(_)) => {}
                other => {
                    return Err(format!(
                        "an orphaned descendant was not found in the scope ({other:?})"
                    ));
                }
            }
            scope.kill().map_err(|error| error.to_string())?;
            if !poll_until(Duration::from_secs(5), || {
                matches!(scope.members(), Ok(Membership::Empty))
            }) {
                return Err("an orphaned descendant survived the kill".to_string());
            }
            Ok(())
        })
    }

    fn check_pids(&self, name: &str) -> Result<(), String> {
        const LIMIT: u32 = 8;
        let limits = Limits {
            max_processes: Some(LIMIT),
            ..Limits::default()
        };
        self.with_scope(name, &limits, |scope| {
            let mut root = scope
                .spawn(&shell(
                    "i=0\nwhile [ \"$i\" -lt 64 ]; do sleep 300 </dev/null >/dev/null 2>&1 & i=$((i+1)); done\nwait",
                ))
                .map_err(|error| error.to_string())?;
            let events = scope.path.join("pids.events");
            let current = scope.path.join("pids.current");
            let mut highest = 0;
            let refused = poll_until(Duration::from_secs(5), || {
                highest = highest.max(read_u64(&current).unwrap_or(0));
                read_kv(&events, "max").ok().flatten().unwrap_or(0) > 0
            });
            highest = highest.max(read_u64(&current).unwrap_or(0));
            scope.kill().map_err(|error| error.to_string())?;
            let _ = wait_bounded(root.child.as_mut(), Duration::from_secs(5));
            if !refused {
                return Err(format!(
                    "pids.max = {LIMIT} never refused a fork ({highest} tasks seen)"
                ));
            }
            if highest > u64::from(LIMIT) {
                return Err(format!(
                    "{highest} tasks were counted under pids.max = {LIMIT}"
                ));
            }
            Ok(())
        })
    }

    fn check_memory(&self, name: &str) -> Result<(), String> {
        const LIMIT: u64 = 32 << 20;
        let limits = Limits {
            memory_bytes: Some(LIMIT),
            ..Limits::default()
        };
        self.with_scope(name, &limits, |scope| {
            // Doubles a string from 1 KiB to 256 MiB and then stops, so an
            // unenforced limit costs a bounded amount of memory.
            let mut root = scope
                .spawn(&shell(
                    "x=$(printf %01024d 0)\ni=0\nwhile [ \"$i\" -lt 18 ]; do x=\"$x$x\"; i=$((i+1)); done\nexit 0",
                ))
                .map_err(|error| error.to_string())?;
            let exit = wait_bounded(root.child.as_mut(), Duration::from_secs(30));
            let killed = scope.oom_kills().unwrap_or(0);
            match exit {
                None => Err("the allocator was still running after 30 s".to_string()),
                Some(_) if killed > 0 => Ok(()),
                Some(exit) => Err(format!(
                    "the allocator passed memory.max = {LIMIT} without an OOM kill (exit {exit:?})"
                )),
            }
        })
    }

    fn check_cpu(&self, name: &str) -> Result<(), String> {
        let limits = Limits {
            cpu_milli: Some(100),
            ..Limits::default()
        };
        self.with_scope(name, &limits, |scope| {
            let mut root = scope
                .spawn(&shell(BUSY_LOOP))
                .map_err(|error| error.to_string())?;
            let stat = scope.path.join("cpu.stat");
            let throttled = poll_until(Duration::from_secs(3), || {
                read_kv(&stat, "nr_throttled").ok().flatten().unwrap_or(0) >= 2
            });
            scope.kill().map_err(|error| error.to_string())?;
            let _ = wait_bounded(root.child.as_mut(), Duration::from_secs(5));
            if throttled {
                Ok(())
            } else {
                Err("a busy loop under cpu.max was never throttled".to_string())
            }
        })
    }
}

impl Containment for CgroupV2 {
    fn kind(&self) -> ScopeKind {
        ScopeKind::LinuxCgroup
    }

    fn mechanism(&self) -> &'static str {
        "cgroup-v2"
    }

    /// Sets up the base cgroup on first call (see the module documentation)
    /// and reports each dimension whose controller is enabled as `Hard`.
    fn enforcement(&self) -> EnforcementReport {
        let prepared = self.prepared();
        let level = |enabled: bool| {
            if enabled {
                Enforcement::Hard
            } else {
                Enforcement::None
            }
        };
        let mut notes = prepared.notes.clone();
        notes.push(
            "membership survives fork, setsid and daemonising; a job that rewrites the worker's cgroup files can still leave its scope"
                .to_string(),
        );
        EnforcementReport {
            cpu: level(prepared.cpu),
            memory: level(prepared.memory),
            process_tree: Enforcement::Hard,
            process_count: level(prepared.pids),
            disk: Enforcement::None,
            verified: false,
            notes,
        }
    }

    /// Starts real processes: an orphaned background sleeper that must stay
    /// in its scope and die with it, a fork loop against `pids.max`, a
    /// bounded allocator against `memory.max` and a busy loop against
    /// `cpu.max`. A check that fails downgrades its dimension to `None` and
    /// adds a note. Needs `/bin/sh` and `sleep`.
    fn self_test(&self) -> Result<EnforcementReport, ContainError> {
        static RUN: AtomicU64 = AtomicU64::new(0);
        let prepared = self.prepared();
        let mut report = self.enforcement();
        report.verified = true;
        let tag = format!(
            "selftest-{}-{}",
            std::process::id(),
            RUN.fetch_add(1, Ordering::Relaxed)
        );

        if let Err(why) = self.check_tree(&format!("{tag}-tree")) {
            report.notes.push(format!("process tree: {why}"));
            report.notes.push(
                "limits were not tested because the process tree was not contained".to_string(),
            );
            report.process_tree = Enforcement::None;
            report.cpu = Enforcement::None;
            report.memory = Enforcement::None;
            report.process_count = Enforcement::None;
            return Ok(report);
        }
        if prepared.pids
            && let Err(why) = self.check_pids(&format!("{tag}-pids"))
        {
            report.notes.push(format!("process count: {why}"));
            report.process_count = Enforcement::None;
        }
        if prepared.memory
            && let Err(why) = self.check_memory(&format!("{tag}-memory"))
        {
            report.notes.push(format!("memory: {why}"));
            report.memory = Enforcement::None;
        }
        if prepared.cpu
            && let Err(why) = self.check_cpu(&format!("{tag}-cpu"))
        {
            report.notes.push(format!("cpu: {why}"));
            report.cpu = Enforcement::None;
        }
        Ok(report)
    }

    fn create(&self, name: &str, limits: &Limits) -> Result<Box<dyn Scope>, ContainError> {
        Ok(Box::new(self.create_scope(name, limits)?))
    }

    fn adopt(&self, identity: &ScopeIdentity) -> Result<Option<Box<dyn Scope>>, ContainError> {
        Ok(self
            .adopt_scope(identity)?
            .map(|scope| Box::new(scope) as Box<dyn Scope>))
    }
}

/// One cgroup holding every process of one attempt.
///
/// Dropping the value leaves the cgroup and its processes alone; they are
/// owned by the journaled [`ScopeIdentity`], not by this handle.
pub struct CgroupScope {
    name: String,
    path: PathBuf,
    /// `<path>/cgroup.procs`, NUL-terminated ahead of any fork so the child
    /// can open it without allocating.
    procs: CString,
    /// The cgroup as `/proc/<pid>/cgroup` prints it, e.g. `/bella-a1`.
    relative: String,
    /// Highest `memory.current` seen by `usage`, for kernels without
    /// `memory.peak`.
    peak_seen: AtomicU64,
}

impl CgroupScope {
    fn bind(name: &str, path: PathBuf) -> Result<Self, ContainError> {
        let procs = cstring(&path.join("cgroup.procs"))?;
        let relative = match path.strip_prefix(CGROUP_MOUNT) {
            Ok(rest) => format!("/{}", rest.to_string_lossy()),
            Err(_) => {
                return Err(ContainError::Other(format!(
                    "{} is outside {CGROUP_MOUNT}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            name: name.to_string(),
            path,
            procs,
            relative,
            peak_seen: AtomicU64::new(0),
        })
    }

    /// The scope's cgroup directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Processes of this scope (and its child cgroups) that the kernel's OOM
    /// killer has killed so far. `None` when the memory controller is not
    /// enabled for the scope or the cgroup is gone.
    pub fn oom_kills(&self) -> Option<u64> {
        read_kv(&self.path.join("memory.events"), "oom_kill")
            .ok()
            .flatten()
    }

    /// Kill every member without `cgroup.kill`: freeze the scope where
    /// `cgroup.freeze` exists so nothing can fork, send `SIGKILL` to each
    /// member, and repeat until none is listed or two seconds have passed.
    /// [`Scope::kill`] falls back to this on kernels before 5.14.
    pub fn kill_by_signal(&mut self) -> Result<(), ContainError> {
        let frozen = self.freeze();
        let outcome = self.kill_members();
        if frozen {
            self.thaw();
        }
        outcome
    }

    fn kill_members(&self) -> Result<(), ContainError> {
        let start = Instant::now();
        loop {
            if self.signal_all(libc::SIGKILL)? == 0 || start.elapsed() >= Duration::from_secs(2) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Send `signal` to every current member and return how many were
    /// listed. A scope whose cgroup is gone has none.
    fn signal_all(&self, signal: libc::c_int) -> Result<usize, ContainError> {
        let pids = match self.pids() {
            Ok(pids) => pids,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        for pid in &pids {
            self.signal_member(*pid, signal)?;
        }
        Ok(pids.len())
    }

    /// Ask the kernel to freeze the scope and wait briefly for it to report
    /// the frozen state. Returns whether the freeze was requested.
    fn freeze(&self) -> bool {
        if write_ctl(&self.path.join("cgroup.freeze"), "1").is_err() {
            return false;
        }
        let events = self.path.join("cgroup.events");
        poll_until(Duration::from_secs(1), || {
            read_kv(&events, "frozen").ok().flatten() == Some(1)
        });
        true
    }

    fn thaw(&self) {
        let _ = write_ctl(&self.path.join("cgroup.freeze"), "0");
    }

    /// Write one limit file. A missing file means its controller is not
    /// enabled for the scope, which `enforcement` already reports.
    fn set(&self, file: &str, value: &str) -> Result<(), ContainError> {
        match write_ctl(&self.path.join(file), value) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ContainError::Other(format!(
                "cannot write `{value}` to {file} of scope `{}`: {error}",
                self.name
            ))),
        }
    }

    fn apply(&self, limits: &Limits) -> Result<(), ContainError> {
        let quota = match limits.cpu_milli {
            Some(milli) => cpu_quota_usec(milli).to_string(),
            None => "max".to_string(),
        };
        self.set("cpu.max", &format!("{quota} {CPU_PERIOD_USEC}"))?;
        self.set(
            "cpu.weight",
            if limits.low_priority {
                CPU_WEIGHT_LOW
            } else {
                CPU_WEIGHT_DEFAULT
            },
        )?;
        match limits.memory_bytes {
            Some(bytes) => {
                self.set("memory.max", &bytes.to_string())?;
                // Above memory.high the kernel reclaims and slows the scope
                // down, so a job that creeps towards its limit is pushed back
                // before the OOM killer is the only tool left.
                self.set("memory.high", &memory_high(bytes).to_string())?;
                // Without this a scope over its limit is swapped out instead
                // of stopped: the machine thrashes and the memory the
                // scheduler reserved for other work is not actually free.
                self.set("memory.swap.max", "0")?;
            }
            None => {
                self.set("memory.max", "max")?;
                self.set("memory.high", "max")?;
                self.set("memory.swap.max", "max")?;
            }
        }
        match limits.max_processes {
            Some(count) => self.set("pids.max", &count.to_string()),
            None => self.set("pids.max", "max"),
        }
    }

    /// The scope's cgroup and every cgroup below it, parents first. A job
    /// can only create child cgroups when it has write access to the scope.
    fn tree(&self) -> io::Result<Vec<PathBuf>> {
        let mut dirs = vec![self.path.clone()];
        let mut index = 0;
        while index < dirs.len() {
            let mut children = Vec::new();
            match fs::read_dir(&dirs[index]) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            children.push(entry.path());
                        }
                    }
                }
                // A child cgroup removed while walking is simply gone.
                Err(error) if error.kind() == io::ErrorKind::NotFound && index > 0 => {}
                Err(error) => return Err(error),
            }
            dirs.append(&mut children);
            index += 1;
        }
        Ok(dirs)
    }

    /// PIDs of every member, including those in child cgroups. A member in
    /// another PID namespace is listed as 0.
    fn pids(&self) -> io::Result<Vec<u32>> {
        let mut pids = Vec::new();
        for (index, dir) in self.tree()?.iter().enumerate() {
            match fs::read_to_string(dir.join("cgroup.procs")) {
                Ok(text) => pids.extend(parse_pids(&text)),
                Err(error) if error.kind() == io::ErrorKind::NotFound && index > 0 => {}
                Err(error) => return Err(error),
            }
        }
        Ok(pids)
    }

    /// Whether `pid` currently names a process of this scope.
    fn is_member(&self, pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .map(|text| cgroup_is_within(&text, &self.relative))
            .unwrap_or(false)
    }

    /// Signal one member. The process is pinned with a pidfd before its
    /// membership is checked, so the signal can only reach the process that
    /// was checked.
    fn signal_member(&self, pid: u32, signal: libc::c_int) -> Result<(), ContainError> {
        // 0 is a member this PID namespace cannot name; signalling it would
        // hit the worker's own process group.
        if pid == 0 {
            return Ok(());
        }
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return Ok(());
        };
        // SAFETY: pidfd_open takes a PID and a flags word and returns a new
        // descriptor or -1; no memory is shared with the kernel.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, NO_FLAGS) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(()),
                Some(libc::ENOSYS) => self.signal_by_pid(raw_pid, pid, signal),
                _ => Err(error.into()),
            };
        }
        // SAFETY: the kernel just returned this descriptor and nothing else
        // owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        if !self.is_member(pid) {
            return Ok(());
        }
        // SAFETY: `fd` is a live pidfd; a null siginfo requests the same
        // behaviour as kill(2), and the flags word is 0.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                NO_FLAGS,
            )
        };
        if sent < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Kernels before 5.3 have no pidfds: check membership, then signal.
    fn signal_by_pid(
        &self,
        raw_pid: libc::pid_t,
        pid: u32,
        signal: libc::c_int,
    ) -> Result<(), ContainError> {
        if !self.is_member(pid) {
            return Ok(());
        }
        // SAFETY: kill takes a PID and a signal number; `raw_pid` is
        // positive, so it names one process and never a group.
        if unsafe { libc::kill(raw_pid, signal) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn remove(&self) -> Result<String, ContainError> {
        if !self.path.exists() {
            return Ok("cgroup already removed".to_string());
        }
        if self.members()? != Membership::Empty {
            return Err(ContainError::NotEmpty(self.name.clone()));
        }
        // Children first. The kernel itself refuses to remove a cgroup that
        // still has a process, so a successful rmdir is the proof.
        for dir in self.tree()?.iter().rev() {
            match remove_cgroup(dir) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
                    return Err(ContainError::NotEmpty(self.name.clone()));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok("cgroup.procs empty; cgroup removed".to_string())
    }
}

impl Scope for CgroupScope {
    /// `detail` is the cgroup directory and never changes.
    fn identity(&self) -> ScopeIdentity {
        ScopeIdentity {
            kind: ScopeKind::LinuxCgroup,
            name: self.name.clone(),
            detail: self.path.to_string_lossy().into_owned(),
        }
    }

    /// The child moves itself into the cgroup between `fork` and `exec` and
    /// becomes the leader of a new session, so it has no controlling
    /// terminal and shares no process group with the worker.
    fn spawn(&mut self, spec: &ProcessSpec) -> Result<Spawned, ContainError> {
        let Some(program) = spec.argv.first() else {
            return Err(ContainError::Spawn {
                program: String::new(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "argv is empty"),
            });
        };
        let mut command = Command::new(program);
        command
            .args(&spec.argv[1..])
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let procs = self.procs.clone();
        let enter = move || -> io::Result<()> {
            join_cgroup(&procs)?;
            // SAFETY: setsid takes no arguments and is async-signal-safe.
            if unsafe { libc::setsid() } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: the hook runs in the forked child before exec. It only
        // calls open, write, close and setsid, all async-signal-safe, on a
        // path that was allocated before the fork; it neither allocates nor
        // takes a lock.
        unsafe {
            command.pre_exec(enter);
        }
        let mut child = command.spawn().map_err(|source| ContainError::Spawn {
            program: program.clone(),
            source,
        })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ContainError::Other(
                "the child was started without output pipes".to_string(),
            ));
        };
        Ok(Spawned {
            child: Box::new(CgroupChild(child)),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
        })
    }

    /// CPU time and peak memory cover exited members too, for as long as the
    /// cgroup exists. `processes` is `pids.current`, which counts threads.
    /// Disk use is not accounted by cgroups and is reported as 0.
    fn usage(&self) -> Result<UsageSummary, ContainError> {
        let cpu_usec = read_kv(&self.path.join("cpu.stat"), "usage_usec")?.unwrap_or(0);
        let current = read_u64(&self.path.join("memory.current")).unwrap_or(0);
        let seen = self.peak_seen.fetch_max(current, Ordering::Relaxed);
        let peak = read_u64(&self.path.join("memory.peak"))
            .unwrap_or(0)
            .max(seen)
            .max(current);
        let processes = match read_u64(&self.path.join("pids.current")) {
            Some(count) => count,
            None => self.pids()?.len() as u64,
        };
        Ok(UsageSummary {
            cpu_time_ms: cpu_usec / 1000,
            memory_current_bytes: current,
            memory_peak_bytes: peak,
            processes: u32::try_from(processes).unwrap_or(u32::MAX),
            disk_bytes: 0,
        })
    }

    /// `Empty` only when no PID is listed in the scope or below it and the
    /// kernel reports the cgroup as unpopulated. Zombies are not members.
    fn members(&self) -> Result<Membership, ContainError> {
        let pids = match self.pids() {
            Ok(pids) => pids,
            // A removed cgroup cannot hold a process.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Membership::Empty),
            Err(error) => return Err(error.into()),
        };
        if !pids.is_empty() {
            return Ok(Membership::Alive(
                u32::try_from(pids.len()).unwrap_or(u32::MAX),
            ));
        }
        match read_kv(&self.path.join("cgroup.events"), "populated") {
            Ok(Some(0)) => Ok(Membership::Empty),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Membership::Empty),
            _ => Ok(Membership::Unknown),
        }
    }

    fn set_limits(&mut self, limits: &Limits) -> Result<(), ContainError> {
        if !self.path.is_dir() {
            return Err(ContainError::Other(format!(
                "scope `{}` no longer exists",
                self.name
            )));
        }
        self.apply(limits)
    }

    /// `SIGTERM` to every member, each exactly once.
    ///
    /// A process can be forked by a member that has the signal queued but
    /// has not acted on it yet: shells and `posix_spawn` block signals while
    /// they fork, and the child does not inherit what is pending. The scope
    /// is therefore re-read every few milliseconds and members that appeared
    /// since are signalled too, until three reads in a row find nobody new
    /// or [`TERMINATE_WINDOW`] has passed. A process created after that by a
    /// member that ignores the signal is not asked; `kill` reaches it.
    fn terminate(&mut self) -> Result<(), ContainError> {
        let start = Instant::now();
        let mut signalled: Vec<u32> = Vec::new();
        let mut quiet = 0;
        loop {
            let pids = match self.pids() {
                Ok(pids) => pids,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if pids.is_empty() {
                return Ok(());
            }
            let mut found_new = false;
            for pid in pids {
                if !signalled.contains(&pid) {
                    self.signal_member(pid, libc::SIGTERM)?;
                    signalled.push(pid);
                    found_new = true;
                }
            }
            quiet = if found_new { 0 } else { quiet + 1 };
            if quiet >= 3 || start.elapsed() >= TERMINATE_WINDOW {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `cgroup.kill` kills the scope and every child cgroup atomically,
    /// including processes that are forking. Returns once the kill is
    /// requested; poll `members` for the result.
    fn kill(&mut self) -> Result<(), ContainError> {
        match write_ctl(&self.path.join("cgroup.kill"), "1") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.path.is_dir() {
                    self.kill_by_signal()
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn destroy(self: Box<Self>) -> Result<String, ContainError> {
        self.remove()
    }
}

struct CgroupChild(std::process::Child);

impl Child for CgroupChild {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitInfo>> {
        Ok(self.0.try_wait()?.map(exit_info))
    }

    fn wait(&mut self) -> io::Result<ExitInfo> {
        self.0.wait().map(exit_info)
    }
}

fn exit_info(status: std::process::ExitStatus) -> ExitInfo {
    ExitInfo {
        code: status.code(),
        signal: status.signal(),
    }
}

/// OOM kills recorded for a journaled scope, for callers that only hold the
/// [`ScopeIdentity`] or a `Box<dyn Scope>`. `None` when the identity is not
/// a cgroup scope, the memory controller is not enabled for it, or the
/// cgroup is gone.
pub fn oom_kills(identity: &ScopeIdentity) -> Option<u64> {
    if identity.kind != ScopeKind::LinuxCgroup {
        return None;
    }
    let path = Path::new(&identity.detail);
    if !path.starts_with(CGROUP_MOUNT) {
        return None;
    }
    read_kv(&path.join("memory.events"), "oom_kill")
        .ok()
        .flatten()
}

/// Move the calling process into the cgroup whose `cgroup.procs` is `procs`.
/// Async-signal-safe: no allocation, no locks.
fn join_cgroup(procs: &CStr) -> io::Result<()> {
    // SAFETY: `procs` is a valid NUL-terminated string that outlives the call.
    let fd = unsafe { libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // "0" means the writing process itself.
    let zero = b"0";
    // SAFETY: `fd` is open for writing and `zero` is readable for one byte.
    let written = unsafe { libc::write(fd, zero.as_ptr().cast(), zero.len()) };
    let result = if written == 1 {
        Ok(())
    } else if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Err(io::Error::from_raw_os_error(libc::EIO))
    };
    // SAFETY: `fd` was opened above and is not used afterwards.
    unsafe { libc::close(fd) };
    result
}

/// Make the base cgroup able to hold limited children and record which
/// controllers ended up enabled for them. Never fails: what could not be
/// done is reported in the notes and by the missing controllers.
fn prepare(base: &Path) -> Prepared {
    let mut notes = Vec::new();
    if let Err(error) = vacate(base) {
        notes.push(format!(
            "could not move the worker's processes out of {} into its {SUPERVISOR} cgroup: {error}",
            base.display()
        ));
    }
    let control = base.join("cgroup.subtree_control");
    let available = read_words(&base.join("cgroup.controllers"));
    for controller in ["cpu", "memory", "pids"] {
        if !available.iter().any(|name| name == controller) {
            notes.push(format!(
                "the {controller} controller is not available in {}",
                base.display()
            ));
            continue;
        }
        // One write per controller: a combined write is all or nothing.
        if let Err(error) = write_ctl(&control, &format!("+{controller}")) {
            notes.push(format!(
                "the {controller} controller could not be enabled for scopes: {error}"
            ));
        }
    }
    let enabled = read_words(&control);
    let has = |controller: &str| enabled.iter().any(|name| name == controller);
    let memory = has("memory");
    if memory && !base.join(SUPERVISOR).join("memory.swap.max").exists() {
        notes.push(
            "memory.swap.max is not available: a scope is limited in RAM but may still use swap"
                .to_string(),
        );
    }
    Prepared {
        cpu: has("cpu"),
        memory,
        pids: has("pids"),
        notes,
    }
}

/// Move every process of `base` into `<base>/supervisor`.
fn vacate(base: &Path) -> io::Result<()> {
    let procs = base.join("cgroup.procs");
    let leaf = base.join(SUPERVISOR);
    let mut last_error = None;
    // New processes can appear while the list is being moved.
    for _ in 0..10 {
        let pids = parse_pids(&fs::read_to_string(&procs)?);
        if pids.is_empty() {
            return Ok(());
        }
        match fs::create_dir(&leaf) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        for pid in pids {
            match write_ctl(&leaf.join("cgroup.procs"), &pid.to_string()) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => last_error = Some(error),
            }
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("processes kept appearing in the cgroup")))
}

/// Check that `path` can hold scopes and belongs to this process.
fn usable_base(path: &Path, mount: &Path) -> Result<(), String> {
    // SAFETY: geteuid has no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let hint = "run the worker in a systemd unit with Delegate=yes or in a container with a private cgroup namespace";
    if let Err(error) = check_access(path) {
        return Err(format!(
            "cgroup {} is not writable by uid {euid} ({error}); {hint}",
            path.display()
        ));
    }
    for file in ["cgroup.procs", "cgroup.subtree_control"] {
        if let Err(error) = fs::OpenOptions::new().write(true).open(path.join(file)) {
            return Err(format!(
                "{} is not writable by uid {euid} ({error}); {hint}",
                path.join(file).display()
            ));
        }
    }
    if path == mount {
        // Only a namespace root has the files of an ordinary cgroup here.
        return if path.join("cgroup.events").exists() {
            Ok(())
        } else {
            Err(format!(
                "the worker runs in the host's root cgroup, which cannot be delegated; {hint}"
            ))
        };
    }
    if has_delegate_attribute(path) {
        return Ok(());
    }
    // Root owns every cgroup, so ownership says nothing about delegation.
    if euid != 0 && fs::metadata(path).is_ok_and(|meta| meta.uid() == euid) {
        return Ok(());
    }
    Err(format!(
        "cgroup {} is writable but was not delegated to the worker; {hint}",
        path.display()
    ))
}

/// Whether systemd marked the cgroup as delegated (`Delegate=yes`).
fn has_delegate_attribute(path: &Path) -> bool {
    let Ok(path) = cstring(path) else {
        return false;
    };
    [c"trusted.delegate", c"user.delegate"].iter().any(|name| {
        let mut value = [0u8; 8];
        // SAFETY: both strings are NUL-terminated and `value` is writable
        // for the length passed.
        let length = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        length == 1 && value[0] == b'1'
    })
}

fn check_access(path: &Path) -> io::Result<()> {
    let path = cstring(path)?;
    // SAFETY: `path` is a valid NUL-terminated string.
    if unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[allow(clippy::unnecessary_cast)] // `f_type` is not the same integer type on every target.
fn fs_type(path: &Path) -> io::Result<i64> {
    let path = cstring(path)?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is writable storage of the
    // size statfs expects.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs returned 0, so it filled the structure in.
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_type as i64)
}

/// The cgroup v2 path of this process from the text of `/proc/self/cgroup`.
fn parse_own_cgroup(text: &str) -> Result<&str, String> {
    let Some(path) = text.lines().find_map(|line| line.strip_prefix("0::")) else {
        return Err(
            "this process has no cgroup v2 membership (no `0::` line in /proc/self/cgroup)"
                .to_string(),
        );
    };
    if path.ends_with(" (deleted)") {
        return Err(format!("this process's cgroup `{path}` has been removed"));
    }
    if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return Err(format!(
            "this process's cgroup `{path}` lies outside its cgroup namespace"
        ));
    }
    Ok(path)
}

/// Whether the `0::` line of a `/proc/<pid>/cgroup` text names `scope` or a
/// cgroup below it.
fn cgroup_is_within(text: &str, scope: &str) -> bool {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .is_some_and(|path| {
            path == scope
                || path
                    .strip_prefix(scope)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

fn validate_name(name: &str) -> Result<(), ContainError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(ContainError::Other(format!(
            "invalid scope name `{name}`: expected 1 to {MAX_NAME_LEN} characters of [a-z0-9_-]"
        )))
    }
}

/// `cpu.max` quota for one 100 ms period.
fn cpu_quota_usec(cpu_milli: u64) -> u64 {
    (cpu_milli.saturating_mul(CPU_PERIOD_USEC) / 1000).max(CPU_MIN_QUOTA_USEC)
}

/// `memory.high` for a given `memory.max`: 5% below it.
fn memory_high(max_bytes: u64) -> u64 {
    max_bytes - max_bytes / 20
}

/// Value of `key` in a flat keyed file such as `cpu.stat`.
fn parse_kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some(key))
            .then(|| parts.next()?.parse().ok())
            .flatten()
    })
}

/// PIDs listed in the text of a `cgroup.procs` file.
fn parse_pids(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn read_kv(path: &Path, key: &str) -> io::Result<Option<u64>> {
    Ok(parse_kv(&fs::read_to_string(path)?, key))
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_words(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|text| text.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Write to an existing control file. Never creates one: a missing file must
/// stay a `NotFound` error.
fn write_ctl(path: &Path, value: &str) -> io::Result<()> {
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value.as_bytes())
}

/// Remove one cgroup directory, tolerating its absence. `EBUSY` is retried
/// briefly because a process that just died may not be accounted yet.
fn remove_cgroup(dir: &Path) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match fs::remove_dir(dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) && attempts < 5 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn unavailable(reason: String) -> ContainError {
    ContainError::Unavailable(reason)
}

fn shell(script: &str) -> ProcessSpec {
    ProcessSpec {
        argv: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
        cwd: PathBuf::from("/"),
        env: [(
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        )]
        .into(),
    }
}

fn poll_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
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

fn wait_bounded(child: &mut dyn Child, limit: Duration) -> Option<ExitInfo> {
    let mut exit = None;
    poll_until(limit, || {
        exit = child.try_wait().ok().flatten();
        exit.is_some()
    });
    exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_cgroup_is_the_unified_line() {
        assert_eq!(parse_own_cgroup("0::/\n"), Ok("/"));
        assert_eq!(
            parse_own_cgroup("0::/system.slice/bella.service/supervisor\n"),
            Ok("/system.slice/bella.service/supervisor")
        );
        // Hybrid: v1 controllers listed first, the unified line last.
        assert_eq!(
            parse_own_cgroup("12:memory:/user.slice\n1:name=systemd:/a\n0::/a\n"),
            Ok("/a")
        );
    }

    #[test]
    fn own_cgroup_rejects_v1_removed_and_foreign_paths() {
        assert!(parse_own_cgroup("5:cpu,cpuacct:/\n1:name=systemd:/\n").is_err());
        assert!(parse_own_cgroup("0::/gone (deleted)\n").is_err());
        assert!(parse_own_cgroup("0::/../other\n").is_err());
        assert!(parse_own_cgroup("").is_err());
    }

    #[test]
    fn membership_matches_the_scope_and_its_children_only() {
        assert!(cgroup_is_within("0::/bella-a\n", "/bella-a"));
        assert!(cgroup_is_within("0::/bella-a/inner\n", "/bella-a"));
        assert!(!cgroup_is_within("0::/bella-ab\n", "/bella-a"));
        assert!(!cgroup_is_within("0::/supervisor\n", "/bella-a"));
        assert!(!cgroup_is_within("0::/\n", "/bella-a"));
        assert!(!cgroup_is_within("", "/bella-a"));
    }

    #[test]
    fn scope_names_are_restricted() {
        assert!(validate_name("attempt-01_a").is_ok());
        for bad in ["", "Upper", "a/b", "..", "a b", "a.b"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"a".repeat(MAX_NAME_LEN)).is_ok());
        assert!(validate_name(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn cpu_quota_follows_milli_cpus() {
        assert_eq!(cpu_quota_usec(1000), 100_000);
        assert_eq!(cpu_quota_usec(1500), 150_000);
        assert_eq!(cpu_quota_usec(250), 25_000);
        // The kernel rejects quotas under 1 ms.
        assert_eq!(cpu_quota_usec(1), 1_000);
        assert_eq!(cpu_quota_usec(0), 1_000);
        assert_eq!(cpu_quota_usec(u64::MAX), u64::MAX / 1000);
    }

    #[test]
    fn memory_high_sits_below_max() {
        assert_eq!(memory_high(100 << 20), 95 << 20);
        assert_eq!(memory_high(0), 0);
        assert!(memory_high(u64::MAX) < u64::MAX);
    }

    #[test]
    fn keyed_files_are_parsed_by_exact_key() {
        let stat = "usage_usec 1234\nuser_usec 1000\nnr_throttled 7\nthrottled_usec 99\n";
        assert_eq!(parse_kv(stat, "usage_usec"), Some(1234));
        assert_eq!(parse_kv(stat, "nr_throttled"), Some(7));
        assert_eq!(parse_kv(stat, "throttled"), None);
        assert_eq!(parse_kv("oom 2\noom_kill 3\n", "oom_kill"), Some(3));
        assert_eq!(parse_kv("populated x\n", "populated"), None);
    }
}
