// How the Overview page classifies what the API reports: whether a machine
// is alive, and which held attempts are being launched or are running.

import type { PhaseMark } from '../api/activity';
import type {
  ActiveAttemptView,
  AttemptView,
  EnvironmentView,
  FleetOverview,
  JobView,
  MachineView,
} from '../api/schemas';
import { phaseLabel, type Tone } from './labels';

export type Liveness = 'connected' | 'stale' | 'unreachable' | 'disconnected';

export const LIVENESS: Record<Liveness, { label: string; tone: Tone; detail: string }> = {
  connected: { label: 'Connected', tone: 'ok', detail: 'Connected and reporting.' },
  stale: {
    label: 'Stale',
    tone: 'warn',
    detail: 'No heartbeat within the freshness window, so it gets no new offers.',
  },
  unreachable: {
    label: 'Unreachable',
    tone: 'bad',
    detail: 'No contact for longer than the unreachable threshold. Its reservations stay held.',
  },
  disconnected: {
    label: 'Disconnected',
    tone: 'bad',
    detail: 'No control connection to the controller.',
  },
};

/** The worst thing known about a machine's contact with the controller. */
export function liveness(machine: MachineView): Liveness {
  if (machine.flags.includes('unreachable')) return 'unreachable';
  if (!machine.connected) return 'disconnected';
  return machine.flags.includes('stale') ? 'stale' : 'connected';
}

export function livenessCounts(machines: MachineView[]): Record<Liveness, number> {
  const counts = { connected: 0, stale: 0, unreachable: 0, disconnected: 0 };
  for (const machine of machines) counts[liveness(machine)] += 1;
  return counts;
}

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

export const attemptPhaseLabel = (row: AttemptRow) =>
  row.accepted ? 'Accepted, starting' : phaseLabel(row.attempt.phase);

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
