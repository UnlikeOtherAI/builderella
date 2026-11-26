// Zod schemas mirroring the wire types in `crates/bella-core/src`. Every
// response is parsed with one of these before anything is rendered, so a
// change in a serialized shape fails with a visible error instead of showing
// wrong data.
//
// Three deliberate leniencies, matching how serde reads the same JSON:
// * unknown object keys are ignored (a newer server may add fields);
// * enum fields accept any string, so a value added by a newer server is
//   shown as-is instead of failing the whole response;
// * a tagged union with an unrecognised `type` becomes `{ type: 'unknown' }`.
// A missing required field, a wrong type or a known variant with the wrong
// shape is always an error.

import { z } from 'zod';

/** A known set of strings that a newer server may extend. */
export type Open<T extends string> = T | (string & {});

function openEnum<const T extends readonly [string, ...string[]]>(known: T) {
  const schema = z.string() as unknown as z.ZodType<Open<T[number]>, Open<T[number]>>;
  return Object.assign(schema, { known });
}

// Rust `u64`. Values above 2^53 lose precision in JavaScript; the only one
// the API sends is `u64::MAX` meaning "no limit" (see `isUnlimited`).
const u64 = z.number().nonnegative();
const u32 = z.number().int().nonnegative();
const i32 = z.number().int();
const millis = u64;

/** Rust `Option<T>`: absent, or `null` from a serializer that writes it. */
const opt = <T extends z.ZodType>(schema: T) => schema.nullish();
/** Rust `Vec<T>` with `#[serde(default)]`. */
const list = <T extends z.ZodType>(schema: T) => z.array(schema).default([]);

const id = (prefix: string) =>
  z.string().regex(new RegExp(`^${prefix}_[0-9a-f]{32}$`), `expected a ${prefix}_ identifier`);

const FleetId = id('flt');
const MachineId = id('mch');
const PrincipalId = id('prn');
const ProjectId = id('prj');
const SessionId = id('ses');
const JobId = id('job');
const AttemptId = id('att');
const EnvironmentId = id('env');
const ArtifactId = id('art');
const Digest = z.string().regex(/^[0-9a-f]{64}$/, 'expected a sha-256 digest');
const SnapshotId = Digest;

/** The fallback arm of a tagged union: an unrecognised `type`, kept by name. */
const unknownVariant = (known: readonly string[]) =>
  z
    .looseObject({ type: z.string() })
    .refine((v) => !known.includes(v.type), { message: 'known variant with an unexpected shape' })
    .transform((v) => ({ type: 'unknown' as const, raw: v.type }));

// --------------------------------------------------------------- resources

const Tokens = z.record(z.string(), u64);

export const ResourceVector = z.looseObject({
  cpu_milli: u64.default(0),
  memory_bytes: u64.default(0),
  disk_bytes: u64.default(0),
  tokens: opt(Tokens),
});
export type ResourceVector = z.infer<typeof ResourceVector>;

export const ResourceEnvelope = z.object({
  initial: ResourceVector,
  peak: ResourceVector,
  maximum: ResourceVector,
});

export const Enforcement = openEnum(['none', 'advisory', 'bounded_guest', 'hard']);
export type Enforcement = z.infer<typeof Enforcement>;

export const EnforcementReport = z.object({
  cpu: Enforcement,
  memory: Enforcement,
  process_tree: Enforcement,
  process_count: Enforcement,
  disk: Enforcement,
  verified: z.boolean(),
  notes: list(z.string()),
});
export type EnforcementReport = z.infer<typeof EnforcementReport>;

// ---------------------------------------------------------------- platform

export const Os = openEnum(['linux', 'windows', 'macos']);
export const Arch = openEnum(['x86_64', 'aarch64']);
export const Platform = z.object({ os: Os, arch: Arch });
export type Platform = z.infer<typeof Platform>;

export const PlatformSelector = z.object({
  os: opt(Os),
  arch: opt(Arch),
  os_version: opt(z.string()),
});
export type PlatformSelector = z.infer<typeof PlatformSelector>;

