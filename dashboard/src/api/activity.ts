// What the event stream has said lately, kept for the Overview page: the
// latest events for its activity feed, and when each active job last changed
// phase, which no REST response carries.

import type { EventRecord } from './schemas';

/** Events kept for the feed. */
export const FEED_LENGTH = 50;
/**
 * How many sequence numbers before the newest one a job list reports the
 * first connection asks to have replayed, so the feed does not start empty.
 */
export const REPLAY_EVENTS = 300;

/** A job entering a phase, as the controller recorded it. */
export interface PhaseMark {
  /** A job phase, or `accepted`: still offered, with the start committed. */
  phase: string;
  generation: number;
  at: number;
}

export interface Activity {
  /** Newest first, at most `FEED_LENGTH`. */
  events: readonly EventRecord[];
  /** The latest phase change of each unfinished job, by job id. */
  phases: ReadonlyMap<string, PhaseMark>;
}

const PHASE_EVENTS: Record<string, string> = {
  'job.offered': 'offered',
  'job.preparing': 'preparing',
  'job.executing': 'executing',
  'job.collecting': 'collecting',
  'job.cleaning': 'cleaning',
  'job.quarantined': 'quarantined',
  'attempt.start_committed': 'accepted',
};

/**
 * A job phase change that the controller records right beside the attempt
 * event saying the same thing. The feed shows the attempt event only.
 */
export function repeatsAttemptEvent(kind: string): boolean {
  return kind.startsWith('job.') && Object.hasOwn(PHASE_EVENTS, kind);
}

function generationOf(data: unknown): number | null {
  const value = (data as { generation?: unknown } | null | undefined)?.generation;
  return typeof value === 'number' ? value : null;
}

export class ActivityLog {
  private snapshot: Activity = { events: [], phases: new Map() };
  private readonly listeners = new Set<() => void>();
  private last = 0;

  /** Record events in sequence order; one already recorded is ignored. */
  add(batch: readonly EventRecord[]): void {
    const fresh: EventRecord[] = [];
    let phases: Map<string, PhaseMark> | null = null;
    for (const event of batch) {
      if (event.seq <= this.last) continue;
      this.last = event.seq;
      if (!repeatsAttemptEvent(event.kind)) fresh.push(event);

      const job = event.job_id;
      if (!job) continue;
      if (event.kind === 'job.finished' || event.kind === 'job.queued') {
        phases ??= new Map(this.snapshot.phases);
        phases.delete(job);
        continue;
      }
      const phase = Object.hasOwn(PHASE_EVENTS, event.kind) ? PHASE_EVENTS[event.kind] : undefined;
      const generation = generationOf(event.data);
      if (phase !== undefined && generation !== null) {
        phases ??= new Map(this.snapshot.phases);
        phases.set(job, { phase, generation, at: event.at });
      }
    }
    if (fresh.length === 0 && phases === null) return;
    this.snapshot = {
      events:
        fresh.length === 0
          ? this.snapshot.events
          : [...fresh.reverse(), ...this.snapshot.events].slice(0, FEED_LENGTH),
      phases: phases ?? this.snapshot.phases,
    };
    for (const listener of this.listeners) listener();
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  read = (): Activity => this.snapshot;
}
