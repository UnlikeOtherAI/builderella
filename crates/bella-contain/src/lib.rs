//! Process containment adapters.
//!
//! One [`Scope`] owns every process of one attempt: build steps, services,
//! test runners and all their descendants. Limits apply to the scope as a
//! whole, and cleanup is only complete once the scope is verified empty.
//!
//! Adapters advertise only what they verified on this machine:
//!
//! | Adapter | Platform | CPU / memory | Process tree |
//! | --- | --- | --- | --- |
//! | [`linux`] cgroup v2 | Linux with a delegated cgroup | hard | hard |
//! | [`windows`] Job Object | Windows | hard | hard |
//! | [`pgroup`] process groups | macOS, Linux fallback | advisory | advisory |
//!
//! Resource containment is not a security sandbox. A job that can reach a
//! privileged container socket or create a service can escape any of these.

use bella_core::protocol::{ScopeIdentity, ScopeKind, UsageSummary};
use bella_core::resources::EnforcementReport;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(unix)]
pub mod pgroup;
#[cfg(windows)]
pub mod windows;

#[derive(Debug, thiserror::Error)]
pub enum ContainError {
    #[error("containment is unavailable: {0}")]
    Unavailable(String),
    #[error("failed to start `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("scope `{0}` still has live processes")]
    NotEmpty(String),
    #[error("{0} is not supported by this adapter")]
    Unsupported(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Limits for a whole scope. `None` leaves a dimension unlimited.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub cpu_milli: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub max_processes: Option<u32>,
    /// Run at background priority where the OS supports it.
    pub low_priority: bool,
}

/// One process to start inside a scope. Nothing is inherited from the
/// worker: the environment is exactly `env`, and stdin is closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    /// Program and arguments. The worker resolves shell commands to an
    /// explicit interpreter before calling the adapter.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    /// Unix signal that ended the process, if any.
    pub signal: Option<i32>,
}

impl ExitInfo {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Handle to one process started with [`Scope::spawn`].
pub trait Child: Send {
    fn pid(&self) -> u32;
    fn try_wait(&mut self) -> std::io::Result<Option<ExitInfo>>;
    fn wait(&mut self) -> std::io::Result<ExitInfo>;
}

pub struct Spawned {
    pub child: Box<dyn Child>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
}

/// Limit violations observed in a scope so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitHits {
    /// Times the memory limit stopped or killed a member.
    pub memory: u64,
    /// Times the process limit refused a new process.
    pub processes: u64,
}

/// Who is still alive in a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    /// Verified: no process belongs to the scope.
    Empty,
    Alive(u32),
    /// The adapter cannot tell. Treated as not empty.
    Unknown,
}

pub trait Scope: Send {
    /// Journal this before the first spawn so a restarted worker can
    /// re-adopt the scope. Its `detail` changes as processes are added.
    fn identity(&self) -> ScopeIdentity;

    /// Start a process inside the scope. The process is a member before it
    /// executes any of its own code, so no descendant can escape by racing.
    fn spawn(&mut self, spec: &ProcessSpec) -> Result<Spawned, ContainError>;

    /// Aggregate usage of every member, including exited ones where the OS
    /// accounts for them.
    fn usage(&self) -> Result<UsageSummary, ContainError>;

    fn members(&self) -> Result<Membership, ContainError>;

    /// Limit violations so far, so a job killed by its memory limit is
    /// reported as resource exhaustion and not as an ordinary failure.
    /// `Ok(None)` when the adapter cannot observe them. Read it before
    /// [`Scope::destroy`].
    fn limit_hits(&self) -> Result<Option<LimitHits>, ContainError> {
        Ok(None)
    }

    /// Change limits on a live scope. `Unsupported` when the adapter cannot.
    fn set_limits(&mut self, limits: &Limits) -> Result<(), ContainError>;

    /// Ask every member to stop (SIGTERM, or the closest Windows equivalent).
    fn terminate(&mut self) -> Result<(), ContainError>;

    /// Forcibly stop every member, including descendants.
    fn kill(&mut self) -> Result<(), ContainError>;

    /// Remove the scope. Fails with [`ContainError::NotEmpty`] while any
    /// member is alive. Returns how emptiness was established.
    fn destroy(self: Box<Self>) -> Result<String, ContainError>;
}

pub trait Containment: Send + Sync {
    fn kind(&self) -> ScopeKind;

    /// Short mechanism name: `cgroup-v2`, `job-object`, `process-group`.
    fn mechanism(&self) -> &'static str;

    /// What this adapter can enforce, as detected. `verified` is false.
    fn enforcement(&self) -> EnforcementReport;

    /// Commissioning check: create a scope, start a process that spawns
    /// descendants, exercise each limit, kill the tree and verify the scope
    /// is empty. Returns the report with `verified = true` and each
    /// dimension downgraded to what actually worked.
    fn self_test(&self) -> Result<EnforcementReport, ContainError>;

    /// Create an empty scope. `name` is unique per attempt and contains only
    /// `[a-z0-9_-]`.
    fn create(&self, name: &str, limits: &Limits) -> Result<Box<dyn Scope>, ContainError>;

    /// Re-attach to a scope journaled by an earlier worker process on the
    /// same boot. `Ok(None)` means the scope no longer exists.
    fn adopt(&self, identity: &ScopeIdentity) -> Result<Option<Box<dyn Scope>>, ContainError>;
}

/// The strongest adapter usable on this machine right now.
pub fn native() -> Result<Box<dyn Containment>, ContainError> {
    #[cfg(target_os = "linux")]
    {
        match linux::CgroupV2::detect() {
            Ok(adapter) => return Ok(Box::new(adapter)),
            Err(ContainError::Unavailable(_)) => {}
            Err(other) => return Err(other),
        }
    }
    #[cfg(unix)]
    {
        Ok(Box::new(pgroup::ProcessGroups::new()))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows::JobObjects::new()?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(ContainError::Unavailable(
            "unsupported operating system".into(),
        ))
    }
}

/// Terminate, wait up to `grace`, then kill, then wait up to `kill_wait`
/// for the scope to become empty.
pub fn stop_scope(
    scope: &mut dyn Scope,
    grace: Duration,
    kill_wait: Duration,
) -> Result<Membership, ContainError> {
    if scope.members()? == Membership::Empty {
        return Ok(Membership::Empty);
    }
    scope.terminate()?;
    if wait_empty(scope, grace)? == Membership::Empty {
        return Ok(Membership::Empty);
    }
    scope.kill()?;
    wait_empty(scope, kill_wait)
}

fn wait_empty(scope: &dyn Scope, limit: Duration) -> Result<Membership, ContainError> {
    let start = std::time::Instant::now();
    loop {
        let membership = scope.members()?;
        if membership == Membership::Empty || start.elapsed() >= limit {
            return Ok(membership);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
