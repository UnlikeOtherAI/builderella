//! Process-group containment for macOS, and the Linux fallback when no
//! cgroup is delegated.
//!
//! Neither system offers an aggregate CPU or memory limit without cgroups,
//! and a process can always leave its process group. This adapter therefore
//! never reports [`Enforcement::Hard`]: it tracks as much of the process
//! tree as the OS lets an unprivileged process see, signals only what it has
//! identified, and says what is left.
//!
//! # What is tracked
//!
//! Every [`Scope::spawn`] starts its child as the leader of a new session
//! and process group, with `BELLA_SCOPE=<scope name>` added to its
//! environment. On each observation (`spawn`, `members`, `usage`,
//! `terminate`, `kill`, `destroy`, and when a child handle sees its process
//! exit) the scope reads the process table and takes as members:
//!
//! 1. every process recorded earlier that is still present with the same
//!    start time;
//! 2. every process in the session or process group of a member. A session
//!    cannot be joined from outside and a group lies within one session, so
//!    a session that contains one member contains only descendants of the
//!    scope;
//! 3. every process whose parent is a member;
//! 4. every process whose environment still carries the scope's
//!    `BELLA_SCOPE` value, where the OS lets that be read, and the children
//!    of such a process. A process found only this way does not vouch for
//!    its session or group: the variable can be copied into a program that
//!    was started elsewhere, and that program's session is not the scope's.
//!
//! A leader that has exited but has not been reaped keeps its PID, and with
//! it the id of its session and group, reserved. Its session therefore stays
//! trusted until the scope has looked at the process table once more after
//! the death. That is what finds a daemon left behind by a shell that exits
//! at once.
//!
//! # What is not tracked
//!
//! A descendant that starts its own session and loses its parent between
//! two observations can only be found by its `BELLA_SCOPE` value:
//!
//! * if it also dropped that variable, it cannot be found on any system;
//! * on macOS with System Integrity Protection, the kernel does not show
//!   the environment of Apple's own binaries (`/bin/sh`, `/usr/bin/perl`,
//!   `/bin/sleep`, ...) to other processes. Such a process is not found
//!   even though it carries the tag. Programs from any other source
//!   (toolchains, Homebrew, the job's own build output) are found.
//!
//! A program started through a system service (`launchd`, `open`) is not a
//! descendant and inherits nothing. On macOS a process owned by another
//! user is not visible at all. This is why `process_tree` is `Advisory`.
//!
//! # Signals and PID reuse
//!
//! A PID or process group id alone is never used to signal. A group is
//! signalled only while its unreaped leader pins the id, or after a member
//! recorded with its start time was re-read with the same start time and
//! group immediately before the call. A process found only by its tag is
//! signalled by PID after the same re-read, never through its group. The check and the signal are two
//! system calls; the remaining window would need the whole PID space to wrap
//! between them.
//!
//! # Limits
//!
//! `low_priority` runs new processes at nice 10 and, on macOS, at the
//! utility disk I/O tier. `PRIO_DARWIN_BG` is deliberately not used: it puts
//! the whole tree on the throttled I/O tier, throttles its sockets and
//! confines it to efficiency cores, which turns ordinary build and test
//! timeouts into failures. `cpu_milli`, `memory_bytes` and `max_processes`
//! are stored and not enforced: `RLIMIT_AS` breaks runtimes that reserve
//! address space, and `RLIMIT_NPROC` counts every process of the user.
//!
//! # Usage
//!
//! Usage is sampled per process and summed. CPU time includes children that
//! a member has waited for; a descendant that starts and ends between two
//! samples under a parent that is gone by the next sample is not counted.
//! Peak memory is the largest sum seen at a sample, so a spike between
//! samples is missed. On Linux memory is resident set size, which counts
//! shared pages once per process.

use crate::{
    Child, ContainError, Containment, ExitInfo, Limits, Membership, ProcessSpec, Scope, Spawned,
};
use bella_core::protocol::{ScopeIdentity, ScopeKind, UsageSummary};
use bella_core::resources::{Enforcement, EnforcementReport};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Environment variable added to every process a scope starts. Its value is
/// the scope name.
pub const SCOPE_ENV: &str = "BELLA_SCOPE";

/// Nice value of processes started with [`Limits::low_priority`].
pub const LOW_PRIORITY_NICE: i32 = 10;

/// How long [`Scope::kill`] keeps re-reading the process table and killing
/// what it finds before it returns.
const KILL_DEADLINE: Duration = Duration::from_secs(2);
const KILL_POLL: Duration = Duration::from_millis(10);

/// PIDs of leaders whose exit status is held by a `std::process::Child` in
/// a live scope of this process. Only that handle may wait for them.
static OWNED_CHILDREN: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

fn owned_children() -> MutexGuard<'static, BTreeSet<i32>> {
    OWNED_CHILDREN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// One process as recorded in [`ScopeIdentity::detail`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: i32,
    pub pgid: i32,
    /// When the process started: microseconds since the Unix epoch on macOS,
    /// clock ticks since boot on Linux. `0` means unknown and never matches
    /// a running process.
    pub start_time: u64,
    /// Found only by its `BELLA_SCOPE` value (or as the child of such a
    /// process), so its session and group are not taken as the scope's.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tag_only: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The JSON document stored in [`ScopeIdentity::detail`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeDetail {
    /// Processes started by `spawn`, each the leader of its own group.
    #[serde(default)]
    pub leaders: Vec<ProcessRecord>,
    /// Every member seen at the last observation, leaders included.
    #[serde(default)]
    pub observed: Vec<ProcessRecord>,
}

/// One row of the process table.
#[derive(Clone, Copy, Debug)]
struct ProcInfo {
    pid: i32,
    ppid: i32,
    pgid: i32,
    /// Session id, or 0 when it cannot be read.
    sid: i32,
    start_time: u64,
    /// Exited and waiting to be reaped. Never counted as alive.
    zombie: bool,
    nice: i32,
    comm: [u8; 16],
}

/// Usage of one process, including children it has waited for.
#[derive(Clone, Copy, Debug, Default)]
struct ProcUsage {
    cpu_ns: u64,
    memory_bytes: u64,
    /// Lifetime peak of this process where the OS records one, else 0.
    memory_peak_bytes: u64,
}

/// The process-group adapter. See the module documentation for its limits.
pub struct ProcessGroups;

impl ProcessGroups {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProcessGroups {
    fn default() -> Self {
        Self::new()
    }
}

impl Containment for ProcessGroups {
    fn kind(&self) -> ScopeKind {
        ScopeKind::UnixProcessGroup
    }

    fn mechanism(&self) -> &'static str {
        "process-group"
    }

    /// Never `Hard`. CPU and memory are `Advisory` (priority and
    /// observation), the process tree is `Advisory` (see the module
    /// documentation for what escapes), process count and disk are `None`.
    fn enforcement(&self) -> EnforcementReport {
        if !sys::SUPPORTED {
            let mut report = EnforcementReport::none();
            report
                .notes
                .push("process inspection is implemented for macOS and Linux only".into());
            return report;
        }
        EnforcementReport {
            cpu: Enforcement::Advisory,
            memory: Enforcement::Advisory,
            process_tree: Enforcement::Advisory,
            process_count: Enforcement::None,
            disk: Enforcement::None,
            verified: false,
            notes: vec![
                format!(
                    "cpu: advisory. No aggregate CPU limit exists without cgroups; cpu_milli is \
                     recorded and not enforced. low_priority starts processes at nice \
                     {LOW_PRIORITY_NICE}{}.",
                    sys::LOW_PRIORITY_EXTRA
                ),
                "memory: advisory. No aggregate memory limit exists without cgroups; memory_bytes \
                 is recorded and not enforced. Usage is sampled per process and summed, so a \
                 spike between samples is missed."
                    .into(),
                format!(
                    "process_tree: advisory. Members are found by session and process group, \
                     parent links and the {SCOPE_ENV} environment value. A descendant that \
                     leaves its session, drops {SCOPE_ENV} and loses its parent between two \
                     observations is not tracked.{}",
                    sys::VISIBILITY_NOTE
                ),
                "process_count: none. max_processes is recorded and not enforced; RLIMIT_NPROC \
                 counts every process of the user, not of the scope."
                    .into(),
                "disk: none. Scratch disk use is neither limited nor measured.".into(),
            ],
        }
    }

