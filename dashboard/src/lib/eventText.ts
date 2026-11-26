// One short sentence for each event the controller records. The sentence
// follows the name of the job or machine the event is about.

import { Outcome } from '../api/schemas';
import type { EventRecord } from '../api/schemas';
import { describeOutcome } from './job';
import { cloudModeLabel, flagLabel, profileLabel, roleLabel, stepStateLabel } from './labels';

export interface EventNames {
  machine: (id: string) => string;
}

function field(value: unknown, key: string): unknown {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>)[key] : undefined;
}

function str(value: unknown, key: string): string | undefined {
  const found = field(value, key);
  return typeof found === 'string' && found !== '' ? found : undefined;
}

function num(value: unknown, key: string): number | undefined {
  const found = field(value, key);
  return typeof found === 'number' ? found : undefined;
}

function outcomeText(value: unknown): string {
  const parsed = Outcome.safeParse(value);
  if (!parsed.success) return '';
  const text = describeOutcome(parsed.data);
  return `: ${text.label.toLowerCase()}${text.detail ? ` (${text.detail})` : ''}`;
}

const because = (data: unknown, key = 'reason') => {
  const reason = str(data, key);
  return reason ? `: ${reason}` : '';
};

/** Conditions the controller sets and clears as `machine.<flag>[_cleared]`. */
const CONDITIONS: Record<string, [on: string, off: string]> = {
  stale: ['Stale: no recent heartbeat', 'Reporting again; no longer stale'],
  unreachable: ['Unreachable', 'Reachable again'],
  draining: ['Draining', 'No longer draining'],
  quarantined: ['Has quarantined capacity', 'Quarantined capacity cleared'],
};

type Describe = (data: unknown, on: string) => string;

