// One status vocabulary for the whole dashboard. Every machine and every job
// is shown as a coloured dot plus one of a few plain words; the precise state
// the API reports stays available as `precise` for detail views and
// tooltips. A value this dashboard does not know is shown raw, never hidden
// and never mapped to a word that might be wrong.

import type { JobView, MachineView, Outcome, QueueReason } from '../api/schemas';
import { describeOutcome } from './job';
import { flagLabel, lifecycleLabel, phaseLabel, reasonLabel, reasonTone, type Tone } from './labels';

export interface Status {
  /** A key of the vocabulary, or `unknown` for a value shown raw. */
  key: string;
  /** The plain word shown next to the dot. */
  label: string;
  tone: Tone;
  /** The exact state behind the word, for detail views and tooltips. */
  precise: string;
}

interface Word {
  label: string;
  tone: Tone;
  /** What the word means, for a newcomer. */
  meaning: string;
}

// ---------------------------------------------------------------- machines

export type MachineStatusKey =
  | 'online'
  | 'busy'
  | 'idle'
  | 'stale'
  | 'offline'
  | 'draining'
  | 'paused'
  | 'retired';

export const MACHINE_WORDS: Record<MachineStatusKey, Word> = {
  online: { label: 'Online', tone: 'ok', meaning: 'Connected and reporting.' },
  busy: { label: 'Busy', tone: 'info', meaning: 'Online and running jobs.' },
  idle: { label: 'Idle', tone: 'ok', meaning: 'Online with nothing running; ready for work.' },
  stale: {
    label: 'Stale',
    tone: 'warn',
    meaning: 'Connected, but no recent report, so it gets no new jobs until it reports again.',
  },
  offline: {
    label: 'Offline',
    tone: 'bad',
    meaning: 'Not in contact with the controller. Jobs it held stay reserved until it is back.',
  },
  draining: {
    label: 'Draining',
    tone: 'warn',
    meaning: 'Finishing what it runs and taking nothing new.',
  },
  paused: { label: 'Paused', tone: 'neutral', meaning: 'Takes no new jobs; running work continues.' },
  retired: { label: 'Retired', tone: 'neutral', meaning: 'Removed from the fleet.' },
};

const KNOWN_LIFECYCLES = new Set(['enrolling', 'probing', 'ready', 'retired', 'revoked']);

function word(key: MachineStatusKey, precise: string): Status {
  const { label, tone } = MACHINE_WORDS[key];
  return { key, label, tone, precise };
}

/**
 * The one word for a machine. The order is what matters most to someone
 * looking at it: gone, out of contact, not reporting, deliberately not
 * taking work, and then whether it is running anything.
 */
export function machineStatus(machine: MachineView): Status {
  const flags = new Set<string>(machine.flags);
  const { lifecycle } = machine;
  if (!KNOWN_LIFECYCLES.has(lifecycle)) {
    return { key: 'unknown', label: lifecycle, tone: 'neutral', precise: `Lifecycle "${lifecycle}"` };
  }
  if (lifecycle === 'retired' || lifecycle === 'revoked') return word('retired', lifecycleLabel(lifecycle));
  if (lifecycle === 'enrolling' || lifecycle === 'probing') {
    return word('offline', `${lifecycleLabel(lifecycle)}: not yet ready for jobs`);
  }
  if (flags.has('unreachable')) return word('offline', 'Unreachable');
  if (!machine.connected) return word('offline', 'Disconnected');
  if (flags.has('sleeping')) return word('offline', 'Sleeping');
  if (flags.has('stale')) return word('stale', 'Stale telemetry');
  if (flags.has('draining') || machine.local_control.draining) return word('draining', 'Draining');
  if (flags.has('user_paused') || machine.local_control.paused) return word('paused', 'Paused by owner');
  if (flags.has('builds_disabled') || !machine.policy.accept_builds) return word('paused', 'Builds disabled');
  const count = machine.attempts.length;
  if (count > 0) return word('busy', `Running ${count} ${count === 1 ? 'job' : 'jobs'}`);
  return word('idle', 'Connected, nothing running');
}

/** In contact and reporting, whatever it is doing. */
export function isOnline(status: Status): boolean {
  return ['online', 'busy', 'idle', 'draining', 'paused'].includes(status.key);
}

