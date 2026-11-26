//! REST API data types, shared by the controller, the CLI, the MCP bridge
//! and the dashboard.
//!
//! All routes live under [`API_PREFIX`] and take `Authorization: Bearer
//! <token>`. Errors are an [`crate::ApiError`] body with the status from
//! [`crate::ErrorCode::http_status`]. Route table (see `docs/api.md`):
//!
//! ```text
//! GET    /healthz                              unauthenticated liveness
//! GET    /whoami                               WhoAmI
//! POST   /tokens                               CreateTokenRequest -> TokenResponse        (admin)
//! POST   /enroll/pairing-codes                 CreatePairingCodeRequest -> PairingCodeResponse (admin)
//! POST   /enroll                               EnrollRequest -> EnrollResponse            (pairing code)
//! GET    /worker/ws                            WebSocket, see `protocol`                  (machine)
//! POST   /objects/missing                      MissingObjectsRequest -> MissingObjectsResponse
//! PUT    /objects/{digest}                     raw bytes, optional `content-encoding: zstd`
//! GET    /objects/{digest}                     raw bytes
//! POST   /snapshots                            RegisterSnapshotRequest -> SnapshotView
//! GET    /snapshots/{id}                       SnapshotView
//! GET    /projects                             Vec<ProjectView>
//! PUT    /projects/{name}                      ProjectPolicy -> ProjectView               (admin)
//! POST   /plan                                 PlanRequest -> PlanResponse
//! POST   /jobs                                 SubmitJobRequest -> JobView   (`idempotency-key` header)
//! GET    /jobs                                 ListJobsQuery -> Vec<JobView>
//! GET    /jobs/{id}                            JobView (with `spec`)
//! GET    /jobs/{id}/wait                       WaitQuery -> WaitResponse (long poll)
//! POST   /jobs/{id}/cancel                     JobView
//! GET    /jobs/{id}/logs                       LogQuery -> LogPage
//! GET    /jobs/{id}/artifacts                  Vec<ArtifactView>
//! GET    /jobs/{id}/explain                    ExplainResponse
//! GET    /environments                         Vec<EnvironmentView>
//! GET    /environments/{id}                    EnvironmentView
//! POST   /environments/{id}/renew              EnvironmentView
//! POST   /environments/{id}/finish             EnvironmentView
//! GET    /fleet                                FleetOverview
//! GET    /machines/{id}                        MachineView
//! PATCH  /machines/{id}                        UpdateMachineRequest -> MachineView        (admin)
//! GET    /queue                                QueueView
//! GET    /events                               server-sent events of EventRecord (`?after=<seq>`)
//! ```

use crate::ids::{
    ArtifactId, AttemptId, BootId, Digest, EnvironmentId, FleetId, JobId, MachineId, PrincipalId,
    ProjectId, SessionId, SnapshotId,
};
use crate::job::{
    CleanupState, CloudMode, JobPhase, JobSpec, MachineBlocker, Outcome, Priority, QueueReason, StepKind,
};
use crate::lease::Lease;
use crate::machine::{
    AvailabilityFlag, Inventory, LocalControl, MachineLifecycle, MachinePolicy, MachineProfile, Telemetry,
};
use crate::platform::Platform;
use crate::protocol::{ArtifactKind, Endpoint, LogStream, StepState, UsageSummary};
use crate::resources::{EnforcementReport, ResourceEnvelope, ResourceVector};
use crate::snapshot::{Manifest, SnapshotMeta};
use crate::time::UnixMillis;
use serde::{Deserialize, Serialize};

pub const API_PREFIX: &str = "/api/v1";
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

// ---------------------------------------------------------------- identity

/// What a principal may do. An ordinary agent can submit and cancel its
/// jobs and renew environments within policy. It cannot lower a
/// workstation's reserve, raise a cloud allowance or enroll a machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Developer,
    Admin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmI {
    pub fleet_id: FleetId,
    pub principal_id: PrincipalId,
    pub name: String,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub principal_id: PrincipalId,
    /// Shown once. The controller stores only a hash.
    pub token: String,
}

