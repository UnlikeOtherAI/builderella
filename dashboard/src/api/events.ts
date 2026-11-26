// The controller's event stream: `GET /events?after=<seq>` as server-sent
// events. `EventSource` cannot send an `Authorization` header, so the stream
// is read with `fetch` and parsed here.
//
// The stream only says that something changed; views re-read the REST API.
// Losing it therefore never shows wrong data, it only delays a refresh, and
// polling covers the gap.

import { z } from 'zod';
import { EventRecord } from './schemas';

// ------------------------------------------------------------- SSE parsing

export interface SseMessage {
  id?: string;
  event?: string;
  data: string;
}

/**
 * Incremental parser for the `text/event-stream` format. Feed it decoded
 * text in any chunking; it calls `onMessage` once per dispatched event.
 */
export function createSseParser(onMessage: (message: SseMessage) => void) {
  let buffer = '';
  let data: string[] = [];
  let id: string | undefined;
  let event: string | undefined;

  const line = (text: string) => {
    if (text === '') {
      if (data.length > 0) onMessage({ id, event, data: data.join('\n') });
      data = [];
      event = undefined;
      return;
    }
    if (text.startsWith(':')) return; // comment, used as a keep-alive
    const colon = text.indexOf(':');
    const field = colon === -1 ? text : text.slice(0, colon);
    let value = colon === -1 ? '' : text.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'data') data.push(value);
    else if (field === 'id') id = value;
    else if (field === 'event') event = value;
  };

  return {
    feed(chunk: string): void {
      buffer += chunk;
      for (;;) {
        const match = /\r\n|\n|\r/.exec(buffer);
        if (!match) return;
        // A lone CR at the very end may be the first half of a CRLF.
        if (match[0] === '\r' && match.index === buffer.length - 1) return;
        const text = buffer.slice(0, match.index);
        buffer = buffer.slice(match.index + match[0].length);
        line(text);
      }
    },
  };
}

// --------------------------------------------------------------- reconnect

export type StreamStatus =
  /** First connection attempt. */
  | 'connecting'
  /** Connected; events arrive as they happen. */
  | 'open'
  /** Lost; waiting to retry from the last sequence seen. */
  | 'reconnecting'
  /** The controller rejected the token. No further attempts. */
  | 'unauthorized'
  /** Stopped on purpose (sign-out, page closed). */
  | 'stopped';

/** Thrown by `connect` when the controller answers 401. */
export class StreamUnauthorized extends Error {
  constructor() {
    super('the controller rejected the token');
    this.name = 'StreamUnauthorized';
  }
}

export interface EventStreamOptions {
  /**
   * Open the stream after sequence `after` (`null` for "from now"). Must
   * yield an empty string once the connection is established, then decoded
   * text as it arrives, and finish when the server closes the stream.
   */
  connect: (after: number | null, signal: AbortSignal) => AsyncIterable<string>;
  onEvent: (event: EventRecord) => void;
  onStatus: (status: StreamStatus, detail?: string) => void;
  /** An event did not match the schema. It is skipped; the stream goes on. */
  onDrift: (detail: string) => void;
  signal: AbortSignal;
  /** Resolves after `ms`, or as soon as `signal` aborts. */
  sleep?: (ms: number, signal: AbortSignal) => Promise<void>;
  /** Delay before reconnect attempt `failures` (0 = right after a healthy connection). */
  backoff?: (failures: number) => number;
  /** Sequence to resume from; `null` starts from now. */
  after?: number | null;
}

/** 1 s, 2 s, 4 s, 8 s, then 15 s. */
export function backoffMs(failures: number): number {
  return Math.min(1000 * 2 ** Math.max(failures, 0), 15_000);
}

function defaultSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const timer = setTimeout(done, ms);
    function done() {
      clearTimeout(timer);
      signal.removeEventListener('abort', done);
      resolve();
    }
    signal.addEventListener('abort', done);
  });
}