export const CapabilityState = openEnum([
  'installed',
  'missing',
  'unhealthy',
  'occupied',
  'unauthorized',
  'unverified',
]);

export const Capability = z.object({
  name: z.string(),
  version: opt(z.string()),
  state: CapabilityState,
  attributes: opt(z.record(z.string(), z.string())),
});

export const CapabilityRequirement = z.object({
  name: z.string(),
  version: opt(z.string()),
  attributes: opt(z.record(z.string(), z.string())),
});

export const IsolationLevel = openEnum(['native', 'container', 'vm']);

export const Compatibility = z.object({
  build_hosts: list(PlatformSelector),
  artifact_target: opt(z.string()),
  test_runtime: opt(PlatformSelector),
  requires: list(CapabilityRequirement),
  isolation: IsolationLevel.default('native'),
});
export type Compatibility = z.infer<typeof Compatibility>;

export const PlacementChoice = z.union([
  z.discriminatedUnion('type', [
    z.object({ type: z.literal('any') }),
    z.object({ type: z.literal('prefer'), platform: PlatformSelector }),
    z.object({ type: z.literal('require'), platform: PlatformSelector }),
    z.object({ type: z.literal('machine'), machine: z.string() }),
    z.object({ type: z.literal('pool'), pool: z.string() }),
  ]),
  unknownVariant(['any', 'prefer', 'require', 'machine', 'pool']),
]);
export type PlacementChoice = z.infer<typeof PlacementChoice>;

// -------------------------------------------------------------------- jobs

export const Priority = openEnum(['maintenance', 'background', 'normal', 'interactive', 'urgent']);
export type Priority = z.infer<typeof Priority>;
export const RetryClass = openEnum(['pure', 'isolated_side_effects', 'external_side_effects']);
export const CloudMode = openEnum(['existing_only', 'manual', 'auto_overflow', 'hybrid']);
export type CloudMode = z.infer<typeof CloudMode>;
export const StepKind = openEnum(['task', 'service', 'test', 'cleanup']);

export const CommandSpec = z.union([
  z.discriminatedUnion('type', [
    z.object({ type: z.literal('argv'), argv: z.array(z.string()) }),
    z.object({ type: z.literal('shell'), script: z.string(), shell: opt(z.array(z.string())) }),
  ]),
  unknownVariant(['argv', 'shell']),
]);
export type CommandSpec = z.infer<typeof CommandSpec>;

export const ReadinessProbe = z.union([
  z.discriminatedUnion('type', [
    z.object({ type: z.literal('tcp'), port: u32 }),
    z.object({
      type: z.literal('http'),
      port: u32,
      path: z.string().default(''),
      status: opt(u32),
    }),
    z.object({ type: z.literal('log_line'), contains: z.string() }),
    z.object({ type: z.literal('command'), command: CommandSpec }),
  ]),
  unknownVariant(['tcp', 'http', 'log_line', 'command']),
]);

export const Readiness = z.object({
  probe: ReadinessProbe,
  timeout_secs: u64,
  interval_ms: u64.default(500),
});

export const StepSpec = z.object({
  name: z.string(),
  kind: StepKind.default('task'),
  command: CommandSpec,
  cwd: opt(z.string()),
  env: opt(z.record(z.string(), z.string())),
  depends_on: list(z.string()),
  readiness: opt(Readiness),
  timeout_secs: opt(u64),
  resources: opt(ResourceVector),
  ports: list(u32),
});
export type StepSpec = z.infer<typeof StepSpec>;

export const OutputSpec = z.object({
  artifacts: list(z.string()),
  test_reports: list(z.string()),
  max_log_bytes: u64,
  max_artifact_bytes: u64,
});

export const LeaseSpec = z.object({
  idle_timeout_secs: u64,
  lease_secs: u64,
  max_lifetime_secs: u64,
});
export type LeaseSpec = z.infer<typeof LeaseSpec>;