    /// Runs real processes: a shell that leaves a background process and
    /// exits, an orphan that leaves its process group, a daemon that starts
    /// its own session (with and without its environment), and a busy
    /// low-priority process. A dimension is downgraded to `None` when
    /// something this adapter claims does not work; the escapes it documents
    /// are confirmed and reported in the notes. Nothing is ever raised above
    /// `Advisory`.
    fn self_test(&self) -> Result<EnforcementReport, ContainError> {
        if !sys::SUPPORTED {
            return Err(unsupported_platform());
        }
        let mut report = self.enforcement();
        report.verified = true;

        let orphan_found = probe_orphan(&mut report.notes)?;
        let escapes_as_documented = probe_escapes(&mut report.notes)?;
        if !(orphan_found && escapes_as_documented) {
            report.process_tree = Enforcement::None;
        }

        let limits = probe_limits(&mut report.notes)?;
        if !(limits.nice_applied && limits.cpu_observed) {
            report.cpu = Enforcement::None;
        }
        if !limits.memory_observed {
            report.memory = Enforcement::None;
        }
        Ok(report)
    }

    fn create(&self, name: &str, limits: &Limits) -> Result<Box<dyn Scope>, ContainError> {
        if !sys::SUPPORTED {
            return Err(unsupported_platform());
        }
        validate_name(name)?;
        Ok(Box::new(PgScope::new(name, limits.clone())))
    }

    /// Rebuilds a scope from a journaled identity. Recorded processes are
    /// kept only when the same PID is alive with the recorded start time;
    /// the process table is then searched for group members, descendants and
    /// `BELLA_SCOPE` carriers. Limits are not part of the identity: call
    /// [`Scope::set_limits`] again after adopting.
    fn adopt(&self, identity: &ScopeIdentity) -> Result<Option<Box<dyn Scope>>, ContainError> {
        if !sys::SUPPORTED {
            return Err(unsupported_platform());
        }
        if identity.kind != ScopeKind::UnixProcessGroup {
            return Err(ContainError::Other(format!(
                "scope `{}` is not a process-group scope",
                identity.name
            )));
        }
        validate_name(&identity.name)?;
        let detail: ScopeDetail = if identity.detail.trim().is_empty() {
            ScopeDetail::default()
        } else {
            serde_json::from_str(&identity.detail).map_err(|err| {
                ContainError::Other(format!(
                    "scope `{}` has an unreadable identity: {err}",
                    identity.name
                ))
            })?
        };

        let scope = PgScope::new(&identity.name, Limits::default());
        let alive = {
            let mut state = scope.lock();
            for rec in detail.leaders.iter().chain(&detail.observed) {
                if rec.pid > 1 && rec.start_time != 0 {
                    state.known.entry(rec.pid).or_insert(Known {
                        rec: *rec,
                        ppid: 0,
                        cpu_ns: 0,
                    });
                }
            }
            state
                .leaders
                .extend(detail.leaders.iter().map(|rec| Leader {
                    rec: *rec,
                    state: LeaderState::Adopted,
                    seen_dead: false,
                }));
            state.refresh()?.count()
        };
        if alive == 0 {
            return Ok(None);
        }
        Ok(Some(Box::new(scope)))
    }
}

fn unsupported_platform() -> ContainError {
    ContainError::Unavailable("process groups are implemented for macOS and Linux only".into())
}

fn validate_name(name: &str) -> Result<(), ContainError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(ContainError::Other(format!(
            "scope name `{name}` must be non-empty and contain only [a-z0-9_-]"
        )))
    }
}

enum LeaderState {
    /// An unreaped child of this worker. While it stays unreaped its PID,
    /// and so its process group id, cannot be given to another process.
    Running(std::process::Child),
    Exited(ExitInfo),
    /// The exit status was collected by something other than this scope.
    Lost,
    /// Recorded by an earlier worker process; not a child of this one.
    Adopted,
}

struct Leader {
    rec: ProcessRecord,
    state: LeaderState,
    /// An earlier process-table read found the process dead, so every
    /// process it ever started was visible to a later read.
    seen_dead: bool,
}

impl Leader {
    fn running(&self) -> bool {
        matches!(self.state, LeaderState::Running(_))
    }
}

struct Known {
    rec: ProcessRecord,
    ppid: i32,
    /// Largest CPU time sampled for this process.
    cpu_ns: u64,
}

struct TagSeen {
    start_time: u64,
    comm: [u8; 16],
    tagged: bool,
}

/// Result of one read of the process table.
struct Census {
    alive: Vec<ProcessRecord>,
    /// Trusted process groups that have a live member, with that member.
    groups: BTreeMap<i32, ProcessRecord>,
    /// Running leaders the process table does not show (another user's
    /// process after a setuid exec). Counted as alive.
    hidden_leaders: u32,
    memory_bytes: u64,
    scanned: usize,
}

impl Census {
    fn count(&self) -> u32 {
        u32::try_from(self.alive.len())
            .unwrap_or(u32::MAX)
            .saturating_add(self.hidden_leaders)
    }
}

struct State {
    name: String,
    /// `BELLA_SCOPE=<name>` as it appears in a process environment.
    tag: Vec<u8>,
    limits: Limits,
    leaders: Vec<Leader>,
    /// Members seen at the last read, by PID.
    known: BTreeMap<i32, Known>,
    /// CPU time of members that are gone and whose time was not passed on
    /// to a parent that is still a member.
    retired_cpu_ns: u64,
    /// Largest CPU total reported so far; the reported value never drops.
    cpu_ms: u64,
    peak_memory_bytes: u64,
    /// Processes already checked for the tag, so that a read only inspects
    /// the environment of processes that are new or have exec'd.
    tag_cache: HashMap<i32, TagSeen>,
    env_buf: Vec<u8>,
}

/// Not a member.
const OUTSIDE: u8 = 0;
/// A member found by the scope tag, or the child of one.
const BY_TAG: u8 = 1;
/// A member known to be in a session the scope created.
const IN_SESSION: u8 = 2;

/// Breadth-first search over one process table: a member brings in its
/// children and, unless it was found only by the tag, its session and its
/// process group.
struct Walk<'a> {
    procs: &'a [ProcInfo],
    by_parent: &'a HashMap<i32, Vec<usize>>,
    by_group: &'a HashMap<i32, Vec<usize>>,
    by_session: &'a HashMap<i32, Vec<usize>>,
    own_pid: i32,
    own_group: i32,
    own_session: i32,
    /// Per process: [`OUTSIDE`], [`BY_TAG`] or [`IN_SESSION`].
    level: Vec<u8>,
    trusted: BTreeSet<i32>,
    trusted_sessions: BTreeSet<i32>,
    queue: Vec<usize>,
}

impl Walk<'_> {
    /// PID 0, PID 1 and this process are never members.
    fn add(&mut self, index: usize, level: u8) {
        let pid = self.procs[index].pid;
        if self.level[index] >= level || pid <= 1 || pid == self.own_pid {
            return;
        }
        self.level[index] = level;
        self.queue.push(index);
    }

    fn is_member(&self, index: usize) -> bool {
        self.level[index] != OUTSIDE
    }

    /// Group 0, group 1 and this process's own group are never trusted.
    fn trust(&mut self, pgid: i32) {
        if pgid <= 1 || pgid == self.own_group || !self.trusted.insert(pgid) {
            return;
        }
        let by_group = self.by_group;
        if let Some(indices) = by_group.get(&pgid) {
            for &index in indices {
                self.add(index, IN_SESSION);
            }
        }
    }

    /// Session 0, session 1 and this process's own session are never
    /// trusted.
    fn trust_session(&mut self, sid: i32) {
        if sid <= 1 || sid == self.own_session || !self.trusted_sessions.insert(sid) {
            return;
        }
        let by_session = self.by_session;
        if let Some(indices) = by_session.get(&sid) {
            for &index in indices {
                self.add(index, IN_SESSION);
            }
        }
    }

    fn run(&mut self) {
        while let Some(index) = self.queue.pop() {
            let proc = self.procs[index];
            let level = self.level[index];
            if level == IN_SESSION {
                self.trust_session(proc.sid);
                self.trust(proc.pgid);
            }
            if proc.zombie {
                continue;
            }
            let by_parent = self.by_parent;
            if let Some(children) = by_parent.get(&proc.pid) {
                for &child in children {
                    // A child cannot be older than its parent; an older
                    // process with this parent PID belongs to a previous
                    // owner of the PID.
                    if self.procs[child].start_time >= proc.start_time {
                        self.add(child, level);
                    }
                }
            }
        }
    }
}

