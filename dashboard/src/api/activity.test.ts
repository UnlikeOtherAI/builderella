import { describe, expect, it, vi } from 'vitest';
import { rawFixtures } from '../test/fixtures';
import { ActivityLog, FEED_LENGTH, REPLAY_EVENTS } from './activity';
import { setTransport } from './client';
import { replayStart } from './live';
import { createMock } from './mock';
import type { EventRecord } from './schemas';

const event = (seq: number, kind = 'machine.connected', extra: Partial<EventRecord> = {}): EventRecord => ({
  seq,
  at: seq * 1000,
  entity: 'machine',
  entity_id: 'mch_1',
  kind,
  ...extra,
});

describe('ActivityLog', () => {
  it('keeps the newest events, newest first, each once', () => {
    const log = new ActivityLog();
    const heard = vi.fn();
    log.subscribe(heard);
    log.add(Array.from({ length: FEED_LENGTH + 5 }, (_, i) => event(i + 1)));
    log.add([event(3), event(FEED_LENGTH + 6)]);
    const seqs = log.read().events.map((e) => e.seq);
    expect(seqs).toHaveLength(FEED_LENGTH);
    expect(seqs.slice(0, 3)).toEqual([FEED_LENGTH + 6, FEED_LENGTH + 5, FEED_LENGTH + 4]);
    expect(new Set(seqs).size).toBe(seqs.length);
    expect(heard).toHaveBeenCalledTimes(2);
    // Nothing new: same snapshot, nobody told.
    const before = log.read();
    log.add([event(1)]);
    expect(log.read()).toBe(before);
    expect(heard).toHaveBeenCalledTimes(2);
  });

  it('remembers when an unfinished job entered its phase', () => {
    const log = new ActivityLog();
    const job = { entity: 'job' as const, job_id: 'job_a' };
    log.add([event(1, 'attempt.start_committed', { ...job, data: { generation: 2 } })]);
    expect(log.read().phases.get('job_a')).toEqual({ phase: 'accepted', generation: 2, at: 1000 });
    log.add([event(2, 'job.preparing', { ...job, data: { generation: 2 } })]);
    expect(log.read().phases.get('job_a')).toEqual({ phase: 'preparing', generation: 2, at: 2000 });
    // The phase change repeats an attempt event, so the feed leaves it out.
    expect(log.read().events.map((e) => e.kind)).toEqual(['attempt.start_committed']);
    log.add([event(3, 'job.finished', job)]);
    expect(log.read().phases.has('job_a')).toBe(false);
  });
});

describe('replayStart', () => {
  it('asks for a stretch of history before the newest known event', async () => {
    const raw = rawFixtures();
    raw.jobs[0].last_event = 5_000;
    setTransport(createMock(raw, { delayMs: 0 }).transport);
    // The mock lists newest-updated first, within the first hundred.
    expect(await replayStart(new AbortController().signal)).toBe(5_000 - REPLAY_EVENTS);
  });

  it('replays from the beginning on a young fleet, and from now on when it cannot ask', async () => {
    setTransport(async () => ({ status: 200, body: [] }));
    expect(await replayStart(new AbortController().signal)).toBe(0);
    setTransport(async () => ({ status: 500, body: { code: 'internal', message: 'broken' } }));
    expect(await replayStart(new AbortController().signal)).toBeNull();
  });
});
