// How the Overview page reads what the API reports: which held attempts are
// being launched or are running, and what needs a person.

import type { PhaseMark } from '../api/activity';
import type {
  ActiveAttemptView,
  AttemptView,
  EnvironmentView,
  FleetOverview,
  JobView,
  QueueView,
} from '../api/schemas';
import type { Tone } from './labels';
import { isFailure, machineStatus, queueLine } from './status';

/** Phases before anything executes. Every other held phase counts as running. */
const LAUNCHING: ReadonlySet<string> = new Set(['offered', 'preparing']);
export const isLaunching = (phase: string) => LAUNCHING.has(phase);

/** A time, and whether it is the one asked for or only the offer's. */
export interface Since {
  at: number;
  /** The time is when the job was offered, which is earlier than what was asked. */
  sinceOffer: boolean;
}

export interface AttemptRow {
  attempt: ActiveAttemptView;
  machineId: string;
  machineName: string;
  /** Offered with the start already committed: the machine accepted it. */
  accepted: boolean;
  /** When the attempt entered its phase; `null` when the job list lacks it. */
  phaseSince: Since | null;
  /** When it started executing; `null` when the job list lacks it. */
  runningSince: Since | null;
}

function phaseSince(
  attempt: AttemptView | undefined,
  accepted: boolean,
  phase: string,
  mark: PhaseMark | undefined,
): Since | null {
  if (!attempt) return null;
  const wanted = accepted ? 'accepted' : phase;
  if (mark && mark.generation === attempt.generation && mark.phase === wanted) {
    return { at: mark.at, sinceOffer: false };
  }
  // An offer that is not yet accepted has been in that phase since it was made.
  return { at: attempt.offered_at, sinceOffer: phase !== 'offered' || accepted };
}

export interface AttemptRows {
  launching: AttemptRow[];
  running: AttemptRow[];
}

const startOf = (row: AttemptRow) => row.runningSince?.at ?? Number.POSITIVE_INFINITY;

/**
 * Every attempt holding a reservation, as the fleet response lists them, so
 * that these rows and the machine table never disagree. Times come from the
 * job list and the event stream, which may lag or be missing.
 */
export function attemptRows(
  fleet: FleetOverview,
  jobs: JobView[] | undefined,
  phases: ReadonlyMap<string, PhaseMark>,
): AttemptRows {
  const attempts = new Map<string, AttemptView>();
  for (const job of jobs ?? []) {
    for (const attempt of job.attempts) attempts.set(attempt.id, attempt);
  }
  const rows: AttemptRows = { launching: [], running: [] };
  for (const machine of fleet.machines) {
    for (const attempt of machine.attempts) {
      const detail = attempts.get(attempt.attempt_id);
      const accepted = attempt.phase === 'offered' && detail?.start_committed === true;
      const started = detail?.started_at;
      const row: AttemptRow = {
        attempt,
        machineId: machine.id,
        machineName: machine.name,
        accepted,
        phaseSince: phaseSince(detail, accepted, attempt.phase, phases.get(attempt.job_id)),
        runningSince:
          started !== null && started !== undefined
            ? { at: started, sinceOffer: false }
            : detail
              ? { at: detail.offered_at, sinceOffer: true }
              : null,
      };
      rows[isLaunching(attempt.phase) ? 'launching' : 'running'].push(row);
    }
  }
  // Longest waiting first, where a time is known.
  rows.launching.sort(
    (a, b) =>
      (a.phaseSince?.at ?? Number.POSITIVE_INFINITY) - (b.phaseSince?.at ?? Number.POSITIVE_INFINITY),
  );
  rows.running.sort((a, b) => startOf(a) - startOf(b));
  return rows;
}

/** Environments that still exist: everything but the ended ones. */
export const aliveEnvironments = (list: EnvironmentView[]) =>
  list.filter((environment) => environment.state !== 'ended');

// ---------------------------------------------------------------- attention

export interface AttentionItem {
  key: string;
  tone: Tone;
  /** What needs a person, in one sentence. */
  text: string;
  /** Where to act on it. */
  to: string;
}

/**
 * What needs a person right now: machines out of contact or holding
 * capacity that cleanup could not release, and queued jobs that no machine
 * can run however long they wait, the most serious first. Empty when
 * everything is fine.
 */
export function attentionItems(fleet: FleetOverview | undefined, queue: QueueView | undefined): AttentionItem[] {
  const items: AttentionItem[] = [];
  if (fleet?.reconciling) {
    items.push({
      key: 'reconciling',
      tone: 'warn',
      text: 'The controller restarted and is checking in with its machines; no new jobs start until it is done.',
      to: '/machines',
    });
  }
  for (const machine of fleet?.machines ?? []) {
    const status = machineStatus(machine);
    const to = `/machines/${machine.id}`;
    if (status.key === 'offline' && machine.lifecycle === 'ready') {
      items.push({
        key: `offline/${machine.id}`,
        tone: 'bad',
        text:
          machine.attempts.length > 0
            ? `${machine.name} is offline (${status.precise.toLowerCase()}) while holding ${machine.attempts.length} ${machine.attempts.length === 1 ? 'job' : 'jobs'}.`
            : `${machine.name} is offline (${status.precise.toLowerCase()}).`,
        to,
      });
    } else if (status.key === 'stale') {
      items.push({
        key: `stale/${machine.id}`,
        tone: 'warn',
        text: `${machine.name} has stopped reporting, so it gets no new jobs.`,
        to,
      });
    }
    if (machine.flags.includes('quarantined')) {
      items.push({
        key: `quarantined/${machine.id}`,
        tone: 'bad',
        text: `${machine.name} holds capacity that could not be cleaned up; someone has to resolve it.`,
        to,
      });
    }
  }
  for (const entry of queue?.entries ?? []) {
    const line = queueLine(entry.job.queue_reason);
    if (line.stuck) {
      items.push({
        key: `stuck/${entry.job.id}`,
        tone: 'bad',
        text: `${entry.job.recipe} (${entry.job.project_name}) ${line.headline.charAt(0).toLowerCase()}${line.headline.slice(1)}.`,
        to: `/jobs/${entry.job.id}`,
      });
    }
  }
  // What only a person can fix comes first; the sort keeps the order within each tone.
  return items.sort((a, b) => (a.tone === 'bad' ? 0 : 1) - (b.tone === 'bad' ? 0 : 1));
}

/** The longest-waiting queued job, by the time the queue was read. */
export function longestWait(queue: QueueView): QueueView['entries'][number] | undefined {
  let longest: QueueView['entries'][number] | undefined;
  for (const entry of queue.entries) {
    if (!longest || entry.waiting_ms > longest.waiting_ms) longest = entry;
  }
  return longest;
}

export interface Failures {
  count: number;
  /** The list may not reach back to `since`, so there may be more. */
  atLeast: boolean;
}

/**
 * Jobs that finished without succeeding since `since`, from a list of the
 * most recently changed jobs. A finished job's last change is its finish.
 */
export function failuresSince(jobs: JobView[], since: number, listLimit: number): Failures {
  const count = jobs.filter((job) => isFailure(job) && job.updated_at >= since).length;
  const oldest = jobs.reduce((min, job) => Math.min(min, job.updated_at), Number.POSITIVE_INFINITY);
  return { count, atLeast: jobs.length >= listLimit && oldest >= since };
}