impl State {
    fn new(name: &str, limits: Limits) -> Self {
        Self {
            name: name.to_owned(),
            tag: format!("{SCOPE_ENV}={name}").into_bytes(),
            limits,
            leaders: Vec::new(),
            known: BTreeMap::new(),
            retired_cpu_ns: 0,
            cpu_ms: 0,
            peak_memory_bytes: 0,
            tag_cache: HashMap::new(),
            env_buf: Vec::new(),
        }
    }

    /// Read the process table, recompute membership, sample usage and reap
    /// leaders that an earlier read already found dead.
    fn refresh(&mut self) -> io::Result<Census> {
        let procs = sys::list()?;
        let own_pid = i32::try_from(std::process::id()).unwrap_or(0);
        // SAFETY: getpgrp takes no arguments and cannot fail.
        let own_group = unsafe { libc::getpgrp() };
        // SAFETY: getsid(0) reads the session of the calling process.
        let own_session = unsafe { libc::getsid(0) };

        let mut by_pid: HashMap<i32, usize> = HashMap::with_capacity(procs.len());
        let mut by_parent: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut by_group: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut by_session: HashMap<i32, Vec<usize>> = HashMap::new();
        for (index, proc) in procs.iter().enumerate() {
            by_pid.insert(proc.pid, index);
            by_parent.entry(proc.ppid).or_default().push(index);
            by_group.entry(proc.pgid).or_default().push(index);
            by_session.entry(proc.sid).or_default().push(index);
        }

        // Decided before this read: these leaders were dead when the
        // previous one was taken, so this one has seen all they left behind.
        let ready_to_reap: Vec<usize> = self
            .leaders
            .iter()
            .enumerate()
            .filter(|(_, leader)| leader.running() && leader.seen_dead)
            .map(|(index, _)| index)
            .collect();

        let mut walk = Walk {
            procs: &procs,
            by_parent: &by_parent,
            by_group: &by_group,
            by_session: &by_session,
            own_pid,
            own_group,
            own_session,
            level: vec![OUTSIDE; procs.len()],
            trusted: BTreeSet::new(),
            trusted_sessions: BTreeSet::new(),
            queue: Vec::new(),
        };
        for leader in &self.leaders {
            if leader.running() {
                // The leader called setsid, so both ids equal its PID, which
                // stays reserved until the leader is reaped.
                walk.trust_session(leader.rec.pgid);
                walk.trust(leader.rec.pgid);
            }
        }
        for known in self.known.values() {
            if let Some(&index) = by_pid.get(&known.rec.pid)
                && procs[index].start_time == known.rec.start_time
            {
                walk.add(
                    index,
                    if known.rec.tag_only {
                        BY_TAG
                    } else {
                        IN_SESSION
                    },
                );
            }
        }
        walk.run();

        for (index, proc) in procs.iter().enumerate() {
            if walk.is_member(index) || proc.zombie || proc.pid <= 1 || proc.pid == own_pid {
                continue;
            }
            let cached = self
                .tag_cache
                .get(&proc.pid)
                .filter(|seen| seen.start_time == proc.start_time && seen.comm == proc.comm)
                .map(|seen| seen.tagged);
            let tagged = match cached {
                Some(tagged) => tagged,
                // An unreadable environment is not cached: the process may
                // be in the middle of an exec.
                None => match sys::has_tag(proc.pid, &self.tag, &mut self.env_buf) {
                    Some(tagged) => {
                        self.tag_cache.insert(
                            proc.pid,
                            TagSeen {
                                start_time: proc.start_time,
                                comm: proc.comm,
                                tagged,
                            },
                        );
                        tagged
                    }
                    None => false,
                },
            };
            if tagged {
                walk.add(index, BY_TAG);
            }
        }
        walk.run();
        self.tag_cache.retain(|pid, _| by_pid.contains_key(pid));

        let mut census = Census {
            alive: Vec::new(),
            groups: BTreeMap::new(),
            hidden_leaders: 0,
            memory_bytes: 0,
            scanned: procs.len(),
        };
        let mut next: BTreeMap<i32, Known> = BTreeMap::new();
        let mut lifetime_peak = 0u64;
        for (index, proc) in procs.iter().enumerate() {
            if !walk.is_member(index) {
                continue;
            }
            let rec = ProcessRecord {
                pid: proc.pid,
                pgid: proc.pgid,
                start_time: proc.start_time,
                tag_only: walk.level[index] == BY_TAG,
            };
            let mut cpu_ns = self
                .known
                .get(&proc.pid)
                .filter(|known| known.rec.start_time == proc.start_time)
                .map_or(0, |known| known.cpu_ns);
            if !proc.zombie {
                if let Some(usage) = sys::usage(proc.pid) {
                    cpu_ns = cpu_ns.max(usage.cpu_ns);
                    census.memory_bytes = census.memory_bytes.saturating_add(usage.memory_bytes);
                    lifetime_peak = lifetime_peak.max(usage.memory_peak_bytes);
                }
                census.alive.push(rec);
                if walk.trusted.contains(&proc.pgid) {
                    census.groups.entry(proc.pgid).or_insert(rec);
                }
            }
            next.insert(
                proc.pid,
                Known {
                    rec,
                    ppid: proc.ppid,
                    cpu_ns,
                },
            );
        }

        // A member that disappeared while its parent is still a member was
        // waited for by that parent, whose own figure now includes it.
        // Anything else keeps its last sample.
        for gone in self.known.values() {
            let still_here = next
                .get(&gone.rec.pid)
                .is_some_and(|now| now.rec.start_time == gone.rec.start_time);
            if !still_here && !passed_to_parent(gone, &self.known, &next) {
                self.retired_cpu_ns = self.retired_cpu_ns.saturating_add(gone.cpu_ns);
            }
        }
        self.known = next;
        let live_cpu_ns = self
            .known
            .values()
            .fold(0u64, |sum, known| sum.saturating_add(known.cpu_ns));
        let total_ms = self.retired_cpu_ns.saturating_add(live_cpu_ns) / 1_000_000;
        self.cpu_ms = self.cpu_ms.max(total_ms);
        // One process's lifetime peak is a lower bound for the scope's.
        self.peak_memory_bytes = self
            .peak_memory_bytes
            .max(census.memory_bytes)
            .max(lifetime_peak);

        for index in ready_to_reap {
            // An error marks the leader lost, which ends its group's trust.
            let _ = self.reap(index);
        }
        for leader in &mut self.leaders {
            if !leader.running() {
                continue;
            }
            // An unreaped child keeps its PID, so this row is the leader.
            let row = by_pid
                .get(&leader.rec.pid)
                .map(|&index| procs[index])
                .filter(|proc| !proc.zombie);
            match row {
                Some(proc) => {
                    if leader.rec.start_time == 0 {
                        leader.rec.start_time = proc.start_time;
                    }
                }
                None if sys::child_exited(leader.rec.pid) => leader.seen_dead = true,
                None => census.hidden_leaders += 1,
            }
        }

        // Orphans are re-parented to this process when it is PID 1 of a
        // container or a subreaper, and a leader outlives a scope object
        // that was dropped. Reap the members among them that no handle
        // owns; never wait for "any child", which would take exit statuses
        // that belong to someone else.
        for (index, proc) in procs.iter().enumerate() {
            let own_zombie = walk.is_member(index) && proc.zombie && proc.ppid == own_pid;
            if own_zombie && !owned_children().contains(&proc.pid) {
                let mut status = 0;
                // SAFETY: `status` is a valid out-pointer and WNOHANG makes
                // the call return at once. The PID is a zombie child of this
                // process whose status no `std::process::Child` holds.
                unsafe { libc::waitpid(proc.pid, &mut status, libc::WNOHANG) };
            }
        }

        Ok(census)
    }