export const JobSpec = z.object({
  recipe: z.string(),
  recipe_digest: Digest,
  steps: z.array(StepSpec),
  envelope: ResourceEnvelope,
  compatibility: Compatibility.default({
    build_hosts: [],
    requires: [],
    isolation: 'native',
  }),
  placement: PlacementChoice.default({ type: 'any' }),
  priority: Priority.default('normal'),
  retry_class: RetryClass.default('pure'),
  max_attempts: u32,
  timeout_secs: u64,
  cleanup_timeout_secs: u64,
  cloud: CloudMode.default('existing_only'),
  source_lan_only: z.boolean().default(false),
  outputs: opt(OutputSpec),
  environment: opt(LeaseSpec),
  env: opt(z.record(z.string(), z.string())),
  disconnected_continuation_secs: u64.default(0),
  parallelism: opt(u32),
});
export type JobSpec = z.infer<typeof JobSpec>;

export const JobPhase = openEnum([
  'capturing',
  'queued',
  'offered',
  'preparing',
  'executing',
  'collecting',
  'cleaning',
  'finished',
  'quarantined',
]);
export type JobPhase = z.infer<typeof JobPhase>;

export const Outcome = z.union([
  z.discriminatedUnion('type', [
    z.object({ type: z.literal('succeeded') }),
    z.object({ type: z.literal('failed'), step: opt(z.string()), exit_code: opt(i32) }),
    z.object({ type: z.literal('cancelled') }),
    z.object({ type: z.literal('timed_out'), step: opt(z.string()) }),
    z.object({ type: z.literal('resource_exhausted'), detail: z.string() }),
    z.object({ type: z.literal('infra_interrupted'), detail: z.string() }),
    z.object({ type: z.literal('rejected'), detail: z.string() }),
  ]),
  unknownVariant([
    'succeeded',
    'failed',
    'cancelled',
    'timed_out',
    'resource_exhausted',
    'infra_interrupted',
    'rejected',
  ]),
]);
export type Outcome = z.infer<typeof Outcome>;

export const CleanupState = openEnum([
  'not_started',
  'requested',
  'worker_acknowledged',
  'processes_stopped',
  'cleanup_complete',
  'released',
  'quarantined',
]);
export type CleanupState = z.infer<typeof CleanupState>;

export const QueueReasonCode = openEnum([
  'schedulable',
  'waiting_for_capacity',
  'no_compatible_machine',
  'wrong_platform',
  'missing_capability',
  'machine_too_small',
  'foreground_protection',
  'memory_pressure',
  'disk_low',
  'stale_telemetry',
  'machine_unreachable',
  'machine_paused',
  'machine_draining',
  'on_battery',
  'builds_disabled',
  'project_not_allowed',
  'machine_concurrency_limit',
  'project_concurrency_limit',
  'fair_share',
  'reserved_for_older_job',
  'previous_attempt_unresolved',
  'cleanup_pending',
  'quarantined',
  'snapshot_unavailable',
  'source_lan_only',
  'cloud_manual_only',
  'controller_reconciling',
  'offer_rejected',
]);
export type QueueReasonCode = z.infer<typeof QueueReasonCode>;

export const MachineBlocker = z.object({
  machine: z.string(),
  code: QueueReasonCode,
  message: z.string(),
  permanent: z.boolean(),
});
export type MachineBlocker = z.infer<typeof MachineBlocker>;

export const QueueReason = z.object({
  code: QueueReasonCode,
  message: z.string(),
  blockers: list(MachineBlocker),
});
export type QueueReason = z.infer<typeof QueueReason>;

export const StepState = openEnum([
  'pending',
  'running',
  'ready',
  'succeeded',
  'failed',
  'timed_out',
  'skipped',
  'stopped',
]);
export type StepState = z.infer<typeof StepState>;

export const UsageSummary = z.object({
  cpu_time_ms: u64,
  memory_current_bytes: u64,
  memory_peak_bytes: u64,
  processes: u32,
  disk_bytes: u64,
});
export type UsageSummary = z.infer<typeof UsageSummary>;

export const SessionInfo = z.object({
  id: SessionId,
  label: opt(z.string()),
  agent: opt(z.string()),
  origin_machine: opt(z.string()),
  thread_ref: opt(z.string()),
});
export type SessionInfo = z.infer<typeof SessionInfo>;

