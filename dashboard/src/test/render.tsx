// Renders the dashboard against the mock controller, the way it runs in
// the browser: live updates, the frame and every route.

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render } from '@testing-library/react';
import type { ReactNode } from 'react';
import { MemoryRouter, useLocation } from 'react-router';
import { AppRoutes } from '../App';
import { setTransport, type RawResponse, type Transport } from '../api/client';
import { LiveProvider, setEventConnect } from '../api/live';
import { createMock } from '../api/mock';
import { Shell } from '../components/Shell';
import { rawFixtures, type Json } from './fixtures';

export interface Setup {
  /** Starting route, e.g. `/jobs?show=failed`. */
  path?: string;
  fixtures?: Json;
  /** Answer some requests differently from the mock. */
  override?: (method: string, path: string) => RawResponse | undefined;
  role?: string;
  /** Render this instead of every route (and without the frame). */
  only?: ReactNode;
}

export interface Rendered {
  calls: { method: string; path: string }[];
  asked: (number | null)[];
  /** The router's current path and search. */
  location: () => string;
}

export function renderApp({ path = '/', fixtures = rawFixtures(), override, role, only }: Setup = {}): Rendered {
  const mock = createMock(fixtures, { delayMs: 0, role });
  const calls: { method: string; path: string }[] = [];
  const asked: (number | null)[] = [];
  const transport: Transport = async (method, requestPath, ...rest) => {
    calls.push({ method, path: requestPath });
    return override?.(method, requestPath) ?? mock.transport(method, requestPath, ...rest);
  };
  setTransport(transport);
  setEventConnect((after, signal) => {
    asked.push(after);
    return mock.connect(after, signal);
  });
  let current = path;
  function Probe() {
    const location = useLocation();
    current = `${location.pathname}${location.search}`;
    return null;
  }
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <LiveProvider token="t">
        <MemoryRouter initialEntries={[path]}>
          <Probe />
          {only ?? (
            <Shell signedIn>
              <AppRoutes />
            </Shell>
          )}
        </MemoryRouter>
      </LiveProvider>
    </QueryClientProvider>,
  );
  return { calls, asked, location: () => current };
}
