import { describe, expect, it } from 'vitest';
import {
  StreamUnauthorized,
  backoffMs,
  createSseParser,
  runEventStream,
  staleKeys,
  type SseMessage,
} from './events';
import { EventRecord } from './schemas';

const record = (seq: number, extra: Record<string, unknown> = {}) => ({
  seq,
  at: 1_795_000_000_000 + seq,
  entity: 'job',
  entity_id: 'job_0123456789abcdef0123456789abcdef',
  kind: 'job.queued',
  ...extra,
});
const sse = (seq: number) => `id: ${seq}\nevent: job.queued\ndata: ${JSON.stringify(record(seq))}\n\n`;

describe('createSseParser', () => {
  const collect = () => {
    const messages: SseMessage[] = [];
    return { messages, parser: createSseParser((m) => messages.push(m)) };
  };

  it('dispatches on a blank line and joins data lines', () => {
    const { messages, parser } = collect();
    parser.feed('id: 7\nevent: job.queued\ndata: {"a":\ndata: 1}\n\n');
    expect(messages).toEqual([{ id: '7', event: 'job.queued', data: '{"a":\n1}' }]);
  });

  it('reassembles events split anywhere, including inside a CRLF', () => {
    const { messages, parser } = collect();
    const text = 'data: one\r\n\r\n: keep-alive\r\ndata: two\r\n\r\n';
    for (const char of text) parser.feed(char);
    expect(messages.map((m) => m.data)).toEqual(['one', 'two']);
  });

  it('ignores comments and events without data', () => {
    const { messages, parser } = collect();
    parser.feed(': ping\n\nevent: noop\n\ndata:x\n\n');
    expect(messages.map((m) => m.data)).toEqual(['x']);
  });
});

interface Run {
  afters: (number | null)[];
  events: number[];
  statuses: string[];
  sleeps: number[];
  drifts: string[];
}

/** Run the stream against scripted connections; abort after the last one. */
async function run(
  scripts: ((after: number | null) => AsyncGenerator<string>)[],
  after: number | null = null,
): Promise<Run> {
  const controller = new AbortController();
  const out: Run = { afters: [], events: [], statuses: [], sleeps: [], drifts: [] };
  let call = 0;
  await runEventStream({
    after,
    signal: controller.signal,
    connect: (from) => {
      out.afters.push(from);
      const script = scripts[call++];
      if (script) return script(from);
      controller.abort();
      return (async function* () {})();
    },
    onEvent: (event) => out.events.push(event.seq),
    onStatus: (status) => out.statuses.push(status),
    onDrift: (detail) => out.drifts.push(detail),
    sleep: async (ms) => {
      out.sleeps.push(ms);
    },
  });
  return out;
}

describe('runEventStream', () => {
  it('reconnects from the last sequence it delivered', async () => {
    const out = await run([
      async function* () {
        yield '';
        yield sse(5) + sse(6);
        throw new Error('connection reset');
      },
      async function* () {
        yield '';
        yield sse(7);
      },
    ]);
    expect(out.afters).toEqual([null, 6, 7]);
    expect(out.events).toEqual([5, 6, 7]);
    expect(out.statuses).toEqual(['connecting', 'open', 'reconnecting', 'open', 'reconnecting', 'stopped']);
  });

  it('drops events the server replays after a reconnect', async () => {
    const out = await run([
      async function* () {
        yield '';
        yield sse(10) + sse(11);
      },
      async function* () {
        yield '';
        yield sse(10) + sse(11) + sse(12);
      },
    ]);
    expect(out.events).toEqual([10, 11, 12]);
    expect(out.afters).toEqual([null, 11, 12]);
  });

  it('resumes from a given sequence and ignores anything at or below it', async () => {
    const out = await run(
      [
        async function* () {
          yield '';
          yield sse(40) + sse(41);
        },
      ],
      40,
    );
    expect(out.afters[0]).toBe(40);
    expect(out.events).toEqual([41]);
  });

  it('backs off while it cannot connect and recovers quickly after it could', async () => {
    const refuse = async function* (): AsyncGenerator<string> {
      throw new Error('refused');
    };
    const out = await run([
      refuse,
      refuse,
      refuse,
      refuse,
      refuse,
      async function* () {
        yield '';
        yield sse(1);
      },
    ]);
    // Five failures back off; a connection that opened resets the delay.
    expect(out.sleeps).toEqual([2000, 4000, 8000, 15_000, 15_000, 1000]);
    expect(out.events).toEqual([1]);
    expect(out.statuses.filter((s) => s === 'open')).toHaveLength(1);
  });

  it('stops for good when the token is rejected', async () => {
    const out = await run([
      async function* (): AsyncGenerator<string> {
        throw new StreamUnauthorized();
      },
      async function* () {
        yield '';
        yield sse(1);
      },
    ]);
    expect(out.afters).toEqual([null]);
    expect(out.statuses).toEqual(['connecting', 'unauthorized']);
    expect(out.sleeps).toEqual([]);
  });

  it('skips an event of the wrong shape, reports it, and steps past it', async () => {
    const bad = `data: ${JSON.stringify({ seq: 21, at: 'yesterday', entity: 'job' })}\n\n`;
    const out = await run([
      async function* () {
        yield '';
        yield sse(20) + bad + 'data: not json\n\n';
      },
      async function* () {
        yield '';
        yield sse(22);
      },
    ]);
    expect(out.events).toEqual([20, 22]);
    expect(out.drifts).toHaveLength(2);
    // The unreadable event is not asked for again.
    expect(out.afters).toEqual([null, 21, 22]);
  });

  it('stops while waiting to reconnect when aborted', async () => {
    const controller = new AbortController();
    const statuses: string[] = [];
    let connects = 0;
    await runEventStream({
      signal: controller.signal,
      connect: () => {
        connects += 1;
        return (async function* (): AsyncGenerator<string> {
          throw new Error('down');
        })();
      },
      onEvent: () => undefined,
      onStatus: (status) => statuses.push(status),
      onDrift: () => undefined,
      sleep: async () => {
        controller.abort();
      },
    });
    expect(connects).toBe(1);
    expect(statuses.at(-1)).toBe('stopped');
  });
});

describe('backoffMs', () => {
  it('doubles from one second and is capped', () => {
    expect([0, 1, 2, 3, 4, 9].map(backoffMs)).toEqual([1000, 2000, 4000, 8000, 15_000, 15_000]);
  });
});

describe('staleKeys', () => {
  const event = (extra: Record<string, unknown>) => EventRecord.parse(record(1, extra));

  it('refreshes the job an attempt event belongs to', () => {
    const job = `job_${'f'.repeat(32)}`;
    const keys = staleKeys(event({ entity: 'attempt', entity_id: 'att_x', job_id: job }));
    expect(keys).toContainEqual(['job', job]);
    expect(keys).toContainEqual(['queue']);
    expect(keys).toContainEqual(['fleet']);
  });

  it('refreshes everything for an entity it does not know', () => {
    const keys = staleKeys(event({ entity: 'galaxy', entity_id: 'x' }));
    for (const key of ['fleet', 'queue', 'jobs', 'environments', 'projects']) {
      expect(keys).toContainEqual([key]);
    }
  });
});