/** Flags already said by the status word, which a flag list need not repeat. */
const SAID_BY_STATUS: Record<string, string[]> = {
  offline: ['unreachable', 'sleeping'],
  stale: ['stale'],
  draining: ['draining'],
  paused: ['user_paused', 'builds_disabled'],
};

/** Most important first; an unknown flag ranks after the known ones. */
const FLAG_RANK = [
  'quarantined',
  'unreachable',
  'stale',
  'overheated',
  'disk_low',
  'pressure_limited',
  'on_battery',
  'upgrading',
  'sleeping',
  'draining',
  'user_paused',
  'builds_disabled',
];

const FLAG_TONE: Record<string, Tone> = {
  quarantined: 'bad',
  unreachable: 'bad',
  overheated: 'warn',
  disk_low: 'warn',
  pressure_limited: 'warn',
  on_battery: 'warn',
  stale: 'warn',
};

export interface FlagChip {
  flag: string;
  /** Short enough for a chip. */
  label: string;
  tone: Tone;
  /** The full wording, for a tooltip. */
  detail?: string;
}

/** Chip-sized names of machine conditions; the full wording is `flagLabel`. */
const FLAG_SHORT: Record<string, string> = {
  quarantined: 'Stuck cleanup',
  pressure_limited: 'Memory pressure',
  overheated: 'Too hot',
  stale: 'Stale',
  user_paused: 'Paused by owner',
  builds_disabled: 'Builds off',
  sleeping: 'Asleep',
};

/** The machine's conditions not already said by its status word, most important first. */
export function machineFlags(machine: MachineView, status = machineStatus(machine)): FlagChip[] {
  const said = new Set(SAID_BY_STATUS[status.key] ?? []);
  const rank = (flag: string) => {
    const index = FLAG_RANK.indexOf(flag);
    return index === -1 ? FLAG_RANK.length : index;
  };
  return machine.flags
    .filter((flag) => !said.has(flag))
    .sort((a, b) => rank(a) - rank(b))
    .map((flag) => ({
      flag,
      label: FLAG_SHORT[flag] ?? flagLabel(flag),
      tone: FLAG_TONE[flag] ?? 'neutral',
      detail: flagLabel(flag),
    }));
}

// -------------------------------------------------------------------- jobs

export type JobStatusKey =
  | 'queued'
  | 'launching'
  | 'running'
  | 'cleaning'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'timed_out'
  | 'interrupted'
  | 'out_of_resources';

export const JOB_WORDS: Record<JobStatusKey, Word> = {
  queued: { label: 'Queued', tone: 'neutral', meaning: 'Waiting for a machine.' },
  launching: {
    label: 'Launching',
    tone: 'info',
    meaning: 'Given to a machine, which is getting ready to run it.',
  },
  running: { label: 'Running', tone: 'info', meaning: 'Its steps are running.' },
  cleaning: {
    label: 'Cleaning up',
    tone: 'info',
    meaning: 'Finished running; the machine is stopping processes and removing the workspace.',
  },
  succeeded: { label: 'Succeeded', tone: 'ok', meaning: 'Every step passed.' },
  failed: { label: 'Failed', tone: 'bad', meaning: 'A step failed, or the job was refused.' },
  cancelled: { label: 'Cancelled', tone: 'neutral', meaning: 'Someone cancelled it.' },
  timed_out: { label: 'Timed out', tone: 'bad', meaning: 'It ran past its time limit.' },
  interrupted: {
    label: 'Interrupted',
    tone: 'warn',
    meaning: 'The machine or the network failed under it. Not a failure of the build itself.',
  },
  out_of_resources: {
    label: 'Out of resources',
    tone: 'bad',
    meaning: 'It needed more memory, disk or processes than it was allowed.',
  },
};

const PHASE_WORD: Record<string, JobStatusKey> = {
  capturing: 'queued',
  queued: 'queued',
  offered: 'launching',
  preparing: 'launching',
  executing: 'running',
  collecting: 'running',
  cleaning: 'cleaning',
  quarantined: 'cleaning',
};

const OUTCOME_WORD: Record<string, JobStatusKey> = {
  succeeded: 'succeeded',
  failed: 'failed',
  rejected: 'failed',
  cancelled: 'cancelled',
  timed_out: 'timed_out',
  infra_interrupted: 'interrupted',
  resource_exhausted: 'out_of_resources',
};