/** `on` is " on <machine>" when the event names one, else empty. */
const KINDS: Record<string, Describe> = {
  'fleet.created': () => 'Fleet created',
  'fleet.controller_started': (d) => {
    const epoch = num(d, 'epoch');
    return epoch === undefined ? 'Controller started' : `Controller started (epoch ${epoch})`;
  },
  'pairing_code.created': (d) => {
    const profile = str(d, 'profile');
    return `Pairing code created${profile ? ` for a ${profileLabel(profile).toLowerCase()}` : ''}`;
  },
  'pairing_code.consumed': () => 'Pairing code used to enroll',
  'principal.created': (d) => {
    const name = str(d, 'name');
    const role = str(d, 'role');
    return `Token created${name ? ` for ${name}` : ''}${role ? ` (${roleLabel(role)})` : ''}`;
  },
  'principal.revoked': () => 'Token revoked',
  'project.created': (d) => `Project ${str(d, 'name') ?? ''} created`.replace('  ', ' '),
  'project.policy_changed': (d) => {
    const cloud = str(field(d, 'policy'), 'cloud');
    return `Project policy changed${cloud ? ` (cloud: ${cloudModeLabel(cloud).toLowerCase()})` : ''}`;
  },
  'snapshot.registered': (d) => {
    const files = num(d, 'file_count');
    return `Snapshot registered${files === undefined ? '' : ` (${files} ${files === 1 ? 'file' : 'files'})`}`;
  },

  'machine.enrolled': () => 'Enrolled',
  'machine.connected': () => 'Connected',
  'machine.disconnected': (d) => `Disconnected${because(d)}`,
  'machine.reconciled': (d) => {
    const released = num(d, 'released') ?? 0;
    const uncertain = num(d, 'uncertain') ?? 0;
    const parts = [
      released > 0 ? `${released} released` : '',
      uncertain > 0 ? `${uncertain} uncertain` : '',
    ].filter(Boolean);
    return `Reconciled with the controller${parts.length > 0 ? ` (${parts.join(', ')})` : ''}`;
  },
  'machine.flags_changed': (d) => {
    const flags = field(d, 'flags');
    if (!Array.isArray(flags)) return 'Conditions changed';
    if (flags.length === 0) return 'Reports no limiting conditions';
    return `Reports: ${flags.map((flag) => flagLabel(String(flag)).toLowerCase()).join(', ')}`;
  },
  'machine.local_control_changed': (d) => {
    const control = field(d, 'local_control');
    const paused = field(control, 'paused') === true;
    const draining = field(control, 'draining') === true;
    if (paused && draining) return 'Owner paused and drained it';
    if (paused) return 'Owner paused new work';
    if (draining) return 'Owner set it to drain';
    return 'Owner controls changed: accepting work';
  },
  'machine.inventory_changed': () => 'Hardware or tool inventory changed',
  'machine.policy_changed': () => 'Policy changed',
  'machine.renamed': (d) => {
    const from = str(d, 'from');
    return from ? `Renamed from ${from}` : 'Renamed';
  },
  'machine.retired': () => 'Retired',
  'machine.revoked': () => 'Revoked',

  'job.queued': (d) =>
    field(d, 'retry') === true
      ? 'Queued again for another attempt'
      : field(d, 'retry') === false
        ? 'Back in the queue'
        : 'Submitted and queued',
  'job.offered': () => 'Offered',
  'job.preparing': () => 'Preparing',
  'job.executing': () => 'Executing',
  'job.collecting': () => 'Collecting artifacts',
  'job.cleaning': () => 'Cleaning up',
  'job.quarantined': () => 'Quarantined',
  'job.cancel_requested': () => 'Cancellation requested',
  'job.finished': (d) => `Finished${outcomeText(field(d, 'outcome'))}`,

  'attempt.offered': (_d, on) => `Offered${on ? ` to${on.slice(3)}` : ''}`,
  'attempt.accepted': (_d, on) => `Offer accepted${on}`,
  'attempt.start_committed': (_d, on) => `Start committed${on}`,
  'attempt.start_delivered': (_d, on) => `Start delivered${on}`,
  'attempt.preparing': (_d, on) => `Preparing the workspace${on}`,
  'attempt.started': (_d, on) => `Started${on}`,
  'attempt.step_changed': (d, on) => {
    const event = field(d, 'event');
    const step = str(event, 'step');
    const state = str(event, 'state');
    const code = num(event, 'exit_code');
    const exit = code === undefined || code === 0 ? '' : ` with code ${code}`;
    return step && state
      ? `Step ${step}: ${stepStateLabel(state).toLowerCase()}${exit}${on}`
      : `A step changed${on}`;
  },
  'attempt.environment_ready': (_d, on) => `Services ready${on}`,
  'attempt.collecting': (_d, on) => `Collecting artifacts${on}`,
  'attempt.artifact_published': (d, on) => {
    const path = str(field(d, 'event'), 'path');
    return `Artifact published${path ? `: ${path}` : ''}${on}`;
  },
  'attempt.exited': (d, on) => `Exited${on}${outcomeText(field(field(d, 'event'), 'outcome'))}`,
  'attempt.cleanup': (d, on) => {
    const state = str(field(d, 'event'), 'state');
    return `Cleanup${state ? `: ${state.replaceAll('_', ' ')}` : ' progressed'}${on}`;
  },
  'attempt.released': (_d, on) => `Resources released${on}`,
  'attempt.quarantined': (d, on) => `Quarantined${on}${because(d)}`,
  'attempt.uncertain': (_d, on) => `Contact lost; may still be running${on}`,
  'attempt.uncertain_cleared': (_d, on) => `Contact restored${on}`,
  'attempt.log_truncated': (d, on) => {
    const step = str(d, 'step');
    return `Log truncated${step ? ` in step ${step}` : ''}${on}`;
  },
  // Offers withdrawn before anything ran: `attempt.<release reason>`.
  'attempt.rejected': (_d, on) => `Offer rejected${on}`,
  'attempt.expired': (_d, on) => `Offer expired${on}`,
  'attempt.revoked': (_d, on) => `Offer revoked${on}`,
  'attempt.cancelled': (_d, on) => `Offer cancelled${on}`,
  'attempt.forgotten': (_d, on) => `Offer dropped during reconciliation${on}`,

  'environment.ready': (_d, on) => `Environment ready${on}`,
  'environment.renewed': () => 'Environment lease renewed',
  'environment.finishing': (d) => `Environment finishing${because(d)}`,
  'environment.ended': (d) => `Environment ended${because(d)}`,
  'environment.quarantined': (d) => `Environment quarantined${because(d)}`,
};

/** Every kind with its own sentence, apart from the machine conditions. */
export const KNOWN_EVENT_KINDS: readonly string[] = Object.keys(KINDS);

/**
 * The sentence for an event, or `null` for a kind this dashboard does not
 * know, which is then shown by its raw name.
 */
export function describeEvent(event: EventRecord, names: EventNames): string | null {
  const known = Object.hasOwn(KINDS, event.kind) ? KINDS[event.kind] : undefined;
  if (known) {
    const machine = str(event.data, 'machine_id');
    return known(event.data, machine ? ` on ${names.machine(machine)}` : '');
  }
  const condition = /^machine\.([a-z_]+?)(_cleared)?$/.exec(event.kind);
  const pair = condition && Object.hasOwn(CONDITIONS, condition[1]!) ? CONDITIONS[condition[1]!] : undefined;
  if (pair) return pair[condition![2] ? 1 : 0];
  return null;
}
