// Readable names for wire values. Every lookup falls back to the raw value,
// so something a newer server sends is shown as-is rather than hidden.

import type { MachineView, PlatformSelector } from '../api/schemas';
import { formatBytes, formatDuration } from './format';
import { diskFloorBytes } from './policy';

export type Tone = 'ok' | 'info' | 'warn' | 'bad' | 'neutral';

function lookup(table: Record<string, string>, value: string): string {
  return Object.hasOwn(table, value) ? (table[value] as string) : value;
}

const PHASES: Record<string, string> = {
  capturing: 'Capturing',
  queued: 'Queued',
  offered: 'Offered',
  preparing: 'Preparing',
  executing: 'Executing',
  collecting: 'Collecting',
  cleaning: 'Cleaning up',
  finished: 'Finished',
  quarantined: 'Quarantined',
};
export const phaseLabel = (phase: string) => lookup(PHASES, phase);

const PHASE_DETAIL: Record<string, string> = {
  capturing: 'The snapshot is still being uploaded.',
  queued: 'Waiting for a machine.',
  offered: 'Offered to a machine. Nothing runs until the start is committed.',
  preparing: 'The machine is materializing the snapshot.',
  executing: 'Steps are running.',
  collecting: 'Steps finished; artifacts are being uploaded.',
  cleaning: 'Processes are being stopped and the workspace removed.',
  finished: 'Nothing is running.',
  quarantined: 'Cleanup could not be verified.',
};
export const phaseDetail = (phase: string) => lookup(PHASE_DETAIL, phase);

/** Phases in which an attempt owns a reservation (`JobPhase::holds_resources`). */
export const HOLDING_PHASES: ReadonlySet<string> = new Set([
  'offered',
  'preparing',
  'executing',
  'collecting',
  'cleaning',
  'quarantined',
]);

const PRIORITIES: Record<string, string> = {
  maintenance: 'Maintenance',
  background: 'Background',
  normal: 'Normal',
  interactive: 'Interactive',
  urgent: 'Urgent',
};
export const priorityLabel = (priority: string) => lookup(PRIORITIES, priority);
/** Highest first, for sorting and for the filter menu. */
export const PRIORITY_ORDER = ['urgent', 'interactive', 'normal', 'background', 'maintenance'];

const PROFILES: Record<string, string> = {
  protected_workstation: 'Protected workstation',
  dedicated_builder: 'Dedicated builder',
  portable_builder: 'Portable builder',
  infrastructure_only: 'Infrastructure only',
  ephemeral_cloud: 'Ephemeral cloud worker',
};
export const profileLabel = (profile: string) => lookup(PROFILES, profile);

const PROFILE_DETAIL: Record<string, string> = {
  protected_workstation:
    'Someone works on this machine. Keeps a foreground reserve, runs builds at low priority, one heavy job at a time.',
  dedicated_builder: 'Built for sustained load within thermal and disk limits.',
  portable_builder: 'Builder rules plus power, battery and sleep restrictions.',
  infrastructure_only: 'Runs the controller or services. Builds are not admitted.',
  ephemeral_cloud: 'Created by the fleet with a fixed expiry.',
};
export const profileDetail = (profile: string) => lookup(PROFILE_DETAIL, profile);

const FLAGS: Record<string, string> = {
  draining: 'Draining',
  user_paused: 'Paused by owner',
  pressure_limited: 'Limited by memory pressure',
  on_battery: 'On battery',
  overheated: 'Overheated',
  disk_low: 'Disk low',
  sleeping: 'Sleeping',
  stale: 'Stale telemetry',
  unreachable: 'Unreachable',
  quarantined: 'Quarantined capacity',
  upgrading: 'Upgrading',
  builds_disabled: 'Builds disabled',
};
export const flagLabel = (flag: string) => lookup(FLAGS, flag);

const LIFECYCLES: Record<string, string> = {
  enrolling: 'Enrolling',
  probing: 'Probing',
  ready: 'Ready',
  retired: 'Retired',
  revoked: 'Revoked',
};
export const lifecycleLabel = (value: string) => lookup(LIFECYCLES, value);

const OS: Record<string, string> = { linux: 'Linux', windows: 'Windows', macos: 'macOS' };
export const osLabel = (os: string) => lookup(OS, os);

export function selectorLabel(selector: PlatformSelector): string {
  const os = selector.os ? osLabel(selector.os) : 'any OS';
  const arch = selector.arch ?? 'any architecture';
  return selector.os_version ? `${os} ${selector.os_version}, ${arch}` : `${os}, ${arch}`;
}

const ENFORCEMENT: Record<string, string> = {
  hard: 'hard',
  advisory: 'advisory',
  bounded_guest: 'bounded guest',
  none: 'none',
};
export const enforcementLabel = (value: string) => lookup(ENFORCEMENT, value);

