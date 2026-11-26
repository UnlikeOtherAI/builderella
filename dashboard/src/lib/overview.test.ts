import { describe, expect, it } from 'vitest';
import type { PhaseMark } from '../api/activity';
import { Fixtures } from '../api/schemas';
import { rawFixtures } from '../test/fixtures';
import { lastSeenText } from './labels';
import { aliveEnvironments, attemptRows, attentionItems, failuresSince, longestWait } from './overview';

const data = () => Fixtures.parse(rawFixtures());
const NONE = new Map<string, PhaseMark>();

describe('lastSeenText', () => {
  it('says when a machine was last seen', () => {
    const { fleet } = data();
    const linux = fleet.machines.find((m) => m.name === 'linux-1')!;
    expect(lastSeenText(linux, fleet.server_time)).toBe('last seen 21s ago');
    expect(lastSeenText({ ...linux, last_seen_at: null }, 0)).toBe('never seen');
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
    expect(offered().accepted).toBe(false);
    const id = offered().attempt.attempt_id;
    jobs.flatMap((j) => j.attempts).find((a) => a.id === id)!.start_committed = true;
    expect(offered().accepted).toBe(true);
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

describe('attentionItems', () => {
  it('names machines out of contact, stuck capacity and jobs that cannot run', () => {
    const { fleet, queue } = data();
    const items = attentionItems(fleet, queue);
    expect(items.map((item) => item.key.split('/')[0])).toEqual(['quarantined', 'offline', 'stuck', 'stale']);
    expect(items.every((item) => item.to.startsWith('/machines/') || item.to.startsWith('/jobs/'))).toBe(true);
  });

  it('is empty when nothing needs a person, and mentions a reconciling controller', () => {
    const { fleet, queue } = data();
    expect(attentionItems({ ...fleet, machines: [] }, { ...queue, entries: [] })).toEqual([]);
    expect(attentionItems({ ...fleet, machines: [], reconciling: true }, undefined)[0]!.key).toBe('reconciling');
    expect(attentionItems(undefined, undefined)).toEqual([]);
  });
});

describe('headline numbers', () => {
  it('finds the longest wait', () => {
    expect(longestWait(data().queue)!.job.recipe).toBe('lint');
    expect(longestWait({ entries: [], shares: [] })).toBeUndefined();
  });

  it('counts failures since a time, and says when the list may not reach back far enough', () => {
    const { jobs, fleet } = data();
    const hourAgo = fleet.server_time - 3_600_000;
    expect(failuresSince(jobs, hourAgo, 200)).toEqual({ count: 1, atLeast: false });
    expect(failuresSince(jobs, 0, 200).count).toBe(3);
    // A full list whose oldest entry is still inside the window may miss some.
    const recent = jobs.map((job) => ({ ...job, updated_at: fleet.server_time }));
    expect(failuresSince(recent, hourAgo, recent.length).atLeast).toBe(true);
  });
});