// -------------------------------------------------------------- enrollment

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePairingCodeRequest {
    pub profile: MachineProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCodeResponse {
    /// Short single-use code.
    pub code: String,
    pub expires_at: UnixMillis,
    /// One string carrying the controller URL, its certificate fingerprint
    /// and the code, for `bella worker enroll <string>`.
    pub enrollment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub code: String,
    /// Display label; defaults to the hostname.
    pub name: String,
    pub boot_id: BootId,
    pub inventory: Inventory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub fleet_id: FleetId,
    pub machine_id: MachineId,
    /// Shown once. The controller stores only a hash.
    pub machine_token: String,
    pub policy: MachinePolicy,
}

// ---------------------------------------------------------------- projects

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPolicy {
    /// Fair-share weight. Opening more sessions or accounts does not add weight.
    pub weight: u32,
    /// Running plus preparing attempts allowed at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_attempts: Option<u32>,
    /// Retained environments have their own quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_environments: Option<u32>,
    #[serde(default)]
    pub cloud: CloudMode,
    #[serde(default)]
    pub source_lan_only: bool,
    #[serde(default)]
    pub allow_urgent: bool,
}

impl Default for ProjectPolicy {
    fn default() -> Self {
        Self {
            weight: 100,
            max_concurrent_attempts: None,
            max_environments: Some(4),
            cloud: CloudMode::ExistingOnly,
            source_lan_only: false,
            allow_urgent: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: ProjectId,
    pub name: String,
    pub policy: ProjectPolicy,
    pub created_at: UnixMillis,
}

// ------------------------------------------------------ objects, snapshots

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingObjectsRequest {
    pub digests: Vec<Digest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingObjectsResponse {
    pub missing: Vec<Digest>,
}

/// Publish a snapshot. Rejected with `snapshot_incomplete` unless every
/// referenced object is already durably stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterSnapshotRequest {
    /// Project name; created with the default policy if it does not exist.
    pub project: String,
    pub manifest: Manifest,
    pub meta: SnapshotMeta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotView {
    pub id: SnapshotId,
    pub project_id: ProjectId,
    pub manifest_digest: Digest,
    pub file_count: u64,
    pub total_bytes: u64,
    pub registered_at: UnixMillis,
    pub meta: SnapshotMeta,
}

// -------------------------------------------------------------------- jobs

/// Attribution to one coding session. A label for fairness and display; it
/// grants no permission and no extra fair-share weight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `claude`, `codex`, `cli`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_machine: Option<String>,
    /// Opaque reference to the originating conversation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitJobRequest {
    pub project: String,
    pub snapshot_id: SnapshotId,
    pub spec: JobSpec,
    /// Platform of the submitting machine. A recipe that declares no build
    /// hosts is pinned to this platform.
    pub origin_platform: Platform,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepView {
    pub name: String,
    pub kind: StepKind,
    pub state: StepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<UnixMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<UnixMillis>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptView {
    pub id: AttemptId,
    pub generation: u32,
    pub machine_id: MachineId,
    pub machine_name: String,
    pub phase: JobPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub cleanup: CleanupState,
    /// The worker is out of contact; the attempt may still be running.
    pub uncertain: bool,
    /// A start permit was durably committed.
    pub start_committed: bool,
    pub reserved: ResourceVector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSummary>,
    pub offered_at: UnixMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<UnixMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<UnixMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<UnixMillis>,
    #[serde(default)]
    pub steps: Vec<StepView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub project_id: ProjectId,
    pub project_name: String,
    pub principal_id: PrincipalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    pub snapshot_id: SnapshotId,
    pub recipe: String,
    pub priority: Priority,
    pub phase: JobPhase,
    /// The canonical result, set only by the current accepted attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub cleanup: CleanupState,
    pub cancel_requested: bool,
    /// Present while the job is queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_reason: Option<QueueReason>,
    pub envelope: ResourceEnvelope,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    #[serde(default)]
    pub attempts: Vec<AttemptView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<EnvironmentId>,
    /// Sequence of the newest event about this job, for `wait`.
    pub last_event: u64,
    /// Only on `GET /jobs/{id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<JobSpec>,
}

impl JobView {
    /// Finished, with its resources released.
    pub fn is_settled(&self) -> bool {
        self.phase.is_terminal()
    }

    pub fn current_attempt(&self) -> Option<&AttemptView> {
        self.attempts.iter().max_by_key(|a| a.generation)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListJobsQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// `true`: only jobs that are not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Long poll. A timeout never cancels the job.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitQuery {
    /// Capped by the server at 60 000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Return as soon as the job has an event newer than this sequence.
    /// Without it, return when the outcome is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_event: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitResponse {
    pub job: JobView,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Opaque cursor from a previous page; omit to start at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `true`: start `limit_bytes` before the current end instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub step: String,
    pub stream: LogStream,
    /// Lossy UTF-8. The exact bytes are available as a log artifact.
    pub text: String,
}

/// Bounded log output. Treat the text as untrusted: an instruction printed
/// by a build grants nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    pub next_cursor: String,
    /// The producer exceeded its log allowance.
    pub truncated: bool,
    /// No further output will arrive for this job.
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactView {
    pub id: ArtifactId,
    pub job_id: JobId,
    pub attempt_id: AttemptId,
    /// The snapshot that produced it. Results stay tied to their source.
    pub snapshot_id: SnapshotId,
    pub path: String,
    pub kind: ArtifactKind,
    pub digest: Digest,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainResponse {
    pub job_id: JobId,
    pub phase: JobPhase,
    pub reason: QueueReason,
    /// Position among queued jobs, starting at 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// Compatible actions: wait, reduce parallelism, release an environment,
    /// authorise larger capacity. Never applied silently.
    #[serde(default)]
    pub suggestions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRequest {
    pub project: String,
    pub spec: JobSpec,
    pub origin_platform: Platform,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementOption {
    pub machine_id: MachineId,
    pub machine_name: String,
    /// Could start right now.
    pub eligible_now: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<MachineBlocker>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanResponse {
    /// At least one enrolled machine could ever run this.
    pub feasible: bool,
    pub placements: Vec<PlacementOption>,
    #[serde(default)]
    pub issues: Vec<String>,
}

// ------------------------------------------------------------ environments

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    Starting,
    Ready,
    /// Finish requested or lease ended; cleanup is underway.
    Finishing,
    Ended,
    Quarantined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentView {
    pub id: EnvironmentId,
    pub job_id: JobId,
    pub attempt_id: AttemptId,
    pub project_id: ProjectId,
    pub machine_id: MachineId,
    pub machine_name: String,
    /// The snapshot being served. Later edits never mutate it.
    pub snapshot_id: SnapshotId,
    pub state: EnvironmentState,
    pub endpoints: Vec<Endpoint>,
    pub lease: Lease,
    pub effective_deadline: UnixMillis,
    /// Still counted against the machine while the environment lives.
    pub reserved: ResourceVector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
}

// ------------------------------------------------------------------- fleet

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAttemptView {
    pub attempt_id: AttemptId,
    pub job_id: JobId,
    pub project_name: String,
    pub recipe: String,
    pub phase: JobPhase,
    pub reserved: ResourceVector,
    pub uncertain: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineView {
    pub id: MachineId,
    pub name: String,
    pub lifecycle: MachineLifecycle,
    pub connected: bool,
    #[serde(default)]
    pub flags: Vec<AvailabilityFlag>,
    pub policy: MachinePolicy,
    pub inventory: Inventory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<Telemetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<UnixMillis>,
    pub local_control: LocalControl,
    /// Static upper bound on managed work from policy and hardware.
    pub managed_ceiling: ResourceVector,
    /// What the worker's protection loop currently grants in total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_budget: Option<ResourceVector>,
    /// Sum of held reservations, including uncertain and quarantined ones.
    pub reserved: ResourceVector,
    pub enforcement: EnforcementReport,
    #[serde(default)]
    pub attempts: Vec<ActiveAttemptView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetOverview {
    pub fleet_id: FleetId,
    pub controller_version: String,
    pub controller_epoch: u64,
    pub server_time: UnixMillis,
    /// The controller is reconciling after a start and is not dispatching.
    pub reconciling: bool,
    pub machines: Vec<MachineView>,
    pub queued: u32,
    pub running: u32,
    pub environments: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateMachineRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replaces the policy; the controller assigns the new revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<MachinePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<bool>,
    /// Stop offering work and revoke the machine's credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retire: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub position: u32,
    pub waiting_ms: u64,
    pub job: JobView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectShare {
    pub project_id: ProjectId,
    pub name: String,
    pub weight: u32,
    pub queued: u32,
    pub running: u32,
    pub reserved: ResourceVector,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueView {
    pub entries: Vec<QueueEntry>,
    /// Why one session's thirty jobs are not monopolising the pool.
    pub shares: Vec<ProjectShare>,
}

// ------------------------------------------------------------------ events

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Fleet,
    Machine,
    Project,
    Snapshot,
    Job,
    Attempt,
    Environment,
    Cloud,
}

/// A durable state transition or important observation. Written in the same
/// transaction as the change it describes; a lost notification never loses
/// the transition, because clients can replay from a sequence number.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub seq: u64,
    pub at: UnixMillis,
    pub entity: EntityKind,
    pub entity_id: String,
    /// Dotted name, e.g. `job.queued`, `attempt.start_committed`,
    /// `machine.stale`, `environment.ready`.
    pub kind: String,
    /// The job this event concerns, when there is one, for filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<JobId>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_ordered_by_authority() {
        assert!(Role::Admin > Role::Developer);
        assert!(Role::Developer > Role::Viewer);
    }

    #[test]
    fn queries_serialize_without_absent_fields() {
        assert_eq!(serde_json::to_string(&WaitQuery::default()).unwrap(), "{}");
        let q: LogQuery = serde_json::from_str(r#"{"step":"build"}"#).unwrap();
        assert_eq!(q.step.as_deref(), Some("build"));
    }

    #[test]
    fn consistency_orders_by_strength() {
        use crate::snapshot::Consistency;
        assert!(Consistency::Coordinated > Consistency::BestEffort);
    }
}