const ENFORCEMENT_DETAIL: Record<string, string> = {
  hard: 'a kernel-enforced limit',
  advisory: 'admission policy, priority and pressure response only; not a guarantee',
  bounded_guest: 'enforced inside a guest; the host overhead is accounted separately',
  none: 'no control and no reliable observation',
};
export const enforcementDetail = (value: string) =>
  Object.hasOwn(ENFORCEMENT_DETAIL, value)
    ? (ENFORCEMENT_DETAIL[value] as string)
    : 'a strength this dashboard does not know; treat it as not guaranteed';

const STEP_STATES: Record<string, string> = {
  pending: 'Pending',
  running: 'Running',
  ready: 'Ready',
  succeeded: 'Succeeded',
  failed: 'Failed',
  timed_out: 'Timed out',
  skipped: 'Skipped',
  stopped: 'Stopped',
};
export const stepStateLabel = (state: string) => lookup(STEP_STATES, state);

export function stepStateTone(state: string): Tone {
  switch (state) {
    case 'succeeded':
    case 'ready':
      return 'ok';
    case 'running':
      return 'info';
    case 'failed':
    case 'timed_out':
      return 'bad';
    case 'stopped':
      return 'warn';
    default:
      return 'neutral';
  }
}

const STEP_KINDS: Record<string, string> = {
  task: 'task',
  service: 'service',
  test: 'test',
  cleanup: 'cleanup',
};
export const stepKindLabel = (kind: string) => lookup(STEP_KINDS, kind);

const ENVIRONMENT_STATES: Record<string, string> = {
  starting: 'Starting',
  ready: 'Ready',
  finishing: 'Finishing',
  ended: 'Ended',
  quarantined: 'Quarantined',
};
export const environmentStateLabel = (state: string) => lookup(ENVIRONMENT_STATES, state);

export function environmentStateTone(state: string): Tone {
  switch (state) {
    case 'ready':
      return 'ok';
    case 'starting':
    case 'finishing':
      return 'info';
    case 'quarantined':
      return 'bad';
    default:
      return 'neutral';
  }
}

const CLOUD_MODES: Record<string, string> = {
  existing_only: 'Enrolled machines only',
  manual: 'Cloud by manual approval',
  auto_overflow: 'Automatic cloud overflow',
  hybrid: 'Automatic overflow plus manual exceptions',
};
export const cloudModeLabel = (mode: string) => lookup(CLOUD_MODES, mode);

const ROLES: Record<string, string> = { viewer: 'viewer', developer: 'developer', admin: 'admin' };
export const roleLabel = (role: string) => lookup(ROLES, role);

const LEASE_LIMITS: Record<string, string> = {
  idle: 'idle timeout',
  lease: 'lease',
  maximum: 'maximum lifetime',
};
export const leaseLimitLabel = (limit: string) => lookup(LEASE_LIMITS, limit);

// ------------------------------------------------------------ queue reasons

const REASONS: Record<string, string> = {
  schedulable: 'Ready to start',
  waiting_for_capacity: 'Waiting for capacity',
  no_compatible_machine: 'No machine can run this',
  wrong_platform: 'Wrong platform',
  missing_capability: 'Missing tool or SDK',
  machine_too_small: 'Machine too small',
  foreground_protection: 'Held back to protect the foreground',
  memory_pressure: 'Memory pressure',
  disk_low: 'Disk space low',
  stale_telemetry: 'Machine has not reported recently',
  machine_unreachable: 'Machine unreachable',
  machine_paused: 'Machine paused',
  machine_draining: 'Machine draining',
  on_battery: 'Machine on battery',
  builds_disabled: 'Builds disabled',
  project_not_allowed: 'Project not allowed on this machine',
  machine_concurrency_limit: 'Machine at its job limit',
  project_concurrency_limit: 'Project at its job limit',
  fair_share: 'Waiting its turn (fair share)',
  reserved_for_older_job: 'Capacity held for an older job',
  previous_attempt_unresolved: 'Previous attempt not resolved',
  cleanup_pending: 'Waiting for cleanup',
  quarantined: 'Capacity quarantined',
  snapshot_unavailable: 'Snapshot unavailable',
  source_lan_only: 'Source may not leave the LAN',
  cloud_manual_only: 'Cloud capacity needs approval',
  controller_reconciling: 'Controller is reconciling',
  offer_rejected: 'Offer rejected by the machine',
};
export const reasonLabel = (code: string) => lookup(REASONS, code);