export const StepView = z.object({
  name: z.string(),
  kind: StepKind,
  state: StepState,
  exit_code: opt(i32),
  started_at: opt(millis),
  finished_at: opt(millis),
});
export type StepView = z.infer<typeof StepView>;

export const AttemptView = z.object({
  id: AttemptId,
  generation: u32,
  machine_id: MachineId,
  machine_name: z.string(),
  phase: JobPhase,
  outcome: opt(Outcome),
  cleanup: CleanupState,
  uncertain: z.boolean(),
  start_committed: z.boolean(),
  reserved: ResourceVector,
  usage: opt(UsageSummary),
  offered_at: millis,
  started_at: opt(millis),
  exited_at: opt(millis),
  released_at: opt(millis),
  steps: list(StepView),
});
export type AttemptView = z.infer<typeof AttemptView>;

export const JobView = z.object({
  id: JobId,
  project_id: ProjectId,
  project_name: z.string(),
  principal_id: PrincipalId,
  session: opt(SessionInfo),
  snapshot_id: SnapshotId,
  recipe: z.string(),
  priority: Priority,
  phase: JobPhase,
  outcome: opt(Outcome),
  cleanup: CleanupState,
  cancel_requested: z.boolean(),
  queue_reason: opt(QueueReason),
  envelope: ResourceEnvelope,
  created_at: millis,
  updated_at: millis,
  attempts: list(AttemptView),
  environment_id: opt(EnvironmentId),
  last_event: u64,
  spec: opt(JobSpec),
});
export type JobView = z.infer<typeof JobView>;

export const LogStream = openEnum(['stdout', 'stderr', 'system']);

export const LogEntry = z.object({
  step: z.string(),
  stream: LogStream,
  text: z.string(),
});
export type LogEntry = z.infer<typeof LogEntry>;

export const LogPage = z.object({
  entries: z.array(LogEntry),
  next_cursor: z.string(),
  truncated: z.boolean(),
  complete: z.boolean(),
});
export type LogPage = z.infer<typeof LogPage>;

export const ArtifactKind = openEnum(['artifact', 'test_report']);

export const ArtifactView = z.object({
  id: ArtifactId,
  job_id: JobId,
  attempt_id: AttemptId,
  snapshot_id: SnapshotId,
  path: z.string(),
  kind: ArtifactKind,
  digest: Digest,
  size: u64,
});
export type ArtifactView = z.infer<typeof ArtifactView>;

// ------------------------------------------------------------ environments

export const Lease = z.object({
  spec: LeaseSpec,
  created_at: millis,
  last_activity_at: millis,
  lease_until: millis,
});
export type Lease = z.infer<typeof Lease>;

export const Endpoint = z.object({
  step: z.string(),
  host: z.string(),
  port: u32,
});

export const EnvironmentState = openEnum(['starting', 'ready', 'finishing', 'ended', 'quarantined']);
export type EnvironmentState = z.infer<typeof EnvironmentState>;

export const EnvironmentView = z.object({
  id: EnvironmentId,
  job_id: JobId,
  attempt_id: AttemptId,
  project_id: ProjectId,
  machine_id: MachineId,
  machine_name: z.string(),
  snapshot_id: SnapshotId,
  state: EnvironmentState,
  endpoints: z.array(Endpoint),
  lease: Lease,
  effective_deadline: millis,
  reserved: ResourceVector,
  ended_reason: opt(z.string()),
});
export type EnvironmentView = z.infer<typeof EnvironmentView>;

// ---------------------------------------------------------------- machines

export const MachineProfile = openEnum([
  'protected_workstation',
  'dedicated_builder',
  'portable_builder',
  'infrastructure_only',
  'ephemeral_cloud',
]);
export type MachineProfile = z.infer<typeof MachineProfile>;