    /// Collect a leader's exit status if it has exited.
    fn reap(&mut self, index: usize) -> io::Result<Option<ExitInfo>> {
        let leader = &mut self.leaders[index];
        let LeaderState::Running(child) = &mut leader.state else {
            return leader_exit(&leader.state);
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let exit = ExitInfo {
                    code: status.code(),
                    signal: status.signal(),
                };
                leader.state = LeaderState::Exited(exit);
                owned_children().remove(&leader.rec.pid);
                Ok(Some(exit))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                leader.state = LeaderState::Lost;
                owned_children().remove(&leader.rec.pid);
                Err(err)
            }
        }
    }

    /// Non-blocking wait for a child handle. Before the exit status is
    /// collected, and the leader's group id released with it, the process
    /// table is read once more so that anything the leader left in its group
    /// is recorded.
    fn poll_leader(&mut self, index: usize) -> io::Result<Option<ExitInfo>> {
        let leader = &self.leaders[index];
        if !leader.running() {
            return leader_exit(&leader.state);
        }
        if !sys::child_exited(leader.rec.pid) {
            return Ok(None);
        }
        // Without the read the status is still collected; the group is then
        // only found through parent links and the tag.
        let _ = self.refresh();
        self.reap(index)
    }

    /// Send `signal` to every member. See the module documentation for why
    /// each target is safe to signal.
    fn signal(&self, census: &Census, signal: libc::c_int) {
        let mut pinned = BTreeSet::new();
        for leader in &self.leaders {
            if leader.running() && leader.rec.pgid > 1 && pinned.insert(leader.rec.pgid) {
                // SAFETY: killpg only sends a signal. The id is the PID of
                // an unreaped child of this process, which the kernel cannot
                // reuse, so the group is the one this scope created.
                unsafe { libc::killpg(leader.rec.pgid, signal) };
            }
        }
        for (&pgid, witness) in &census.groups {
            if !pinned.contains(&pgid) && same_process(witness) {
                // SAFETY: killpg only sends a signal. A recorded member was
                // just re-read with the same start time and group, so the
                // group still belongs to this scope's session.
                unsafe { libc::killpg(pgid, signal) };
            }
        }
        for rec in &census.alive {
            let in_signalled_group =
                pinned.contains(&rec.pgid) || census.groups.contains_key(&rec.pgid);
            if !in_signalled_group && same_process(rec) {
                // SAFETY: kill only sends a signal. The PID was just re-read
                // with the recorded start time, is greater than 1 and is not
                // this process.
                unsafe { libc::kill(rec.pid, signal) };
            }
        }
    }

    fn detail(&self) -> ScopeDetail {
        let leaders = self
            .leaders
            .iter()
            .filter(|leader| {
                leader.running()
                    || self
                        .known
                        .get(&leader.rec.pid)
                        .is_some_and(|known| known.rec.start_time == leader.rec.start_time)
            })
            .map(|leader| leader.rec)
            .collect();
        ScopeDetail {
            leaders,
            observed: self.known.values().map(|known| known.rec).collect(),
        }
    }
}

impl Drop for State {
    /// The handles of leaders that are still running go away with the
    /// scope; release them so that a scope adopted later may reap them.
    fn drop(&mut self) {
        let mut owned = owned_children();
        for leader in &self.leaders {
            if leader.running() {
                owned.remove(&leader.rec.pid);
            }
        }
    }
}

fn leader_exit(state: &LeaderState) -> io::Result<Option<ExitInfo>> {
    match state {
        LeaderState::Exited(exit) => Ok(Some(*exit)),
        LeaderState::Running(_) => Ok(None),
        LeaderState::Lost => Err(io::Error::other(
            "the exit status was collected outside the scope",
        )),
        LeaderState::Adopted => Err(io::Error::other(
            "the process is not a child of this worker",
        )),
    }
}

/// Is the PID still the recorded process, in the recorded group?
fn same_process(rec: &ProcessRecord) -> bool {
    sys::info(rec.pid)
        .is_some_and(|now| !now.zombie && now.start_time == rec.start_time && now.pgid == rec.pgid)
}

/// Did a member that is still present wait for `gone`? Follows parents that
/// disappeared in the same interval.
fn passed_to_parent(
    gone: &Known,
    before: &BTreeMap<i32, Known>,
    now: &BTreeMap<i32, Known>,
) -> bool {
    let mut parent = gone.ppid;
    for _ in 0..64 {
        let previous = before.get(&parent);
        if let Some(current) = now.get(&parent) {
            return previous.is_some_and(|p| p.rec.start_time == current.rec.start_time);
        }
        match previous {
            Some(p) => parent = p.ppid,
            None => return false,
        }
    }
    false
}

/// A scope made of process groups. Dropping it stops nothing: the processes
/// keep running and can be re-adopted from [`Scope::identity`].
struct PgScope {
    shared: Arc<Mutex<State>>,
}