/** How serious a waiting reason is: `bad` when waiting alone will not clear it. */
export function reasonTone(code: string): Tone {
  switch (code) {
    case 'schedulable':
      return 'ok';
    case 'no_compatible_machine':
    case 'wrong_platform':
    case 'missing_capability':
    case 'machine_too_small':
    case 'project_not_allowed':
    case 'snapshot_unavailable':
      return 'bad';
    case 'quarantined':
    case 'previous_attempt_unresolved':
    case 'cloud_manual_only':
    case 'machine_unreachable':
      return 'warn';
    default:
      return 'info';
  }
}

// --------------------------------------------------- machines and admission

/**
 * Why a machine is not taking new work right now, in plain sentences. Empty
 * when nothing known is stopping it. Built from the flags, the policy and
 * the owner's local controls, since those are what the API reports.
 */
export function notAdmittingReasons(machine: MachineView): string[] {
  const reasons: string[] = [];
  const flags = new Set<string>(machine.flags);
  const telemetry = machine.telemetry ?? null;
  const handled = new Set<string>();
  const take = (flag: string) => {
    handled.add(flag);
    return flags.has(flag);
  };

  if (machine.lifecycle !== 'ready') {
    reasons.push(
      `Lifecycle is "${lifecycleLabel(machine.lifecycle)}": only ready machines are offered work.`,
    );
  }
  if (take('builds_disabled') || !machine.policy.accept_builds) {
    reasons.push(
      machine.policy.profile === 'infrastructure_only'
        ? 'Builds are disabled by its policy (infrastructure only).'
        : 'Builds are disabled by its policy.',
    );
  }
  if (take('unreachable')) {
    reasons.push(
      'Unreachable: no contact for longer than the unreachable threshold. Its reservations stay held, because a lost connection does not prove the work stopped.',
    );
  }
  if (take('stale')) {
    reasons.push(
      'Telemetry is stale: no heartbeat within the freshness window, so it gets no new offers until it reports again.',
    );
  }
  if (take('sleeping')) reasons.push('The machine is asleep.');
  if (take('user_paused') || machine.local_control.paused) {
    reasons.push('Paused by its owner on the machine. Running work continues; new work is not accepted.');
  }
  if (take('draining') || machine.local_control.draining) {
    reasons.push('Draining: running work finishes, new work is not accepted.');
  }
  if (take('upgrading')) reasons.push('The worker is being upgraded.');
  if (take('quarantined')) {
    reasons.push(
      'Holds quarantined capacity: cleanup of an earlier attempt could not be verified, so that reservation is not released.',
    );
  }
  if (take('on_battery')) {
    const percent = telemetry?.power.battery_percent;
    const level = percent === null || percent === undefined ? '' : ` at ${percent}%`;
    reasons.push(
      machine.policy.require_ac_power
        ? `On battery${level}. Its policy accepts new work only on external power.`
        : `On battery${level}.`,
    );
  }
  if (take('pressure_limited')) {
    const pressure = telemetry ? ` (memory pressure: ${telemetry.memory_pressure})` : '';
    reasons.push(`The protection loop is holding back new work under memory pressure${pressure}.`);
  }
  if (take('overheated')) {
    const thermal = telemetry ? ` (thermal state: ${telemetry.thermal})` : '';
    reasons.push(`Too hot to take more work${thermal}.`);
  }
  if (take('disk_low')) {
    if (telemetry && telemetry.disk_total_bytes > 0) {
      const floor = diskFloorBytes(machine.policy, telemetry.disk_total_bytes);
      reasons.push(
        `Work volume has ${formatBytes(telemetry.disk_free_bytes)} free, below its floor of ${formatBytes(floor)}.`,
      );
    } else {
      reasons.push('Free space on the work volume is below its floor.');
    }
  }
  for (const flag of machine.flags) {
    if (!handled.has(flag)) reasons.push(`Flag "${flag}" is set (not known to this dashboard).`);
  }

  const limit = machine.policy.max_concurrent_attempts;
  if (limit !== null && limit !== undefined && machine.attempts.length >= limit) {
    reasons.push(
      `At its concurrency limit: ${machine.attempts.length} of ${limit} ${limit === 1 ? 'attempt' : 'attempts'}.`,
    );
  }
  return reasons;
}

export function powerText(machine: MachineView): string {
  const power = machine.telemetry?.power;
  if (!power) return 'unknown (no telemetry)';
  const percent =
    power.battery_percent === null || power.battery_percent === undefined
      ? ''
      : `, battery ${power.battery_percent}%`;
  if (power.on_ac === true) return `external power${percent}`;
  if (power.on_ac === false) return `on battery${percent}`;
  return percent ? `unknown source${percent}` : 'no battery reported';
}

export function lastSeenText(machine: MachineView, now: number): string {
  if (machine.last_seen_at === null || machine.last_seen_at === undefined) return 'never seen';
  return `last seen ${formatDuration(Math.max(now - machine.last_seen_at, 0))} ago`;
}