/**
 * Read events until `signal` aborts or the token is rejected, reconnecting
 * after every loss from the highest sequence delivered so far. An event at
 * or below that sequence is a replay and is dropped, so `onEvent` sees each
 * event at most once and in order.
 */
export async function runEventStream(options: EventStreamOptions): Promise<void> {
  const { connect, onEvent, onStatus, onDrift, signal } = options;
  const sleep = options.sleep ?? defaultSleep;
  const backoff = options.backoff ?? backoffMs;
  let last: number | null = options.after ?? null;
  let failures = 0;
  let first = true;

  while (!signal.aborted) {
    if (first) onStatus('connecting');
    first = false;
    let opened = false;
    let detail: string | undefined;

    const parser = createSseParser((message) => {
      let raw: unknown;
      try {
        raw = JSON.parse(message.data);
      } catch {
        onDrift('an event was not JSON');
        return;
      }
      const parsed = EventRecord.safeParse(raw);
      if (!parsed.success) {
        // Step past it when its sequence is readable, so a reconnect does
        // not ask for the same unreadable event again.
        const seq = (raw as { seq?: unknown } | null)?.seq;
        if (typeof seq === 'number' && (last === null || seq > last)) last = seq;
        onDrift(z.prettifyError(parsed.error));
        return;
      }
      if (last !== null && parsed.data.seq <= last) return;
      last = parsed.data.seq;
      onEvent(parsed.data);
    });

    try {
      for await (const chunk of connect(last, signal)) {
        if (!opened) {
          opened = true;
          onStatus('open');
        }
        if (chunk !== '') parser.feed(chunk);
      }
      detail = 'the controller closed the stream';
    } catch (error) {
      if (signal.aborted) break;
      if (error instanceof StreamUnauthorized) {
        onStatus('unauthorized');
        return;
      }
      detail = error instanceof Error ? error.message : String(error);
    }
    if (signal.aborted) break;

    // A connection that opened was healthy: retry quickly. One that never
    // opened backs off further each time.
    failures = opened ? 0 : failures + 1;
    onStatus('reconnecting', detail);
    await sleep(backoff(failures), signal);
  }
  onStatus('stopped');
}

// -------------------------------------------------------- real connection

/** `connect` for a real controller: a streaming `fetch` with the bearer token. */
export function fetchEventStream(prefix: string, token: string | null) {
  return async function* connect(after: number | null, signal: AbortSignal): AsyncGenerator<string> {
    const url = `${prefix}/events${after === null ? '' : `?after=${after}`}`;
    const headers: Record<string, string> = { accept: 'text/event-stream' };
    if (token) headers.authorization = `Bearer ${token}`;
    const response = await fetch(url, { headers, signal, cache: 'no-store' });
    if (response.status === 401) throw new StreamUnauthorized();
    if (!response.ok || !response.body) {
      throw new Error(`the event stream answered HTTP ${response.status}`);
    }
    yield '';
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        yield decoder.decode(value, { stream: true });
      }
    } finally {
      reader.cancel().catch(() => undefined);
    }
  };
}

// ------------------------------------------------------------ what changed

/**
 * Query keys an event makes stale. Deliberately broad: a wasted re-read is
 * cheap, a stale view is not.
 */
export function staleKeys(event: EventRecord): string[][] {
  const keys: string[][] = [];
  const job = event.job_id ?? (event.entity === 'job' ? event.entity_id : null);
  if (job) keys.push(['job', job], ['artifacts', job]);
  switch (event.entity) {
    case 'machine':
      keys.push(['fleet'], ['queue']);
      break;
    case 'job':
    case 'attempt':
      keys.push(['queue'], ['jobs'], ['fleet']);
      break;
    case 'environment':
      keys.push(['environments'], ['environment'], ['fleet']);
      break;
    case 'project':
      keys.push(['projects'], ['queue']);
      break;
    case 'snapshot':
      break;
    default:
      // Fleet-wide, cloud, or an entity this dashboard does not know.
      keys.push(['fleet'], ['queue'], ['jobs'], ['environments'], ['environment'], ['projects']);
  }
  return keys;
}