// Loose: the policy is sent back whole on save, so fields this dashboard
// does not know must survive the round trip.
export const MachinePolicy = z.looseObject({
  revision: u64,
  profile: MachineProfile,
  accept_builds: z.boolean(),
  foreground_cpu_percent: u32,
  foreground_memory_percent: u32,
  foreground_memory_min_bytes: u64,
  available_memory_floor_bytes: u64,
  disk_free_floor_percent: u32,
  disk_free_floor_min_bytes: u64,
  max_managed: opt(ResourceVector),
  max_concurrent_attempts: opt(u32),
  low_priority: z.boolean(),
  require_ac_power: z.boolean(),
  min_battery_percent: opt(u32),
  allowed_projects: opt(z.array(ProjectId)),
  tokens: opt(Tokens),
  pools: list(z.string()),
  max_disconnected_secs: u64,
  cleanup_grace_secs: u64,
});
export type MachinePolicy = z.infer<typeof MachinePolicy>;

export const Hardware = z.object({
  logical_cpus: u32,
  physical_cpus: opt(u32),
  cpu_model: z.string().default(''),
  memory_bytes: u64,
  disks: list(
    z.object({
      mount: z.string(),
      total_bytes: u64,
      filesystem: z.string().default(''),
    }),
  ),
  gpus: list(z.string()),
});
export type Hardware = z.infer<typeof Hardware>;

export const Inventory = z.object({
  revision: u64,
  hardware: Hardware,
  os: z.object({
    platform: Platform,
    os_version: z.string(),
    kernel: z.string().default(''),
    hostname: z.string().default(''),
  }),
  capabilities: list(Capability),
  executors: list(
    z.object({
      isolation: IsolationLevel,
      mechanism: z.string(),
      enforcement: EnforcementReport,
    }),
  ),
  worker_version: z.string(),
});

export const PressureLevel = openEnum(['unknown', 'normal', 'warning', 'critical']);
export const ThermalState = openEnum(['unknown', 'nominal', 'fair', 'serious', 'critical']);
export const MeasurementQuality = openEnum(['good', 'degraded', 'unknown']);

export const Telemetry = z.object({
  sampled_at: millis,
  cpu_used_milli: u64,
  cpu_managed_milli: u64,
  memory_total_bytes: u64,
  memory_available_bytes: u64,
  memory_managed_bytes: u64,
  swap_used_bytes: u64,
  memory_pressure: PressureLevel,
  disk_total_bytes: u64,
  disk_free_bytes: u64,
  power: z.object({
    on_ac: opt(z.boolean()),
    battery_percent: opt(u32),
  }),
  thermal: ThermalState,
  quality: MeasurementQuality,
});
export type Telemetry = z.infer<typeof Telemetry>;

export const AvailabilityFlag = openEnum([
  'draining',
  'user_paused',
  'pressure_limited',
  'on_battery',
  'overheated',
  'disk_low',
  'sleeping',
  'stale',
  'unreachable',
  'quarantined',
  'upgrading',
  'builds_disabled',
]);
export type AvailabilityFlag = z.infer<typeof AvailabilityFlag>;

export const MachineLifecycle = openEnum(['enrolling', 'probing', 'ready', 'retired', 'revoked']);

export const LocalControl = z.object({
  paused: z.boolean(),
  draining: z.boolean(),
  boost_until: opt(millis),
});

export const ActiveAttemptView = z.object({
  attempt_id: AttemptId,
  job_id: JobId,
  project_name: z.string(),
  recipe: z.string(),
  phase: JobPhase,
  reserved: ResourceVector,
  uncertain: z.boolean(),
});
export type ActiveAttemptView = z.infer<typeof ActiveAttemptView>;

export const MachineView = z.object({
  id: MachineId,
  name: z.string(),
  lifecycle: MachineLifecycle,
  connected: z.boolean(),
  flags: list(AvailabilityFlag),
  policy: MachinePolicy,
  inventory: Inventory,
  telemetry: opt(Telemetry),
  last_seen_at: opt(millis),
  local_control: LocalControl,
  managed_ceiling: ResourceVector,
  effective_budget: opt(ResourceVector),
  reserved: ResourceVector,
  enforcement: EnforcementReport,
  attempts: list(ActiveAttemptView),
});
export type MachineView = z.infer<typeof MachineView>;

