import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { cleanup, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router';
import { afterEach, describe, expect, it } from 'vitest';
import { setTransport, type RawResponse, type Transport } from '../api/client';
import { LiveProvider, setEventConnect } from '../api/live';
import { createMock } from '../api/mock';
import { rawFixtures, type Json } from '../test/fixtures';
import { OverviewView, QUEUE_SHOWN } from './Overview';

afterEach(() => {
  cleanup();
  setEventConnect(null);
});

interface Setup {
  fixtures?: Json;
  /** Answer some requests differently from the mock. */
  override?: (path: string) => RawResponse | undefined;
  role?: string;
}

function show({ fixtures = rawFixtures(), override, role }: Setup = {}) {
  const mock = createMock(fixtures, { delayMs: 0, role });
  const calls: { method: string; path: string }[] = [];
  const asked: (number | null)[] = [];
  const transport: Transport = async (method, path, ...rest) => {
    calls.push({ method, path });
    return override?.(path) ?? mock.transport(method, path, ...rest);
  };
  setTransport(transport);
  setEventConnect((after, signal) => {
    asked.push(after);
    return mock.connect(after, signal);
  });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <LiveProvider token="t">
        <MemoryRouter>
          <OverviewView />
        </MemoryRouter>
      </LiveProvider>
    </QueryClientProvider>,
  );
  return { calls, asked };
}

const section = (title: string) => screen.getByRole('region', { name: new RegExp(`^${title}( \\(|$)`) });
const rows = (title: string) => within(section(title)).getAllByRole('row').slice(1);
const total = (label: string) => screen.getByText(label, { selector: 'dt' }).nextElementSibling!.textContent;
const loaded = () =>
  waitFor(() => {
    expect(rows('Machines').length).toBeGreaterThan(0);
    expect(rows('Queue').length).toBeGreaterThan(0);
    expect(rows('Alive').length).toBeGreaterThan(0);
    expect(within(section('Running')).queryByText('unknown')).toBeNull();
    expect(rows('Running').length).toBeGreaterThan(0);
  });

