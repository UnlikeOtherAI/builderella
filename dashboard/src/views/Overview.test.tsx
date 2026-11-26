import { cleanup, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { setEventConnect } from '../api/live';
import { rawFixtures } from '../test/fixtures';
import { renderApp, type Setup } from '../test/render';
import { ACTIVITY_SHOWN, NOW_SHOWN, OverviewView, QUEUE_SHOWN } from './Overview';

afterEach(() => {
  cleanup();
  setEventConnect(null);
});

const show = (setup: Setup = {}) => renderApp({ ...setup, only: <OverviewView /> });

const region = (title: string) => screen.getByRole('region', { name: title });
const items = (list: string) => within(screen.getByRole('list', { name: list })).getAllByRole('listitem');
const tile = (label: string) => screen.getByText(label, { selector: '.tile-label' }).closest('a') as HTMLElement;
const loaded = () =>
  waitFor(() => {
    expect(items('Machines')).toHaveLength(6);
    expect(items('Running and launching jobs').length).toBeGreaterThan(0);
    expect(items('Next in the queue').length).toBeGreaterThan(0);
    expect(within(tile('Failed, last hour')).queryByLabelText('loading')).toBeNull();
  });

describe('Overview', () => {
  it('leads with the numbers a newcomer needs, each linking to its page', async () => {
    show();
    await loaded();
    expect(tile('Machines online').textContent).toContain('4 of 6');
    expect(tile('Machines online').textContent).toContain('1 stale, 1 offline');
    expect(tile('Running').textContent).toMatch(/Running8\s*2 launching/);
    expect(tile('Queued').textContent).toContain('11');
    expect(tile('Queued').textContent).toContain("1 can't run");
    expect(tile('Waiting longest').textContent).toContain('lint, atlas');
    // One job failed 38 minutes ago; the others failed hours ago.
    expect(tile('Failed, last hour').textContent).toMatch(/Failed, last hour1/);
    expect(tile('Failed, last hour').getAttribute('href')).toBe('/jobs?show=failed');
    expect(tile('Running').getAttribute('href')).toBe('/jobs?show=running');
  });

  it('says what needs a person, with a link to each', async () => {
    show();
    const attention = await screen.findByRole('region', { name: 'Needs attention' });
    const text = attention.textContent ?? '';
    expect(text).toContain('builder-2 holds capacity that could not be cleaned up');
    expect(text).toContain('linux-1 has stopped reporting');
    expect(text).toContain('handheld-1 is offline (unreachable) while holding 1 job');
    expect(text).toContain("ios-ui-tests (nimbus) can't run: no compatible machine");
    expect(within(attention).getAllByRole('link', { name: /^Open/ })).toHaveLength(4);
  });

  it('shows no attention strip when nothing needs a person', async () => {
    const fixtures = rawFixtures();
    fixtures.fleet.machines = fixtures.fleet.machines.filter((m: { name: string }) => m.name === 'builder-1');
    fixtures.queue.entries = fixtures.queue.entries.filter(
      (e: { job: { queue_reason?: { code: string } } }) => e.job.queue_reason?.code !== 'no_compatible_machine',
    );
    show({ fixtures });
    await waitFor(() => expect(items('Machines')).toHaveLength(1));
    expect(screen.queryByRole('region', { name: 'Needs attention' })).toBeNull();
  });

  it('keeps every machine on one row, its flags summarised', async () => {
    show();
    await loaded();
    const row = (name: string) =>
      within(screen.getByRole('list', { name: 'Machines' })).getByRole('link', { name }).closest('li') as HTMLElement;
    expect(within(row('builder-1')).getByText('Busy')).toBeTruthy();
    expect(within(row('builder-1')).getByText('5 jobs')).toBeTruthy();
    expect(within(row('builder-2')).getByText('Draining')).toBeTruthy();
    // The most important condition, then a count of the rest.
    expect(within(row('builder-2')).getByText('Stuck cleanup')).toBeTruthy();
    expect(within(row('builder-2')).getByText('+1')).toBeTruthy();
    expect(within(row('builder-2')).queryByText('Upgrading')).toBeNull();
    expect(within(row('linux-1')).getByText('Stale')).toBeTruthy();
    expect(within(row('handheld-1')).getByText('Offline')).toBeTruthy();
    expect(within(row('infra-1')).getByText('Paused')).toBeTruthy();
    expect(within(row('infra-1')).getByText('no jobs')).toBeTruthy();
    expect(within(row('infra-1')).getByRole('link', { name: 'infra-1' }).getAttribute('href')).toMatch(
      /^\/machines\/mch_/,
    );
    expect(within(row('builder-1')).getByRole('img').getAttribute('aria-label')).toMatch(/^CPU \d+% .*memory \d+%$/);
  });

  it('lists what is launching and running, a few at a time', async () => {
    show();
    await loaded();
    const rows = items('Running and launching jobs');
    expect(rows).toHaveLength(NOW_SHOWN);
    expect(within(rows[0]!).getByText('Launching')).toBeTruthy();
    expect(within(region('Running now')).getByText(/^4 more\./)).toBeTruthy();
    expect(within(region('Running now')).getAllByText('Cleaning up').length).toBeGreaterThan(0);
    // Ids are shown by their end, so jobs made together stay distinct.
    const ids = within(region('Running now')).getAllByTitle(/^job_/).map((code) => code.textContent);
    expect(ids.every((id) => /^job_…[0-9a-f]{6}$/.test(id ?? ''))).toBe(true);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('previews the queue with why each job waits', async () => {
    show();
    await loaded();
    const rows = items('Next in the queue');
    expect(rows).toHaveLength(QUEUE_SHOWN);
    expect(within(rows[0]!).getByText('Ready to start')).toBeTruthy();
    expect(within(rows[2]!).getByText("Can't run: no compatible machine")).toBeTruthy();
    expect(within(region('Next in the queue')).getByText(/6 more waiting/)).toBeTruthy();
  });

  it('counts the services kept running', async () => {
    show();
    expect(await screen.findByText('5 kept running, 2 ready')).toBeTruthy();
  });

  it('replays recent events newest first, and shows an unknown kind by name', async () => {
    const { asked } = show();
    const feed = screen.getByRole('list', { name: 'Recent events' });
    expect(feed.getAttribute('aria-live')).toBe('polite');
    await waitFor(() => expect(within(feed).getAllByRole('listitem')).toHaveLength(ACTIVITY_SHOWN));
    expect(asked[0]).toBeTypeOf('number');
    const text = within(feed)
      .getAllByRole('listitem')
      .map((item) => item.textContent);
    expect(text[0]).toContain('job.capturing');
    expect(text.some((line) => line!.includes('cloud.overflow_evaluated'))).toBe(true);
    expect(text.some((line) => line!.includes('Offered to builder-1'))).toBe(true);
    screen.getByRole('button', { name: /Show all \d+ recent events/ }).click();
    await waitFor(() => expect(within(feed).getAllByRole('listitem').length).toBeGreaterThan(ACTIVITY_SHOWN));
    expect(
      within(feed)
        .getAllByRole('listitem')
        .some((item) => item.textContent?.includes('linux-1Stale: no recent heartbeat')),
    ).toBe(true);
    // Only the feed announces itself.
    expect(document.querySelectorAll('.overview [aria-live]')).toHaveLength(1);
  });

  it('says what each part would show when there is nothing, and what to do', async () => {
    const fixtures = rawFixtures();
    fixtures.fleet.machines = [];
    fixtures.queue = { entries: [], shares: [] };
    fixtures.jobs = [];
    fixtures.environments = [];
    fixtures.events = [];
    show({ fixtures });
    for (const title of ['No machines yet', 'Nothing is running', 'Nothing is waiting', 'No recent events']) {
      expect(await screen.findByText(title)).toBeTruthy();
    }
    expect(screen.getByRole('link', { name: /Add your first machine/ }).getAttribute('href')).toBe(
      '/settings#add-machine',
    );
    expect(screen.getByText('none kept running')).toBeTruthy();
    expect(tile('Queued').textContent).toContain('0');
    expect(tile('Waiting longest').textContent).toContain('nothing waits');
  });

  it('keeps the other parts when one cannot be read', async () => {
    show({
      override: (_method, path) =>
        path === '/queue' ? { status: 500, body: { code: 'internal', message: 'queue broke' } } : undefined,
    });
    expect(await within(region('Next in the queue')).findByText(/queue broke/)).toBeTruthy();
    await waitFor(() => expect(items('Machines')).toHaveLength(6));
    await waitFor(() => expect(tile('Queued').textContent).toContain('unknown'));
    expect(tile('Running').textContent).not.toContain('unknown');
  });

  it('keeps running rows when the job list cannot be read, and says times are missing', async () => {
    show({
      override: (_method, path) =>
        path.startsWith('/jobs?active') ? { status: 500, body: { code: 'internal', message: 'jobs broke' } } : undefined,
    });
    await waitFor(() => expect(within(region('Running now')).getByText(/jobs broke/)).toBeTruthy());
    expect(items('Running and launching jobs').length).toBeGreaterThan(0);
    // Without the job list no start time is known, and none is made up.
    expect(within(region('Running now')).getAllByText('—').length).toBeGreaterThan(0);
  });

  it('only reads, so it works the same for a viewer', async () => {
    const { calls } = show({ role: 'viewer' });
    await loaded();
    expect(new Set(calls.map((c) => c.method))).toEqual(new Set(['GET']));
  });
});
