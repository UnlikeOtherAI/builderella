import { describe, expect, it } from 'vitest';
import type { PhaseMark } from '../api/activity';
import { Fixtures } from '../api/schemas';
import { rawFixtures } from '../test/fixtures';
import { lastSeenText } from './labels';
import {
  LIVENESS,
  aliveEnvironments,
  attemptPhaseLabel,
  attemptRows,
  liveness,
  livenessCounts,
} from './overview';

const data = () => Fixtures.parse(rawFixtures());
const NONE = new Map<string, PhaseMark>();

describe('liveness', () => {
  const machine = (name: string) => {
    const found = data().fleet.machines.find((m) => m.name === name);
    if (!found) throw new Error(`no fixture machine ${name}`);
    return found;
  };

  it('tells connected, stale, unreachable and disconnected apart', () => {
    expect(liveness(machine('builder-1'))).toBe('connected');
    expect(liveness(machine('linux-1'))).toBe('stale');
    expect(liveness(machine('handheld-1'))).toBe('unreachable');
    expect(liveness({ ...machine('builder-1'), connected: false })).toBe('disconnected');
    // A lost connection outranks a stale mark.
    expect(liveness({ ...machine('linux-1'), connected: false })).toBe('disconnected');
  });

  it('words each state differently, with a different mark from connected', () => {
    const labels = Object.values(LIVENESS).map((state) => state.label);
    expect(new Set(labels).size).toBe(4);
    expect(LIVENESS.stale.tone).not.toBe(LIVENESS.connected.tone);
    expect(LIVENESS.unreachable.tone).not.toBe(LIVENESS.stale.tone);
  });

  it('says when a machine was last seen', () => {
    const { fleet } = data();
    expect(lastSeenText(machine('linux-1'), fleet.server_time)).toBe('last seen 21s ago');
    expect(lastSeenText({ ...machine('linux-1'), last_seen_at: null }, 0)).toBe('never seen');
  });

  it('counts every machine exactly once', () => {
    expect(livenessCounts(data().fleet.machines)).toEqual({
      connected: 4,
      stale: 1,
      unreachable: 1,
      disconnected: 0,
    });
  });
});

describe('attemptRows', () => {
  it('splits held attempts into launching and running by phase', () => {
    const { fleet, jobs } = data();
    const { launching, running } = attemptRows(fleet, jobs, NONE);
    expect(new Set(launching.map((r) => r.attempt.phase))).toEqual(new Set(['offered', 'preparing']));
    expect(new Set(running.map((r) => r.attempt.phase))).toEqual(
      new Set(['executing', 'collecting', 'cleaning', 'quarantined']),
    );
    const held = fleet.machines.flatMap((m) => m.attempts);
    expect(launching.length + running.length).toBe(held.length);
  });

  it('keeps an attempt in a phase it does not know, as running', () => {
    const { fleet, jobs } = data();
    fleet.machines[0]!.attempts[0]!.phase = 'hibernating';
    const { running } = attemptRows(fleet, jobs, NONE);
    expect(running.some((r) => r.attempt.phase === 'hibernating')).toBe(true);
  });

  it('calls an offer with a committed start accepted', () => {
    const { fleet, jobs } = data();
    const offered = () =>
      attemptRows(fleet, jobs, NONE).launching.find((r) => r.attempt.phase === 'offered')!;
    expect(attemptPhaseLabel(offered())).toBe('Offered');
    const id = offered().attempt.attempt_id;
    jobs.flatMap((j) => j.attempts).find((a) => a.id === id)!.start_committed = true;
    expect(attemptPhaseLabel(offered())).toBe('Accepted, starting');
  });

  it('times a phase from its event, and says when it only has the offer time', () => {
    const { fleet, jobs } = data();
    const preparing = jobs.find((j) => j.phase === 'preparing')!;
    const attempt = preparing.attempts.at(-1)!;
    const row = (phases: Map<string, PhaseMark>) =>
      attemptRows(fleet, jobs, phases).launching.find((r) => r.attempt.job_id === preparing.id)!;

    expect(row(new Map()).phaseSince).toEqual({ at: attempt.offered_at, sinceOffer: true });
    const mark = { phase: 'preparing', generation: attempt.generation, at: attempt.offered_at + 4000 };
    expect(row(new Map([[preparing.id, mark]])).phaseSince).toEqual({ at: mark.at, sinceOffer: false });
    // A mark of an earlier attempt, or of another phase, says nothing.
    for (const stale of [
      { ...mark, generation: attempt.generation + 1 },
      { ...mark, phase: 'offered' },
    ]) {
      expect(row(new Map([[preparing.id, stale]])).phaseSince?.sinceOffer).toBe(true);
    }
    // An unaccepted offer has been offered since the offer: exact.
    const offered = attemptRows(fleet, jobs, NONE).launching.find((r) => r.attempt.phase === 'offered')!;
    expect(offered.phaseSince?.sinceOffer).toBe(false);
  });

  it('keeps every row when the job list is missing, with times unknown', () => {
    const { fleet } = data();
    const { launching, running } = attemptRows(fleet, undefined, NONE);
    expect(launching.length).toBeGreaterThan(0);
    expect(running.every((r) => r.runningSince === null && r.phaseSince === null)).toBe(true);
  });
});

describe('aliveEnvironments', () => {
  it('leaves out only the ended ones', () => {
    const { environments } = data();
    const alive = aliveEnvironments(environments);
    expect(alive.length).toBe(environments.filter((e) => e.state !== 'ended').length);
    expect(new Set(alive.map((e) => e.state))).toEqual(
      new Set(['ready', 'starting', 'finishing', 'quarantined']),
    );
  });
});
