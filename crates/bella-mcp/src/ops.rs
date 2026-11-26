//! The operations the bridge needs from the fleet.
//!
//! The server in this crate knows nothing about HTTP, capture or upload. The
//! `bella` binary supplies an implementation of [`FleetOps`] that does those
//! things; tests supply a fake.

use bella_core::api::{
    ArtifactView, EnvironmentView, ExplainResponse, FleetOverview, JobView, LogPage, LogQuery,
    PlanResponse, SessionInfo, WaitQuery, WaitResponse,
};
use bella_core::job::{CommandSpec, Priority};
use bella_core::platform::PlacementChoice;
use bella_core::resources::ResourceVector;
use bella_core::snapshot::Consistency;
use bella_core::{ApiError, EnvironmentId, JobId, SnapshotId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a job would run, without the parts that only matter once it is
/// submitted. Exactly one of `recipe` and `command` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanArgs {
    /// Canonical directory inside one of the server's allowed roots. The
    /// recipe file is read from here; nothing is uploaded for a plan.
    pub workspace: PathBuf,
    /// Recipe name from the workspace's `builderella.toml`.
    pub recipe: Option<String>,
    /// Ad-hoc command, pinned to the submitting platform unless `placement`
    /// says otherwise.
    pub command: Option<CommandSpec>,
    pub priority: Option<Priority>,
    pub placement: Option<PlacementChoice>,
    pub env: BTreeMap<String, String>,
    /// Requested overrides. A dimension left at zero is not overridden.
    pub resources: Option<ResourceVector>,
}

/// One submission. Exactly one of `recipe` and `command` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitArgs {
    /// Canonical directory inside one of the server's allowed roots. This is
    /// the only directory the implementation may capture and upload.
    pub workspace: PathBuf,
    pub recipe: Option<String>,
    pub command: Option<CommandSpec>,
    pub priority: Option<Priority>,
    pub placement: Option<PlacementChoice>,
    pub env: BTreeMap<String, String>,
    /// Requested overrides. A dimension left at zero is not overridden.
    pub resources: Option<ResourceVector>,
    /// Supplied by the caller or generated once per tool call. The same key
    /// with the same request returns the same job.
    pub idempotency_key: String,
    pub session: SessionInfo,
}

/// What `submit` reports once the job exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitOutcome {
    pub job: JobView,
    pub snapshot_id: SnapshotId,
    /// How coherent the capture was.
    pub consistency: Consistency,
    /// Paths left out of the snapshot because they matched a secret pattern.
    pub excluded_secrets: Vec<String>,
    pub warnings: Vec<String>,
}

/// Blocking fleet operations.
///
/// Every method may be called from several threads at once: each
/// `tools/call` runs on its own thread. Errors use the REST error shape so
/// the bridge can tell a policy denial from an infrastructure failure.
///
/// Nothing here changes machine policy, project policy, cloud budgets or
/// enrollment, and the server never calls [`FleetOps::cancel`] on its own:
/// only an explicit `cancel_job` tool call reaches it.
pub trait FleetOps: Send + Sync {
    /// Dry run: could the fleet run this, and where?
    fn plan(&self, req: PlanArgs) -> Result<PlanResponse, ApiError>;

    /// Capture `req.workspace`, upload it and create the job idempotently.
    /// Returns as soon as the job exists; it does not wait for the build.
    fn submit(&self, req: SubmitArgs) -> Result<SubmitOutcome, ApiError>;

    fn job(&self, id: &JobId) -> Result<JobView, ApiError>;

    /// Long poll for at most `query.timeout_ms`. A timeout is a normal
    /// result with `timed_out` set and must not affect the job.
    fn wait(&self, id: &JobId, query: WaitQuery) -> Result<WaitResponse, ApiError>;

    /// One bounded page of log output, at most `query.limit_bytes` of text.
    fn logs(&self, id: &JobId, query: LogQuery) -> Result<LogPage, ApiError>;

    /// Request cancellation. The returned view shows how far cleanup got.
    fn cancel(&self, id: &JobId) -> Result<JobView, ApiError>;

    fn artifacts(&self, id: &JobId) -> Result<Vec<ArtifactView>, ApiError>;

    /// Download the artifact stored under `path` into the directory `dest`
    /// and return the file that was written, which is `dest` joined with
    /// `path`. The server has already checked that `dest` is inside an
    /// allowed root and that the target does not exist. Implementations
    /// must create the file exclusively and fail with
    /// [`bella_core::ErrorCode::Conflict`] rather than overwrite. When
    /// several attempts produced the same path, the newest attempt's
    /// artifact is the one to fetch.
    fn fetch_artifact(&self, id: &JobId, path: &str, dest: &Path) -> Result<PathBuf, ApiError>;

    fn environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ApiError>;

    /// Renew the lease. The returned view carries the deadline that now
    /// applies; a refusal is `PolicyDenied` or `InvalidState`.
    fn renew_environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ApiError>;

    /// Ask for the environment to be torn down.
    fn finish_environment(&self, id: &EnvironmentId) -> Result<EnvironmentView, ApiError>;

    fn explain(&self, id: &JobId) -> Result<ExplainResponse, ApiError>;

    fn fleet(&self) -> Result<FleetOverview, ApiError>;
}
