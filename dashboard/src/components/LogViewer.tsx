// Pages through a job's log with the server's cursor and can follow it
// while the job runs.
//
// Log text is untrusted. It is cleaned of terminal escapes and rendered as
// text nodes only: no markup is interpreted, nothing is turned into a link.

import { useCallback, useEffect, useReducer, useRef } from 'react';
import { api } from '../api/client';
import type { LogPage } from '../api/schemas';
import { sanitizeLogText } from '../lib/ansi';
import { ErrorPanel } from './ui';

const PAGE_BYTES = 64 * 1024;
/** Lines kept on screen; older ones are dropped and counted. */
export const MAX_LINES = 4000;
const FOLLOW_IDLE_MS = 2000;
const FOLLOW_BUSY_MS = 250;

interface Line {
  key: number;
  step: string;
  stream: string;
  text: string;
}

export interface LogState {
  lines: Line[];
  nextKey: number;
  cursor: string | undefined;
  complete: boolean;
  truncated: boolean;
  /** The view started at the tail, so earlier output is not loaded. */
  startedAtTail: boolean;
  dropped: number;
  loading: boolean;
  /** The last page had output, so more may be waiting. */
  lastHadOutput: boolean;
  loaded: boolean;
  error: unknown;
}

export const initialLogState: LogState = {
  lines: [],
  nextKey: 0,
  cursor: undefined,
  complete: false,
  truncated: false,
  startedAtTail: false,
  dropped: 0,
  loading: false,
  lastHadOutput: false,
  loaded: false,
  error: null,
};

export type LogAction =
  | { type: 'reset' }
  | { type: 'loading' }
  | { type: 'page'; page: LogPage; tail: boolean }
  | { type: 'error'; error: unknown };

export function logReducer(state: LogState, action: LogAction): LogState {
  switch (action.type) {
    case 'reset':
      return initialLogState;
    case 'loading':
      return { ...state, loading: true, error: null };
    case 'error':
      return { ...state, loading: false, error: action.error };
    case 'page': {
      const base = action.tail ? { ...initialLogState, startedAtTail: true } : state;
      let key = base.nextKey;
      const added = action.page.entries.map((entry) => ({
        key: key++,
        step: entry.step,
        stream: entry.stream,
        text: sanitizeLogText(entry.text),
      }));
      const all = [...base.lines, ...added];
      const overflow = Math.max(all.length - MAX_LINES, 0);
      return {
        ...base,
        lines: overflow > 0 ? all.slice(overflow) : all,
        nextKey: key,
        dropped: base.dropped + overflow,
        cursor: action.page.next_cursor,
        complete: action.page.complete,
        // Once reported, truncation stays reported.
        truncated: base.truncated || action.page.truncated,
        lastHadOutput: action.page.entries.length > 0,
        loading: false,
        loaded: true,
        error: null,
      };
    }
  }
}

const STREAM_LABEL: Record<string, string> = { stdout: 'out', stderr: 'err', system: 'worker' };

export function LogViewer({
  jobId,
  steps,
  running,
  follow,
  onFollowChange,
  step,
  onStepChange,
}: {
  jobId: string;
  steps: string[];
  /** The job can still produce output. */
  running: boolean;
  follow: boolean;
  onFollowChange: (follow: boolean) => void;
  step: string;
  onStepChange: (step: string) => void;
}) {
  const [state, dispatch] = useReducer(logReducer, initialLogState);
  const generation = useRef(0);
  const box = useRef<HTMLDivElement>(null);
  const cursor = useRef<string | undefined>(undefined);
  cursor.current = state.cursor;

  const load = useCallback(
    async (mode: 'next' | 'tail') => {
      const mine = generation.current;
      dispatch({ type: 'loading' });
      try {
        const page = await api.logs(jobId, {
          step: step || undefined,
          cursor: mode === 'next' ? cursor.current : undefined,
          tail: mode === 'tail' ? true : undefined,
          limit_bytes: PAGE_BYTES,
        });
        if (generation.current === mine) dispatch({ type: 'page', page, tail: mode === 'tail' });
      } catch (error) {
        if (generation.current === mine) dispatch({ type: 'error', error });
      }
    },
    [jobId, step],
  );

  // A different job or step filter starts over from the beginning.
  useEffect(() => {
    generation.current += 1;
    cursor.current = undefined;
    dispatch({ type: 'reset' });
    void load('next');
  }, [load]);

  // Following: keep asking for the next page until the log is complete.
  const waiting = follow && state.loaded && !state.complete && !state.loading && !state.error;
  useEffect(() => {
    if (!waiting) return;
    const timer = setTimeout(
      () => void load('next'),
      state.lastHadOutput ? FOLLOW_BUSY_MS : FOLLOW_IDLE_MS,
    );
    return () => clearTimeout(timer);
  }, [waiting, state.lastHadOutput, state.nextKey, load]);

  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [follow, state.nextKey]);

  return (
    <div className="log">
      <div className="log-controls">
        <div className="field">
          <label htmlFor="log-step">Step</label>
          <select id="log-step" value={step} onChange={(event) => onStepChange(event.target.value)}>
            <option value="">All steps</option>
            {steps.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </div>
        <div className="field field-check">
          <input
            id="log-follow"
            type="checkbox"
            checked={follow}
            onChange={(event) => onFollowChange(event.target.checked)}
          />
          <label htmlFor="log-follow">Follow new output</label>
        </div>
        <button type="button" className="btn" onClick={() => void load('tail')} disabled={state.loading}>
          Jump to the end
        </button>
      </div>

      <div className="log-box" ref={box} role="log" aria-label="Job log" tabIndex={0}>
        {state.startedAtTail ? (
          <p className="log-marker">Showing the end of the log. Earlier output is not loaded.</p>
        ) : null}
        {state.dropped > 0 ? (
          <p className="log-marker">
            {state.dropped} earlier {state.dropped === 1 ? 'chunk was' : 'chunks were'} dropped from this
            view to keep it responsive. The full log is kept by the controller.
          </p>
        ) : null}
        {state.lines.map((line) => (
          <div key={line.key} className={`log-line log-${line.stream}`}>
            <span className="log-meta">
              {line.step} {STREAM_LABEL[line.stream] ?? line.stream}
            </span>
            <pre>{line.text}</pre>
          </div>
        ))}
        {state.loaded && state.lines.length === 0 && !state.error ? (
          <p className="log-marker">
            {state.complete ? 'This job produced no output.' : 'No output yet.'}
          </p>
        ) : null}
        {state.truncated ? (
          <p className="log-marker log-truncated" role="note">
            Output truncated here: the job went over its log allowance and later output was not kept.
          </p>
        ) : null}
        {state.complete ? <p className="log-marker">End of log. No more output will arrive.</p> : null}
      </div>

      {state.error ? <ErrorPanel error={state.error} onRetry={() => void load('next')} /> : null}

      <p className="log-status" role="status">
        {state.loading
          ? 'Loading…'
          : state.complete
            ? 'Complete.'
            : follow
              ? 'Following: checking for new output every few seconds.'
              : running
                ? 'Not following. The job can still produce output.'
                : 'More output may be available.'}
      </p>
      {!state.complete && !follow ? (
        <button type="button" className="btn" onClick={() => void load('next')} disabled={state.loading}>
          Load the next page
        </button>
      ) : null}
    </div>
  );
}