impl PgScope {
    fn new(name: &str, limits: Limits) -> Self {
        Self {
            shared: Arc::new(Mutex::new(State::new(name, limits))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.shared)
    }
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Scope for PgScope {
    /// `detail` is a [`ScopeDetail`] as JSON. It reflects the last
    /// observation; journal it again after each `spawn`.
    fn identity(&self) -> ScopeIdentity {
        let state = self.lock();
        ScopeIdentity {
            kind: ScopeKind::UnixProcessGroup,
            name: state.name.clone(),
            detail: serde_json::to_string(&state.detail()).unwrap_or_default(),
        }
    }

    /// The child becomes a session and process group leader before it runs
    /// any of its own code. Its environment is `spec.env` plus
    /// [`SCOPE_ENV`], which replaces a variable of that name in the spec.
    /// File descriptors of the worker that are not close-on-exec are
    /// inherited; the Rust standard library opens all of its own that way.
    fn spawn(&mut self, spec: &ProcessSpec) -> Result<Spawned, ContainError> {
        let Some((program, args)) = spec.argv.split_first() else {
            return Err(ContainError::Spawn {
                program: String::new(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "argv is empty"),
            });
        };
        let mut state = self.lock();
        let low_priority = state.limits.low_priority;

        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .env(SCOPE_ENV, &state.name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: the closure runs in the forked child before exec. It only
        // makes direct system calls (setsid, getpriority, setpriority and
        // the macOS I/O policy call), allocates nothing and takes no lock.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if low_priority {
                    enter_low_priority();
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|source| ContainError::Spawn {
            program: program.clone(),
            source,
        })?;

        let pid = i32::try_from(child.id()).unwrap_or(0);
        owned_children().insert(pid);
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            // SAFETY: killpg only sends a signal. `pid` is this process's
            // unreaped child, which made itself a group leader before exec.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            let _ = child.wait();
            owned_children().remove(&pid);
            return Err(ContainError::Other(
                "the new process has no output pipes".into(),
            ));
        };

        // A process that has already exited has no readable start time on
        // macOS. Its record then never matches a PID; while it is unreaped
        // its group is still found through the leader itself.
        let info = sys::info(pid);
        let rec = ProcessRecord {
            pid,
            pgid: pid,
            start_time: info.map_or(0, |info| info.start_time),
            tag_only: false,
        };
        if let Some(info) = info {
            state.known.insert(
                pid,
                Known {
                    rec,
                    ppid: info.ppid,
                    cpu_ns: 0,
                },
            );
        }
        state.leaders.push(Leader {
            rec,
            state: LeaderState::Running(child),
            seen_dead: false,
        });
        let index = state.leaders.len() - 1;
        // Best effort: membership is recomputed on every later call.
        let _ = state.refresh();

        Ok(Spawned {
            child: Box::new(PgChild {
                pid: child_pid(pid),
                index,
                shared: Arc::clone(&self.shared),
            }),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
        })
    }

    /// `cpu_time_ms` never decreases. `disk_bytes` is always 0.
    fn usage(&self) -> Result<UsageSummary, ContainError> {
        let mut state = self.lock();
        let census = state.refresh()?;
        Ok(UsageSummary {
            cpu_time_ms: state.cpu_ms,
            memory_current_bytes: census.memory_bytes,
            memory_peak_bytes: state.peak_memory_bytes,
            processes: census.count(),
            disk_bytes: 0,
        })
    }

    /// `Empty` only when no live process is found in a trusted group, among
    /// the descendants of a member or carrying the scope's tag. `Unknown`
    /// when the process table could not be read.
    fn members(&self) -> Result<Membership, ContainError> {
        let mut state = self.lock();
        Ok(match state.refresh() {
            Ok(census) => match census.count() {
                0 => Membership::Empty,
                alive => Membership::Alive(alive),
            },
            Err(_) => Membership::Unknown,
        })
    }

    /// Stores the limits. A change of `low_priority` is applied to running
    /// members as a nice value; returning running processes to normal
    /// priority needs privilege and fails with `Unsupported` without it.
    /// `cpu_milli`, `memory_bytes` and `max_processes` are accepted and not
    /// enforced.
    fn set_limits(&mut self, limits: &Limits) -> Result<(), ContainError> {
        let mut state = self.lock();
        let was_low = state.limits.low_priority;
        state.limits = limits.clone();
        if was_low == limits.low_priority {
            return Ok(());
        }
        let census = state.refresh()?;
        // SAFETY: getpriority only reads the nice value of this process.
        let own_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS as _, 0) };
        let target = if limits.low_priority {
            own_nice.max(LOW_PRIORITY_NICE)
        } else {
            own_nice
        };
        let mut refused = false;
        for rec in &census.alive {
            let Some(now) = sys::info(rec.pid).filter(|now| now.start_time == rec.start_time)
            else {
                continue;
            };
            if limits.low_priority && now.nice >= target {
                continue;
            }
            // SAFETY: setpriority only changes a scheduling value. The PID
            // was just re-read with the recorded start time.
            let rc = unsafe {
                libc::setpriority(libc::PRIO_PROCESS as _, rec.pid.unsigned_abs(), target)
            };
            if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                refused = true;
            }
        }
        if refused && !limits.low_priority {
            return Err(ContainError::Unsupported(
                "restoring normal priority of running processes",
            ));
        }
        Ok(())
    }

    /// SIGTERM, then SIGCONT so that stopped processes can act on it.
    fn terminate(&mut self) -> Result<(), ContainError> {
        let mut state = self.lock();
        let census = state.refresh()?;
        state.signal(&census, libc::SIGTERM);
        state.signal(&census, libc::SIGCONT);
        Ok(())
    }

    /// SIGKILL, repeated with a fresh read of the process table until
    /// nothing is alive or about two seconds have passed. Check `members`
    /// afterwards: a process this user may not signal stays alive.
    fn kill(&mut self) -> Result<(), ContainError> {
        let mut state = self.lock();
        let deadline = Instant::now() + KILL_DEADLINE;
        loop {
            let census = state.refresh()?;
            if census.count() == 0 {
                return Ok(());
            }
            state.signal(&census, libc::SIGKILL);
            if Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(KILL_POLL);
        }
    }

    fn destroy(self: Box<Self>) -> Result<String, ContainError> {
        let mut state = self.lock();
        let name = state.name.clone();
        let unverifiable = |err: io::Error| {
            ContainError::Other(format!("cannot verify that scope `{name}` is empty: {err}"))
        };
        // Check every environment again rather than trusting earlier reads.
        state.tag_cache.clear();
        let mut census = state.refresh().map_err(unverifiable)?;
        // A leader found dead by one read is reaped by the next.
        for _ in 0..3 {
            if census.count() > 0 || !state.leaders.iter().any(Leader::running) {
                break;
            }
            census = state.refresh().map_err(unverifiable)?;
        }
        if census.count() > 0 || state.leaders.iter().any(Leader::running) {
            return Err(ContainError::NotEmpty(name));
        }
        Ok(format!(
            "no process in tracked groups, descendant walk or tag scan ({} process table entries checked)",
            census.scanned
        ))
    }
}

fn child_pid(pid: i32) -> u32 {
    u32::try_from(pid).unwrap_or(0)
}

/// Lower the calling process's priority. Only direct system calls: this
/// runs between fork and exec.
fn enter_low_priority() {
    // SAFETY: getpriority and setpriority on the calling process have no
    // memory-safety preconditions. A failure leaves the priority unchanged.
    unsafe {
        if libc::getpriority(libc::PRIO_PROCESS as _, 0) < LOW_PRIORITY_NICE {
            libc::setpriority(libc::PRIO_PROCESS as _, 0, LOW_PRIORITY_NICE);
        }
    }
    sys::lower_io_priority();
}

/// Handle to a spawned leader. The exit status is collected at most once,
/// by this handle or by the scope, and kept for this handle either way.
struct PgChild {
    pid: u32,
    index: usize,
    shared: Arc<Mutex<State>>,
}

impl Child for PgChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitInfo>> {
        lock(&self.shared).poll_leader(self.index)
    }

    fn wait(&mut self) -> io::Result<ExitInfo> {
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            block_until_exit(self.pid);
        }
    }
}