describe('Overview', () => {
  it('shows totals that match the sections below', async () => {
    show();
    await loaded();
    expect(total('Machines connected')).toBe('4 of 6 1 stale, 1 unreachable');
    expect(total('Queued')).toBe('11');
    expect(total('Launching')).toBe(String(rows('Launching').length));
    expect(total('Running')).toBe(String(rows('Running').length));
    expect(total('Environments alive')).toBe(String(rows('Alive').length));
    expect(rows('Machines')).toHaveLength(6);
  });

  it('lists each machine with liveness, flags, enforcement, load and its jobs', async () => {
    show();
    await loaded();
    const row = (name: string) =>
      within(section('Machines')).getByRole('link', { name }).closest('tr') as HTMLElement;
    const stale = row('linux-1');
    expect(stale.className).toContain('contact-stale');
    expect(within(stale).getByText('Stale')).toBeTruthy();
    expect(within(stale).getByText(/last seen \d+s ago/)).toBeTruthy();
    expect(within(stale).getByText('Disk low')).toBeTruthy();
    expect(within(row('handheld-1')).getByText('Unreachable')).toBeTruthy();
    expect(within(row('handheld-1')).getByText('(uncertain)')).toBeTruthy();
    expect(within(row('studio-1')).getByText('advisory')).toBeTruthy();
    expect(within(row('studio-1')).getByText('Paused by owner')).toBeTruthy();
    expect(within(row('infra-1')).getByText('idle')).toBeTruthy();
    expect(within(row('infra-1')).getByRole('link', { name: 'infra-1' }).getAttribute('href')).toMatch(
      /^\/fleet\/mch_/,
    );
    expect(within(row('builder-1')).getByText(/ \/ [\d.]+ CPUs$/)).toBeTruthy();
    expect(within(row('builder-1')).getAllByRole('link', { name: /^job_/ })).toHaveLength(5);
  });

  it('separates what is being launched from what is running', async () => {
    show();
    await loaded();
    const launching = section('Launching');
    expect(within(launching).getByText('Offered')).toBeTruthy();
    expect(within(launching).getByText('Preparing')).toBeTruthy();
    expect(within(launching).queryByText('Executing')).toBeNull();
    const running = section('Running');
    expect(within(running).getAllByText('Executing').length).toBeGreaterThan(0);
    expect(within(running).getAllByText('Cleaning up').length).toBeGreaterThan(0);
    expect(within(running).getByText('Quarantined')).toBeTruthy();
    expect(within(running).getAllByText('resources still held').length).toBeGreaterThanOrEqual(3);
    expect(within(running).queryByText('Preparing')).toBeNull();
  });

  it('lists the first queued jobs with their reason, and links to the rest', async () => {
    show();
    await loaded();
    expect(rows('Queue')).toHaveLength(QUEUE_SHOWN);
    expect(within(section('Queue')).getByText(/Showing the first 10 of 11/)).toBeTruthy();
    expect(within(section('Queue')).getByRole('link', { name: 'Open the full queue' })).toBeTruthy();
    expect(within(rows('Queue')[0]!).getByText('1')).toBeTruthy();
  });

  it('shows held environments only, with endpoints', async () => {
    show();
    await loaded();
    expect(rows('Alive')).toHaveLength(5);
    expect(within(section('Alive')).queryByText('Ended')).toBeNull();
    expect(within(section('Alive')).getAllByText(/studio-1\.fleet\.example:/).length).toBeGreaterThan(0);
  });

  it('replays recent events newest first, and shows an unknown kind by name', async () => {
    const { asked } = show();
    const feed = screen.getByRole('list', { name: 'Recent events' });
    expect(feed.getAttribute('aria-live')).toBe('polite');
    await waitFor(() => expect(within(feed).getAllByRole('listitem').length).toBeGreaterThan(5));
    expect(asked[0]).toBeTypeOf('number');
    const text = within(feed)
      .getAllByRole('listitem')
      .map((item) => item.textContent);
    expect(text[0]).toContain('job.capturing');
    expect(text.some((line) => line!.includes('cloud.overflow_evaluated'))).toBe(true);
    expect(text.some((line) => line!.includes('Offered to builder-1'))).toBe(true);
    expect(text.some((line) => line!.includes('linux-1: Stale: no recent heartbeat'))).toBe(true);
    // Only the feed announces itself.
    expect(document.querySelectorAll('.overview [aria-live]')).toHaveLength(1);
  });

  it('says what each section would show when there is nothing', async () => {
    const fixtures = rawFixtures();
    fixtures.fleet.machines = [];
    fixtures.queue = { entries: [], shares: [] };
    fixtures.jobs = [];
    fixtures.environments = [];
    fixtures.events = [];
    show({ fixtures });
    for (const title of [
      'No machines are enrolled',
      'Nothing is being launched',
      'Nothing is running',
      'Nothing is waiting',
      'Nothing is kept alive',
      'No recent events',
    ]) {
      expect(await screen.findByText(title)).toBeTruthy();
    }
    expect(total('Queued')).toBe('0');
  });

  it('keeps the other sections when one cannot be read', async () => {
    show({
      override: (path) =>
        path === '/queue' ? { status: 500, body: { code: 'internal', message: 'queue broke' } } : undefined,
    });
    expect(await within(await waitFor(() => section('Queue'))).findByText(/queue broke/)).toBeTruthy();
    await waitFor(() => expect(rows('Machines')).toHaveLength(6));
    await waitFor(() => expect(rows('Alive').length).toBeGreaterThan(0));
    expect(total('Queued')).toBe('unknown');
    expect(total('Running')).not.toBe('unknown');
  });

  it('keeps launching and running rows when the job list cannot be read', async () => {
    show({
      override: (path) =>
        path.startsWith('/jobs?active') ? { status: 500, body: { code: 'internal', message: 'jobs broke' } } : undefined,
    });
    await waitFor(() => expect(within(section('Running')).getByText(/jobs broke/)).toBeTruthy());
    expect(rows('Running').length).toBeGreaterThan(0);
    expect(within(section('Running')).getAllByText('unknown').length).toBeGreaterThan(0);
  });

  it('works for a viewer, because it only reads', async () => {
    const { calls } = show({ role: 'viewer' });
    await loaded();
    expect(new Set(calls.map((c) => c.method))).toEqual(new Set(['GET']));
    expect(screen.queryByRole('button')).toBeNull();
  });
});
