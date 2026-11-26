// Live updates. One event stream per signed-in session marks queries stale
// as things change; while the stream is down, queries poll instead.

import { useQueryClient } from '@tanstack/react-query';
import { createContext, useContext, useEffect, useState, type ReactNode } from 'react';
import { API_PREFIX, clearToken } from './client';
import { fetchEventStream, runEventStream, staleKeys, type StreamStatus } from './events';

type Connect = (after: number | null, signal: AbortSignal) => AsyncIterable<string>;

let connectOverride: Connect | null = null;

/** Use another event source: the mock layer in mock mode. */
export function setEventConnect(connect: Connect | null): void {
  connectOverride = connect;
}

export interface LiveState {
  status: StreamStatus;
  /** Why the stream was last lost. */
  detail?: string;
  /** Set when an event did not match the schema and was skipped. */
  drift?: string;
}

const LiveContext = createContext<LiveState>({ status: 'stopped' });

export const useLive = () => useContext(LiveContext);

/** With the stream open, polling is only a safety net. */
export const LIVE_POLL_MS = 30_000;
/** Without it, polling is how the views stay current. */
export const FALLBACK_POLL_MS = 5_000;

export function pollInterval(state: LiveState): number {
  return state.status === 'open' && !state.drift ? LIVE_POLL_MS : FALLBACK_POLL_MS;
}

export const usePollInterval = () => pollInterval(useLive());

export function LiveProvider({ token, children }: { token: string | null; children: ReactNode }) {
  const queryClient = useQueryClient();
  const [state, setState] = useState<LiveState>({ status: 'connecting' });

  useEffect(() => {
    const controller = new AbortController();
    const pending = new Map<string, string[]>();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let wasOpen = false;

    // Events arrive in bursts; one refresh per burst is enough.
    const flush = () => {
      timer = undefined;
      for (const queryKey of pending.values()) void queryClient.invalidateQueries({ queryKey });
      pending.clear();
    };

    void runEventStream({
      connect: connectOverride ?? fetchEventStream(API_PREFIX, token),
      signal: controller.signal,
      onEvent: (event) => {
        for (const key of staleKeys(event)) pending.set(key.join('/'), key);
        timer ??= setTimeout(flush, 300);
      },
      onStatus: (status, detail) => {
        if (controller.signal.aborted) return;
        if (status === 'unauthorized') clearToken();
        // Anything may have changed while the stream was down.
        if (status === 'open' && wasOpen) void queryClient.invalidateQueries();
        if (status === 'open') wasOpen = true;
        setState((previous) => ({
          ...previous,
          status,
          detail: status === 'open' ? undefined : (detail ?? previous.detail),
        }));
      },
      onDrift: (drift) => {
        if (!controller.signal.aborted) setState((previous) => ({ ...previous, drift }));
      },
    });

    return () => {
      controller.abort();
      if (timer !== undefined) clearTimeout(timer);
    };
  }, [token, queryClient]);

  return <LiveContext.Provider value={state}>{children}</LiveContext.Provider>;
}