export const FleetOverview = z.object({
  fleet_id: FleetId,
  controller_version: z.string(),
  controller_epoch: u64,
  server_time: millis,
  reconciling: z.boolean(),
  machines: z.array(MachineView),
  queued: u32,
  running: u32,
  environments: u32,
});
export type FleetOverview = z.infer<typeof FleetOverview>;

/** Body of `PATCH /machines/{id}`. Absent fields are left unchanged. */
export interface UpdateMachineRequest {
  name?: string;
  policy?: MachinePolicy;
  drain?: boolean;
  retire?: boolean;
}

// ------------------------------------------------------------------- queue

export const QueueEntry = z.object({
  position: u32,
  waiting_ms: u64,
  job: JobView,
});
export type QueueEntry = z.infer<typeof QueueEntry>;

export const ProjectShare = z.object({
  project_id: ProjectId,
  name: z.string(),
  weight: u32,
  queued: u32,
  running: u32,
  reserved: ResourceVector,
});
export type ProjectShare = z.infer<typeof ProjectShare>;

export const QueueView = z.object({
  entries: z.array(QueueEntry),
  shares: z.array(ProjectShare),
});
export type QueueView = z.infer<typeof QueueView>;

// --------------------------------------------------- identity and projects

export const Role = openEnum(['viewer', 'developer', 'admin']);
export type Role = z.infer<typeof Role>;

export const WhoAmI = z.object({
  fleet_id: FleetId,
  principal_id: PrincipalId,
  name: z.string(),
  role: Role,
});
export type WhoAmI = z.infer<typeof WhoAmI>;

export interface CreateTokenRequest {
  name: string;
  role: Role;
}

export const TokenResponse = z.object({
  principal_id: PrincipalId,
  token: z.string().min(1),
});
export type TokenResponse = z.infer<typeof TokenResponse>;

export interface CreatePairingCodeRequest {
  profile: MachineProfile;
  ttl_secs?: number;
}

export const PairingCodeResponse = z.object({
  code: z.string().min(1),
  expires_at: millis,
  enrollment: z.string().min(1),
});
export type PairingCodeResponse = z.infer<typeof PairingCodeResponse>;

// Loose for the same reason as `MachinePolicy`: it is sent back whole.
export const ProjectPolicy = z.looseObject({
  weight: u32,
  max_concurrent_attempts: opt(u32),
  max_environments: opt(u32),
  cloud: CloudMode.default('existing_only'),
  source_lan_only: z.boolean().default(false),
  allow_urgent: z.boolean().default(false),
});
export type ProjectPolicy = z.infer<typeof ProjectPolicy>;

export const ProjectView = z.object({
  id: ProjectId,
  name: z.string(),
  policy: ProjectPolicy,
  created_at: millis,
});
export type ProjectView = z.infer<typeof ProjectView>;

// ------------------------------------------------------------------ events

export const EntityKind = openEnum([
  'fleet',
  'machine',
  'project',
  'snapshot',
  'job',
  'attempt',
  'environment',
  'cloud',
]);

export const EventRecord = z.object({
  seq: u64,
  at: millis,
  entity: EntityKind,
  entity_id: z.string(),
  kind: z.string(),
  job_id: opt(JobId),
  data: z.unknown().optional(),
});
export type EventRecord = z.infer<typeof EventRecord>;

// ------------------------------------------------------------------ errors

export const ApiErrorBody = z.object({
  code: z.string(),
  message: z.string(),
  details: z.unknown().optional(),
});

// ---------------------------------------------------------------- fixtures

/** The document printed by `cargo run -p bella-core --example api_fixtures`. */
export const Fixtures = z.object({
  whoami: WhoAmI,
  fleet: FleetOverview,
  queue: QueueView,
  jobs: z.array(JobView),
  log_pages: z.array(LogPage),
  artifacts: z.array(ArtifactView),
  environments: z.array(EnvironmentView),
  projects: z.array(ProjectView),
  events: z.array(EventRecord),
  pairing_code: PairingCodeResponse,
  token: TokenResponse,
});
export type Fixtures = z.infer<typeof Fixtures>;