function jobWord(key: JobStatusKey, precise: string): Status {
  const { label, tone } = JOB_WORDS[key];
  return { key, label, tone, precise };
}

const PHASE_PRECISE: Record<string, string> = {
  capturing: 'Capturing: the snapshot is still being uploaded',
  offered: 'Offered to a machine',
  preparing: 'Preparing the workspace',
  executing: 'Executing steps',
  collecting: 'Collecting artifacts',
  cleaning: 'Cleaning up',
  quarantined: 'Quarantined: cleanup could not be verified',
};

/** The word for an attempt, from its phase alone (the fleet lists attempts this way). */
export function phaseStatus(phase: string): Status {
  const key = Object.hasOwn(PHASE_WORD, phase) ? PHASE_WORD[phase] : undefined;
  if (key) return jobWord(key, PHASE_PRECISE[phase] ?? phaseLabel(phase));
  return { key: 'unknown', label: phaseLabel(phase), tone: 'neutral', precise: `Phase "${phase}"` };
}

/** The word for an outcome. */
export function outcomeStatus(outcome: Outcome): Status {
  const text = describeOutcome(outcome);
  const precise = text.detail ? `${text.label}: ${text.detail}` : text.label;
  if (outcome.type === 'unknown') {
    return { key: 'unknown', label: outcome.raw, tone: 'neutral', precise };
  }
  const key = OUTCOME_WORD[outcome.type];
  return key ? jobWord(key, precise) : { key: 'unknown', label: outcome.type, tone: 'neutral', precise };
}

/**
 * The word for a job. While anything runs or is cleaned up the phase
 * decides; once it is finished, the outcome does.
 */
export function jobStatus(job: Pick<JobView, 'phase' | 'outcome'>): Status {
  if (job.phase === 'finished') {
    if (job.outcome) return outcomeStatus(job.outcome);
    return { key: 'unknown', label: 'Finished', tone: 'neutral', precise: 'Finished without a reported result' };
  }
  return phaseStatus(job.phase);
}

/** Conditions worth a chip beside a job's word. */
export function jobFlags(job: JobView): FlagChip[] {
  const flags: FlagChip[] = [];
  if (job.phase === 'quarantined' || job.cleanup === 'quarantined') {
    flags.push({ flag: 'quarantined', label: 'Stuck cleanup', tone: 'bad' });
  }
  if (job.cancel_requested && job.phase !== 'finished') {
    flags.push({ flag: 'cancel_requested', label: 'Cancelling', tone: 'warn' });
  }
  let current = job.attempts[0];
  for (const attempt of job.attempts) {
    if (!current || attempt.generation > current.generation) current = attempt;
  }
  if (current?.uncertain && job.phase !== 'finished') {
    flags.push({ flag: 'uncertain', label: 'Out of contact', tone: 'warn' });
  }
  return flags;
}

/** The job-list filter a job belongs to. */
export type JobGroup = 'running' | 'queued' | 'finished';

export function jobGroup(job: Pick<JobView, 'phase'>): JobGroup {
  if (job.phase === 'finished') return 'finished';
  if (job.phase === 'queued' || job.phase === 'capturing') return 'queued';
  return 'running';
}

/** Finished without succeeding, other than by being cancelled. */
export function isFailure(job: Pick<JobView, 'phase' | 'outcome'>): boolean {
  if (job.phase !== 'finished' || !job.outcome) return false;
  return !['succeeded', 'cancelled'].includes(job.outcome.type);
}

// ------------------------------------------------------------------- queue

export interface QueueLine {
  /** `Can't run: …` when waiting alone will not clear it. */
  headline: string;
  tone: Tone;
  /** The controller's own sentence. */
  message: string;
  /** Waiting will not help: someone has to change something. */
  stuck: boolean;
}

/** Why a job waits, as one line. */
export function queueLine(reason: QueueReason | null | undefined): QueueLine {
  if (!reason) {
    return { headline: 'Waiting', tone: 'neutral', message: 'The controller gave no reason.', stuck: false };
  }
  const tone = reasonTone(reason.code);
  const label = reasonLabel(reason.code);
  const stuck = tone === 'bad';
  return {
    headline: stuck ? `Can't run: ${label.charAt(0).toLowerCase()}${label.slice(1)}` : label,
    tone,
    message: reason.message,
    stuck,
  };
}