/// Block until the child can be waited for, without collecting its status
/// and without holding the scope lock.
fn block_until_exit(pid: u32) {
    // SAFETY: siginfo_t is a plain C structure for which all-zero is valid.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid out-pointer. WNOWAIT leaves the child
    // waitable, so the `std::process::Child` still owns its exit status.
    let rc = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
    if rc != 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        // The status was collected by the scope in the meantime, or the OS
        // refused the call; the caller polls again.
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------
// Commissioning probes
// ---------------------------------------------------------------------------

static PROBE_SEQ: AtomicU32 = AtomicU32::new(0);

/// A self-test scope that is killed when it goes out of scope.
struct Probe {
    scope: PgScope,
}

impl Probe {
    fn new(limits: Limits) -> Self {
        let name = format!(
            "selftest-{}-{}",
            std::process::id(),
            PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            scope: PgScope::new(&name, limits),
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.scope.kill();
        // One more read reaps leaders the kill left as zombies.
        let _ = self.scope.members();
    }
}

fn probe_spec(argv: &[&str]) -> ProcessSpec {
    ProcessSpec {
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: PathBuf::from("/"),
        env: BTreeMap::from([(
            "PATH".to_owned(),
            "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin".to_owned(),
        )]),
    }
}

/// A shell starts a background process and exits. The process stays in the
/// shell's group and must be found and killed there.
fn probe_orphan(notes: &mut Vec<String>) -> Result<bool, ContainError> {
    let mut probe = Probe::new(Limits::default());
    let mut spawned = probe
        .scope
        .spawn(&probe_spec(&["/bin/sh", "-c", "sleep 30 & exit 0"]))?;
    spawned.child.wait()?;
    let before = probe.scope.members()?;
    probe.scope.kill()?;
    let after = probe.scope.members()?;
    let ok = matches!(before, Membership::Alive(_)) && after == Membership::Empty;
    notes.push(if ok {
        "self-test: a background process left by a shell that exited was found in its process \
         group and killed."
            .into()
    } else {
        format!(
            "self-test FAILED: a background process left by an exited shell was not contained \
             (before kill: {before:?}, after: {after:?})."
        )
    });
    Ok(ok)
}

/// Leaves the leader's process group but not its session, and is orphaned.
const ESCAPE_GROUP: &str =
    r#"fork and exit; setpgrp(0, 0); print "PID $$\n"; close STDOUT; sleep 30"#;
/// Starts its own session and is orphaned. The environment is kept.
const ESCAPE_SESSION: &str = r#"use POSIX; fork and exit; POSIX::setsid(); my $p = fork; if ($p) { print "PID $p\n"; exit } sleep 30"#;
/// The same with the environment replaced. The pause lets the scope finish
/// the read it makes at spawn, so that the escape happens entirely between
/// two observations.
const ESCAPE_SESSION_NO_TAG: &str = r#"use POSIX; select(undef, undef, undef, 0.3); fork and exit; POSIX::setsid(); my $p = fork; if ($p) { print "PID $p\n"; exit } %ENV = (); exec "/bin/sleep", "30""#;

struct Escape {
    /// The escapee really left the leader's process group.
    left: bool,
    /// The scope reported it as a member and it was gone after `kill`.
    caught: bool,
    /// The environment of the probe's own process could be read.
    tag_readable: bool,
}

/// Run a perl script that leaves a process behind and prints its PID, then
/// see whether the scope finds and kills that process. `None` when the
/// probe could not run. Whatever is left is killed by PID at the end.
fn probe_escape(
    script: &str,
    wait_for_exec: bool,
    notes: &mut Vec<String>,
) -> Result<Option<Escape>, ContainError> {
    let mut probe = Probe::new(Limits::default());
    let mut spawned = match probe.scope.spawn(&probe_spec(&["perl", "-e", script])) {
        Ok(spawned) => spawned,
        Err(ContainError::Spawn { .. }) => {
            notes.push(
                "self-test: an escape probe was not run because perl is not installed.".into(),
            );
            return Ok(None);
        }
        Err(other) => return Err(other),
    };
    let leader = i32::try_from(spawned.child.pid()).unwrap_or(0);
    let tag_readable = {
        let mut state = probe.scope.lock();
        let State { tag, env_buf, .. } = &mut *state;
        sys::has_tag(leader, tag, env_buf) == Some(true)
    };

    let mut line = String::new();
    BufReader::new(&mut spawned.stdout).read_line(&mut line)?;
    let escapee = line
        .trim()
        .strip_prefix("PID ")
        .and_then(|pid| pid.parse::<i32>().ok())
        .filter(|pid| *pid > 1);
    let Some((escapee, first)) = escapee.and_then(|pid| Some((pid, sys::info(pid)?))) else {
        notes.push("self-test: an escape probe did not start.".into());
        return Ok(None);
    };
    if wait_for_exec {
        // Wait until the escapee has replaced its environment and its
        // parent is gone, without observing the scope in the meantime.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !sys::info(escapee)
                .is_some_and(|now| now.comm.starts_with(b"sleep") && now.ppid != first.ppid)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    spawned.child.wait()?;

    let left = sys::info(escapee).is_some_and(|now| now.pgid != leader);
    let before = probe.scope.members()?;
    probe.scope.kill()?;
    let still_running =
        || sys::info(escapee).is_some_and(|now| !now.zombie && now.start_time == first.start_time);
    let caught = matches!(before, Membership::Alive(_)) && !still_running();
    if still_running() {
        // SAFETY: kill only sends a signal. The PID was printed by this
        // probe's own process and was just re-read with the start time
        // recorded when it was first seen.
        unsafe { libc::kill(escapee, libc::SIGKILL) };
    }
    Ok(Some(Escape {
        left,
        caught,
        tag_readable,
    }))
}

/// The three escape probes. Returns whether tree tracking did everything
/// this adapter claims; the gaps it documents are reported as notes.
fn probe_escapes(notes: &mut Vec<String>) -> Result<bool, ContainError> {
    let mut ok = true;
    if let Some(group) = probe_escape(ESCAPE_GROUP, false, notes)?
        && group.left
    {
        ok &= group.caught;
        notes.push(if group.caught {
            "self-test: an orphan that left its process group but not its session was found \
             and killed."
                .into()
        } else {
            "self-test FAILED: an orphan that left its process group but not its session was \
             not found."
                .into()
        });
    }

    let Some(session) = probe_escape(ESCAPE_SESSION, false, notes)?.filter(|probe| probe.left)
    else {
        return Ok(ok);
    };
    match (session.caught, session.tag_readable) {
        (true, _) => notes.push(format!(
            "self-test: a daemon that started its own session and was orphaned was found by \
             its {SCOPE_ENV} value and killed."
        )),
        (false, true) => {
            ok = false;
            notes.push(format!(
                "self-test FAILED: a daemon that started its own session and kept {SCOPE_ENV} \
                 was not found."
            ));
        }
        (false, false) => notes.push(format!(
            "self-test: confirmed that a system program which starts its own session and is \
             orphaned between two observations is NOT tracked here: this system does not let \
             the environment of its own binaries be read, so {SCOPE_ENV} cannot identify \
             them. Programs from other sources are identified by it."
        )),
    }
    if session.tag_readable
        && let Some(bare) =
            probe_escape(ESCAPE_SESSION_NO_TAG, true, notes)?.filter(|probe| probe.left)
    {
        notes.push(if bare.caught {
            format!(
                "self-test: a daemon that cleared {SCOPE_ENV} was still caught in this run; \
                 that depends on timing and is not guaranteed."
            )
        } else {
            format!(
                "self-test: confirmed that a daemon which starts its own session, clears \
                 {SCOPE_ENV} and is orphaned between two observations is NOT tracked."
            )
        });
    }
    Ok(ok)
}

struct LimitProbe {
    nice_applied: bool,
    cpu_observed: bool,
    memory_observed: bool,
}

/// A busy process started with `low_priority`: its nice value must be
/// lowered and its CPU time and memory must show up in `usage`.
fn probe_limits(notes: &mut Vec<String>) -> Result<LimitProbe, ContainError> {
    let mut probe = Probe::new(Limits {
        low_priority: true,
        ..Limits::default()
    });
    let spawned = probe
        .scope
        .spawn(&probe_spec(&["/bin/sh", "-c", "while :; do :; done"]))?;
    let pid = i32::try_from(spawned.child.pid()).unwrap_or(0);
    let nice_applied = sys::info(pid).is_some_and(|info| info.nice >= LOW_PRIORITY_NICE);

    let mut usage = probe.scope.usage()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while (usage.cpu_time_ms == 0 || usage.memory_current_bytes == 0) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        usage = probe.scope.usage()?;
    }
    let result = LimitProbe {
        nice_applied,
        cpu_observed: usage.cpu_time_ms > 0,
        memory_observed: usage.memory_current_bytes > 0,
    };
    notes.push(if result.nice_applied {
        format!("self-test: low_priority started a process at nice {LOW_PRIORITY_NICE}.")
    } else {
        "self-test FAILED: low_priority did not lower the nice value.".into()
    });
    notes.push(if result.cpu_observed && result.memory_observed {
        "self-test: CPU time and memory of a busy process were measured; neither is limited.".into()
    } else {
        "self-test FAILED: usage of a busy process could not be measured.".into()
    });
    Ok(result)
}

// ---------------------------------------------------------------------------
// Operating system access
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod sys {
    use super::{ProcInfo, ProcUsage};
    use std::io;
    use std::sync::OnceLock;

    pub const SUPPORTED: bool = true;
    pub const LOW_PRIORITY_EXTRA: &str = " and at the utility disk I/O tier";
    pub const VISIBILITY_NOTE: &str = " With System Integrity Protection the environment of \
        Apple's own binaries is not readable, so the tag only finds programs from other \
        sources. A process owned by another user is not visible.";

    const IOPOL_TYPE_DISK: libc::c_int = 0;
    const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
    const IOPOL_UTILITY: libc::c_int = 4;
    const SZOMB: u32 = 5;

    unsafe extern "C" {
        /// `<sys/resource.h>`; not exposed by the `libc` crate.
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
    }

    /// Every process this user may inspect. Processes of other users and
    /// zombies are not returned by the kernel.
    pub fn list() -> io::Result<Vec<ProcInfo>> {
        // SAFETY: a null buffer of size 0 only asks for the number of PIDs.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let mut capacity = usize::try_from(count).unwrap_or(0) + 256;
        loop {
            let mut pids = vec![0 as libc::pid_t; capacity];
            let bytes = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice()))
                .map_err(|_| io::Error::other("process table is too large"))?;
            // SAFETY: the buffer is valid for `bytes` bytes of pid_t.
            let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            if filled <= 0 {
                return Err(io::Error::last_os_error());
            }
            let filled = usize::try_from(filled).unwrap_or(0);
            if filled >= capacity {
                // The table grew past the buffer; the list may be cut short.
                capacity *= 2;
                continue;
            }
            pids.truncate(filled);
            return Ok(pids
                .into_iter()
                .filter(|pid| *pid > 0)
                .filter_map(info)
                .collect());
        }
    }

    pub fn info(pid: i32) -> Option<ProcInfo> {
        // SAFETY: proc_bsdinfo is a plain C structure of integers and
        // character arrays, for which all-zero is valid.
        let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: the buffer is a valid proc_bsdinfo of `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size)
        };
        if written != size {
            return None;
        }
        Some(ProcInfo {
            pid: i32::try_from(bsd.pbi_pid).ok()?,
            ppid: i32::try_from(bsd.pbi_ppid).ok()?,
            pgid: i32::try_from(bsd.pbi_pgid).ok()?,
            // SAFETY: getsid only reads the session id of a PID.
            sid: unsafe { libc::getsid(pid) }.max(0),
            start_time: bsd
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(bsd.pbi_start_tvusec),
            zombie: bsd.pbi_status == SZOMB,
            nice: bsd.pbi_nice,
            comm: bsd.pbi_comm.map(|byte| byte.to_ne_bytes()[0]),
        })
    }

    fn arg_max() -> usize {
        static ARG_MAX: OnceLock<usize> = OnceLock::new();
        *ARG_MAX.get_or_init(|| {
            let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
            let mut value: libc::c_int = 0;
            let mut len = size_of::<libc::c_int>();
            // SAFETY: `mib` names an integer sysctl and `value`/`len`
            // describe a buffer of exactly that size.
            let rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    2,
                    (&raw mut value).cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            match usize::try_from(value) {
                Ok(max) if rc == 0 && max > 0 => max,
                _ => 1 << 20,
            }
        })
    }

    /// Does the environment the process was started with contain `tag`?
    /// `None` when it cannot be read.
    pub fn has_tag(pid: i32, tag: &[u8], buf: &mut Vec<u8>) -> Option<bool> {
        buf.resize(arg_max(), 0);
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut len = buf.len();
        // SAFETY: `buf` is valid for `len` bytes and the kernel writes at
        // most that many, updating `len` to what it wrote.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        Some(procargs_env_contains(buf.get(..len)?, tag))
    }

    /// `KERN_PROCARGS2` layout: argc, executable path, NUL padding, argc
    /// argument strings, then environment strings up to an empty string.
    pub(super) fn procargs_env_contains(data: &[u8], tag: &[u8]) -> bool {
        let Some((argc, mut rest)) = data.split_first_chunk::<4>() else {
            return false;
        };
        let argc = i32::from_ne_bytes(*argc).max(0);
        let Some(end) = rest.iter().position(|byte| *byte == 0) else {
            return false;
        };
        rest = &rest[end..];
        let padding = rest
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(rest.len());
        rest = &rest[padding..];
        for _ in 0..argc {
            let Some(end) = rest.iter().position(|byte| *byte == 0) else {
                return false;
            };
            rest = &rest[end + 1..];
        }
        rest.split(|byte| *byte == 0)
            .take_while(|entry| !entry.is_empty())
            .any(|entry| entry == tag)
    }

    /// Mach time units to nanoseconds, as a fraction. `libc` marks its Mach
    /// bindings deprecated in favour of a separate crate; one call does not
    /// justify the dependency.
    #[allow(deprecated)]
    fn timebase() -> (u64, u64) {
        static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
        *TIMEBASE.get_or_init(|| {
            let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
            // SAFETY: `info` is a valid out-pointer for the call.
            let rc = unsafe { libc::mach_timebase_info(&mut info) };
            if rc == 0 && info.denom != 0 {
                (u64::from(info.numer), u64::from(info.denom))
            } else {
                (1, 1)
            }
        })
    }

    /// CPU time of the process and of the children it has waited for, and
    /// its physical footprint. The kernel reports the times in Mach time
    /// units.
    pub fn usage(pid: i32) -> Option<ProcUsage> {
        // SAFETY: rusage_info_v4 is a plain C structure of integers, for
        // which all-zero is valid.
        let mut usage: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer is a valid rusage_info_v4, the structure the
        // V4 flavor fills.
        let rc =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut usage).cast()) };
        if rc != 0 {
            return None;
        }
        let units = usage
            .ri_user_time
            .saturating_add(usage.ri_system_time)
            .saturating_add(usage.ri_child_user_time)
            .saturating_add(usage.ri_child_system_time);
        let (numer, denom) = timebase();
        let cpu_ns = u128::from(units) * u128::from(numer) / u128::from(denom);
        Some(ProcUsage {
            cpu_ns: u64::try_from(cpu_ns).unwrap_or(u64::MAX),
            memory_bytes: usage.ri_phys_footprint,
            memory_peak_bytes: usage.ri_lifetime_max_phys_footprint,
        })
    }

    /// Has this process's own child exited? Does not collect its status.
    pub fn child_exited(pid: i32) -> bool {
        // SAFETY: siginfo_t is a plain C structure for which all-zero is
        // valid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid out-pointer; WNOHANG returns at once and
        // WNOWAIT leaves the child waitable.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.unsigned_abs(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            // No such child: its status was collected elsewhere.
            return io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
        }
        // SAFETY: waitid succeeded, so the structure holds a child report or
        // is still all-zero; `si_pid` is valid to read in both cases.
        unsafe { info.si_pid() == pid }
    }

    /// Move the calling process to the utility disk I/O tier, which its
    /// children inherit. A direct system call, usable between fork and exec.
    pub fn lower_io_priority() {
        // SAFETY: the call takes three integers and changes only the I/O
        // policy of the calling process.
        unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_UTILITY) };
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use super::{ProcInfo, ProcUsage};
    use std::io::{self, Read};

    pub const SUPPORTED: bool = true;
    pub const LOW_PRIORITY_EXTRA: &str = "";
    pub const VISIBILITY_NOTE: &str = "";

    struct Stat {
        info: ProcInfo,
        /// utime + stime + cutime + cstime, in clock ticks.
        cpu_ticks: u64,
    }

    /// Parse `/proc/<pid>/stat`. The command name may contain spaces and
    /// parentheses, so fields are counted from the last `)`.
    fn parse_stat(text: &str) -> Option<Stat> {
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        let pid = text.get(..open)?.trim().parse().ok()?;
        let mut comm = [0u8; 16];
        for (slot, byte) in comm.iter_mut().zip(text.get(open + 1..close)?.bytes()) {
            *slot = byte;
        }
        let mut fields = text.get(close + 1..)?.split_ascii_whitespace();
        let state = fields.next()?; // field 3
        let ppid = fields.next()?.parse().ok()?; // 4
        let pgid = fields.next()?.parse().ok()?; // 5
        let sid = fields.next()?.parse().ok()?; // 6
        let utime: u64 = fields.nth(7)?.parse().ok()?; // 14
        let stime: u64 = fields.next()?.parse().ok()?; // 15
        let cutime: i64 = fields.next()?.parse().ok()?; // 16
        let cstime: i64 = fields.next()?.parse().ok()?; // 17
        let nice = fields.nth(1)?.parse().ok()?; // 19
        let start_time = fields.nth(2)?.parse().ok()?; // 22
        Some(Stat {
            info: ProcInfo {
                pid,
                ppid,
                pgid,
                sid,
                start_time,
                zombie: matches!(state, "Z" | "X" | "x"),
                nice,
                comm,
            },
            cpu_ticks: utime
                .saturating_add(stime)
                .saturating_add(u64::try_from(cutime).unwrap_or(0))
                .saturating_add(u64::try_from(cstime).unwrap_or(0)),
        })
    }

    fn read_stat(pid: i32) -> Option<Stat> {
        let bytes = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(&String::from_utf8_lossy(&bytes))
    }

    /// Every process in this PID namespace, zombies included.
    pub fn list() -> io::Result<Vec<ProcInfo>> {
        let mut procs = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if let Some(stat) = read_stat(pid) {
                procs.push(stat.info);
            }
        }
        if procs.is_empty() {
            return Err(io::Error::other("/proc lists no processes"));
        }
        Ok(procs)
    }

    pub fn info(pid: i32) -> Option<ProcInfo> {
        read_stat(pid).map(|stat| stat.info)
    }

    /// Does the environment the process was started with contain `tag`?
    /// `None` when it cannot be read.
    pub fn has_tag(pid: i32, tag: &[u8], buf: &mut Vec<u8>) -> Option<bool> {
        buf.clear();
        std::fs::File::open(format!("/proc/{pid}/environ"))
            .ok()?
            .read_to_end(buf)
            .ok()?;
        Some(buf.split(|byte| *byte == 0).any(|entry| entry == tag))
    }

    fn sysconf(name: libc::c_int) -> Option<u64> {
        // SAFETY: sysconf takes an integer and has no other preconditions.
        let value = unsafe { libc::sysconf(name) };
        u64::try_from(value).ok().filter(|value| *value > 0)
    }

    /// CPU time of the process and of the children it has waited for, and
    /// its resident set size.
    pub fn usage(pid: i32) -> Option<ProcUsage> {
        let stat = read_stat(pid)?;
        let ticks_per_second = sysconf(libc::_SC_CLK_TCK).unwrap_or(100);
        let cpu_ns = u128::from(stat.cpu_ticks) * 1_000_000_000 / u128::from(ticks_per_second);
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident_pages: u64 = statm.split_ascii_whitespace().nth(1)?.parse().ok()?;
        let page_size = sysconf(libc::_SC_PAGESIZE).unwrap_or(4096);
        Some(ProcUsage {
            cpu_ns: u64::try_from(cpu_ns).unwrap_or(u64::MAX),
            memory_bytes: resident_pages.saturating_mul(page_size),
            memory_peak_bytes: 0,
        })
    }

    /// Has this process's own child exited? An unreaped child keeps its
    /// PID, so its `/proc` entry cannot belong to another process.
    pub fn child_exited(pid: i32) -> bool {
        read_stat(pid).is_none_or(|stat| stat.info.zombie)
    }

    /// Nothing beyond the nice value, which the I/O schedulers that honour
    /// priorities derive their class from.
    pub fn lower_io_priority() {}

    #[cfg(test)]
    mod tests {
        use super::parse_stat;

        #[test]
        fn stat_fields_are_counted_from_the_last_parenthesis() {
            let line = "4242 (a b) c)) S 17 4000 3999 0 -1 4194304 100 0 0 0 7 5 3 2 30 10 1 0 \
                        987654 1000000 250 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0";
            let stat = parse_stat(line).unwrap();
            assert_eq!(stat.info.pid, 4242);
            assert_eq!(stat.info.ppid, 17);
            assert_eq!(stat.info.pgid, 4000);
            assert_eq!(stat.info.sid, 3999);
            assert_eq!(stat.info.nice, 10);
            assert_eq!(stat.info.start_time, 987_654);
            assert!(!stat.info.zombie);
            assert_eq!(&stat.info.comm[..7], b"a b) c)");
            assert_eq!(stat.cpu_ticks, 7 + 5 + 3 + 2);
        }

        #[test]
        fn zombie_state_is_recognised() {
            let line = "9 (sh) Z 1 9 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0";
            assert!(parse_stat(line).unwrap().info.zombie);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod sys {
    use super::{ProcInfo, ProcUsage};
    use std::io;

    pub const SUPPORTED: bool = false;
    pub const LOW_PRIORITY_EXTRA: &str = "";
    pub const VISIBILITY_NOTE: &str = "";

    pub fn list() -> io::Result<Vec<ProcInfo>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the process table cannot be read on this operating system",
        ))
    }

    pub fn info(_pid: i32) -> Option<ProcInfo> {
        None
    }

    pub fn has_tag(_pid: i32, _tag: &[u8], _buf: &mut Vec<u8>) -> Option<bool> {
        None
    }

    pub fn usage(_pid: i32) -> Option<ProcUsage> {
        None
    }

    pub fn child_exited(_pid: i32) -> bool {
        false
    }

    pub fn lower_io_priority() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_round_trips_and_tolerates_missing_lists() {
        let detail = ScopeDetail {
            leaders: vec![ProcessRecord {
                pid: 10,
                pgid: 10,
                start_time: 77,
                tag_only: false,
            }],
            observed: vec![ProcessRecord {
                pid: 11,
                pgid: 10,
                start_time: 78,
                tag_only: true,
            }],
        };
        let json = serde_json::to_string(&detail).unwrap();
        assert_eq!(
            json,
            r#"{"leaders":[{"pid":10,"pgid":10,"start_time":77}],"observed":[{"pid":11,"pgid":10,"start_time":78,"tag_only":true}]}"#
        );
        assert_eq!(serde_json::from_str::<ScopeDetail>(&json).unwrap(), detail);
        assert_eq!(
            serde_json::from_str::<ScopeDetail>("{}").unwrap(),
            ScopeDetail::default()
        );
    }

    #[test]
    fn scope_names_are_restricted() {
        assert!(validate_name("attempt-01_a").is_ok());
        for bad in ["", "Upper", "a b", "a=b", "a/b"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    fn known(pid: i32, ppid: i32) -> (i32, Known) {
        let rec = ProcessRecord {
            pid,
            pgid: 1000,
            start_time: u64::from(pid.unsigned_abs()),
            tag_only: false,
        };
        (
            pid,
            Known {
                rec,
                ppid,
                cpu_ns: 1,
            },
        )
    }

    #[test]
    fn cpu_of_a_reaped_child_is_left_to_its_surviving_ancestor() {
        // 100 -> 101 -> 102; 101 and 102 are gone, 100 remains.
        let before = BTreeMap::from([known(100, 1), known(101, 100), known(102, 101)]);
        let now = BTreeMap::from([known(100, 1)]);
        assert!(passed_to_parent(&before[&102], &before, &now));
        assert!(passed_to_parent(&before[&101], &before, &now));
        // The whole chain is gone: nobody is left to account for it.
        let empty = BTreeMap::new();
        assert!(!passed_to_parent(&before[&102], &before, &empty));
        // The parent PID now belongs to a different process.
        let mut reused = BTreeMap::from([known(100, 1)]);
        reused.get_mut(&100).unwrap().rec.start_time = 999;
        assert!(!passed_to_parent(&before[&101], &before, &reused));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn procargs_environment_is_found_after_the_arguments() {
        let mut data = 2i32.to_ne_bytes().to_vec();
        data.extend_from_slice(b"/bin/sh\0\0\0\0sh\0BELLA_SCOPE=arg\0HOME=/x\0BELLA_SCOPE=s1\0\0");
        data.extend_from_slice(b"BELLA_SCOPE=apple\0");
        assert!(sys::procargs_env_contains(&data, b"BELLA_SCOPE=s1"));
        // An argument is not an environment entry.
        assert!(!sys::procargs_env_contains(&data, b"BELLA_SCOPE=arg"));
        // Strings after the empty terminator are not the environment.
        assert!(!sys::procargs_env_contains(&data, b"BELLA_SCOPE=apple"));
        assert!(!sys::procargs_env_contains(&data, b"BELLA_SCOPE=s"));
        assert!(!sys::procargs_env_contains(b"\0\0", b"BELLA_SCOPE=s1"));
    }
}
