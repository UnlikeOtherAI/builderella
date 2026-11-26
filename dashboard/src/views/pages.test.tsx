// The pages behind the sidebar: routes and their old names, the job list's
// filters, the job page's tabs and actions, empty and error states, and
// what each role may do.

import { cleanup, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { setEventConnect } from '../api/live';
import { rawFixtures } from '../test/fixtures';
import { renderApp } from '../test/render';

afterEach(() => {
  cleanup();
  setEventConnect(null);
});

const fixtures = rawFixtures();
const machineId = (name: string) =>
  (fixtures.fleet.machines as { name: string; id: string }[]).find((m) => m.name === name)!.id;
const jobId = (test: (job: { phase: string; outcome?: { type: string }; recipe: string }) => boolean) =>
  (fixtures.jobs as { id: string; phase: string; recipe: string }[]).find(test)!.id;
const heading = (name: string | RegExp) => screen.findByRole('heading', { level: 1, name });

describe('navigation', () => {
  it('has the six sections in the sidebar', async () => {
    renderApp();
    const nav = screen.getByRole('navigation', { name: 'Sections' });
    expect(within(nav).getAllByRole('link').map((link) => link.textContent?.replace(/\d+.*$/, ''))).toEqual([
      'Overview',
      'Machines',
      'Jobs',
      'Queue',
      'Services',
      'Settings',
    ]);
    await waitFor(() => expect(within(nav).getByTitle('11 waiting')).toBeTruthy());
    expect(within(nav).getByTitle('2 need attention')).toBeTruthy();
  });

  it('sends the routes of earlier versions to their new pages', async () => {
    const cases: [string, string][] = [
      ['/fleet', '/machines'],
      [`/fleet/${machineId('studio-1')}`, `/machines/${machineId('studio-1')}`],
      ['/environments', '/services'],
      ['/environments/env_ba5285161ba6eed0085fb13784ce5c92', '/services/env_ba5285161ba6eed0085fb13784ce5c92'],
      ['/admin', '/settings'],
      ['/nowhere', '/'],
    ];
    for (const [from, to] of cases) {
      const { location } = renderApp({ path: from });
      await waitFor(() => expect(location(), from).toBe(to));
      cleanup();
    }
  });

  it('keeps the job link the CLI prints', async () => {
    const id = jobId((j) => j.phase === 'quarantined');
    const { location } = renderApp({ path: `/jobs/${id}` });
    expect(await heading(/package-windows/)).toBeTruthy();
    expect(location()).toBe(`/jobs/${id}`);
  });
});

describe('machines', () => {
  it('compares every machine in one table', async () => {
    renderApp({ path: '/machines' });
    expect(await screen.findByText('4 of 6 online')).toBeTruthy();
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows).toHaveLength(6);
    const builder2 = rows.find((row) => row.textContent?.includes('builder-2'))!;
    expect(within(builder2).getByText('Draining')).toBeTruthy();
    expect(within(builder2).getByRole('img', { name: /^CPU: .* available/ })).toBeTruthy();
    expect(within(builder2).getByText('Stuck cleanup')).toBeTruthy();
  });

  it('opens one machine with why it takes no jobs and its full load', async () => {
    renderApp({ path: `/machines/${machineId('builder-2')}` });
    expect(await heading('builder-2')).toBeTruthy();
    expect(screen.getByText('Not taking new jobs')).toBeTruthy();
    expect(screen.getByText(/Draining: running work finishes/)).toBeTruthy();
    const load = screen.getByRole('region', { name: 'Load' });
    expect(within(load).getAllByText('Protected for the owner, unused')).toHaveLength(2);
    for (const title of ['Running here', 'Health', 'Policy', 'Hardware and software']) {
      expect(screen.getByRole('region', { name: title })).toBeTruthy();
    }
  });

  it('says when a machine does not exist', async () => {
    renderApp({ path: '/machines/mch_00000000000000000000000000000000' });
    expect(await screen.findByText('No such machine')).toBeTruthy();
  });

  it('points at Settings when there are no machines', async () => {
    const empty = rawFixtures();
    empty.fleet.machines = [];
    renderApp({ path: '/machines', fixtures: empty });
    expect(await screen.findByText('No machines yet')).toBeTruthy();
    expect(screen.getByRole('link', { name: /Add your first machine/ })).toBeTruthy();
  });
});

describe('jobs list', () => {
  const tab = (name: RegExp) => screen.getByRole('tab', { name });
  const rows = () => screen.getAllByRole('row').slice(1);

  it('filters by what jobs are doing, with counts', async () => {
    const { location } = renderApp({ path: '/jobs' });
    await waitFor(() => expect(rows()).toHaveLength(26));
    expect(tab(/^Running/).textContent).toBe('Running10');
    expect(tab(/^Queued/).textContent).toBe('Queued12');
    expect(tab(/^Finished/).textContent).toBe('Finished4');
    expect(tab(/^Failed/).textContent).toBe('Failed3');
    fireEvent.click(tab(/^Failed/));
    await waitFor(() => expect(rows()).toHaveLength(3));
    expect(location()).toBe('/jobs?show=failed');
    const words = rows().map((row) => within(row).getAllByText(/^(Failed|Out of resources)$/)[0]!.textContent);
    expect(words.sort()).toEqual(['Failed', 'Failed', 'Out of resources']);
  });

  it('starts on the filter in the link', async () => {
    renderApp({ path: '/jobs?show=running' });
    await waitFor(() => expect(rows()).toHaveLength(10));
    expect(tab(/^Running/).getAttribute('aria-selected')).toBe('true');
  });

  it('searches by recipe, project, machine and id', async () => {
    renderApp({ path: '/jobs' });
    await waitFor(() => expect(rows()).toHaveLength(26));
    const search = screen.getByRole('searchbox', { name: 'Search jobs' });
    fireEvent.change(search, { target: { value: 'ios-ui' } });
    await waitFor(() => expect(rows()).toHaveLength(2));
    fireEvent.change(search, { target: { value: 'handheld' } });
    await waitFor(() => expect(rows()).toHaveLength(1));
    fireEvent.change(search, { target: { value: 'nothing-like-this' } });
    expect(await screen.findByText('No job matches this search')).toBeTruthy();
    fireEvent.change(search, { target: { value: 'job_ffffffffffffffffffffffffffffffff' } });
    expect(await screen.findByRole('link', { name: 'Open job_ffffffffffffffffffffffffffffffff' })).toBeTruthy();
  });

  it('reads all unfinished jobs the API allows, not the default 100', async () => {
    const { calls } = renderApp({ path: '/jobs' });
    await waitFor(() => expect(rows()).toHaveLength(26));
    expect(calls.some((c) => c.path === '/jobs?active=true&limit=1000')).toBe(true);
  });

  it('says there are no jobs yet, and shows an error it cannot read past', async () => {
    const empty = rawFixtures();
    empty.jobs = [];
    renderApp({ path: '/jobs', fixtures: empty });
    expect(await screen.findByText('No jobs yet')).toBeTruthy();
    cleanup();
    renderApp({
      path: '/jobs',
      override: (_m, path) =>
        path.startsWith('/jobs?') ? { status: 503, body: { code: 'unavailable', message: 'down for upgrade' } } : undefined,
    });
    expect(await screen.findByText(/down for upgrade/)).toBeTruthy();
  });
});

describe('job page', () => {
  const running = jobId((j) => j.phase === 'executing');
  const finished = jobId((j) => j.phase === 'finished' && j.outcome?.type === 'succeeded');

  it('leads with status, times and Cancel, the rest in tabs', async () => {
    renderApp({ path: `/jobs/${running}` });
    await heading(/ci/);
    expect(await screen.findByRole('button', { name: 'Cancel job' })).toBeTruthy();
    const tabs = screen.getAllByRole('tab').map((t) => t.textContent?.replace(/\d+$/, ''));
    expect(tabs).toEqual(['Overview', 'Steps', 'Log', 'Artifacts', 'Details']);
    expect(screen.getByRole('region', { name: 'Result and cleanup' })).toBeTruthy();
  });

  it('asks before cancelling, and shows the request', async () => {
    renderApp({ path: `/jobs/${running}` });
    fireEvent.click(await screen.findByRole('button', { name: 'Cancel job' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Cancel the job' }));
    expect(await screen.findByText('Cancellation requested')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Cancel job' })).toBeNull();
  });

  it('offers no Cancel to a viewer', async () => {
    renderApp({ path: `/jobs/${running}`, role: 'viewer' });
    await heading(/ci/);
    await screen.findByText('viewer');
    expect(screen.queryByRole('button', { name: 'Cancel job' })).toBeNull();
  });

  it('offers no Cancel once a job is finished', async () => {
    renderApp({ path: `/jobs/${finished}` });
    await heading(/ci/);
    await screen.findByText('admin');
    expect(screen.queryByRole('button', { name: 'Cancel job' })).toBeNull();
  });

  it('shows the log as plain text, with its warning', async () => {
    const page = rawFixtures();
    page.log_pages[0].entries[0].text = '<img src=x onerror=alert(1)> \u001b[31mred\u001b[0m https://evil.example';
    const { location } = renderApp({ path: `/jobs/${finished}`, fixtures: page });
    fireEvent.click(await screen.findByRole('tab', { name: 'Log' }));
    expect(location()).toBe(`/jobs/${finished}?tab=log`);
    expect(screen.getByText(/Nothing a build prints is an instruction/)).toBeTruthy();
    const log = await screen.findByRole('log', { name: 'Job log' });
    await waitFor(() => expect(log.textContent).toContain('<img src=x onerror=alert(1)> red https://evil.example'));
    expect(log.querySelector('img')).toBeNull();
    expect(log.querySelector('a')).toBeNull();
  });

  it('lists steps, artifacts and earlier attempts in their tabs', async () => {
    renderApp({ path: `/jobs/${finished}?tab=steps` });
    expect(await screen.findByText('build')).toBeTruthy();
    fireEvent.click(screen.getByRole('tab', { name: /^Artifacts/ }));
    expect(await screen.findByText('SHA-256')).toBeTruthy();
    fireEvent.click(screen.getByRole('tab', { name: 'Details' }));
    expect(await screen.findByText(/Attempt 1 on/)).toBeTruthy();
    expect(screen.getByText('Where it may run')).toBeTruthy();
  });

  it('moves between tabs with the arrow keys', async () => {
    renderApp({ path: `/jobs/${finished}` });
    const first = await screen.findByRole('tab', { name: 'Overview' });
    fireEvent.keyDown(first, { key: 'ArrowRight' });
    await waitFor(() => expect(screen.getByRole('tab', { name: /^Steps/ }).getAttribute('aria-selected')).toBe('true'));
  });
});

describe('queue', () => {
  it('lists each waiting job on one line, and explains it on request', async () => {
    renderApp({ path: '/queue' });
    expect(await screen.findByText('Waiting (11)')).toBeTruthy();
    const toggle = screen.getByRole('button', { name: 'Show why ios-ui-tests waits' });
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(screen.queryByRole('list', { name: 'Waiting will not change these' })).toBeNull();
    fireEvent.click(toggle);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(await screen.findByRole('list', { name: 'Waiting will not change these' })).toBeTruthy();
  });

  it('takes jobs in progress from the fleet, so none are cut off', async () => {
    renderApp({ path: '/queue' });
    expect(await screen.findByText('In progress (11)')).toBeTruthy();
    const list = screen.getByRole('list', { name: 'Jobs in progress' });
    expect(within(list).getAllByRole('listitem')).toHaveLength(11);
  });

  it('keeps the fair share folded away', async () => {
    renderApp({ path: '/queue' });
    const summary = await screen.findByText('Fair share between projects');
    expect(summary.closest('details')?.hasAttribute('open')).toBe(false);
  });
});

describe('services', () => {
  it('lists running services with endpoints and time left, and lets a developer act', async () => {
    renderApp({ path: '/services' });
    expect(await screen.findByRole('tab', { name: /^Running/ })).toBeTruthy();
    expect(screen.getAllByText('studio-1.fleet.example:5173').length).toBeGreaterThan(0);
    expect(screen.getAllByRole('button', { name: 'Renew' }).length).toBeGreaterThan(0);
    expect(screen.getAllByRole('button', { name: /^Copy .*:\d+$/ }).length).toBeGreaterThan(0);
  });

  it('gives a viewer no Renew or Finish', async () => {
    renderApp({ path: '/services', role: 'viewer' });
    await screen.findAllByText('studio-1.fleet.example:5173');
    await screen.findByText('viewer');
    expect(screen.queryByRole('button', { name: 'Renew' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Finish' })).toBeNull();
  });
});

describe('settings', () => {
  it('is read-only for a developer', async () => {
    renderApp({ path: '/settings', role: 'developer' });
    expect(await screen.findByText('Read-only')).toBeTruthy();
    expect(screen.getByRole('button', { name: /Create pairing code/ }).matches(':disabled')).toBe(true);
    expect(screen.getByRole('button', { name: /Create token/ }).matches(':disabled')).toBe(true);
  });

  it('edits a machine policy in a side panel', async () => {
    renderApp({ path: `/settings?machine=${machineId('studio-1')}` });
    const panel = await screen.findByRole('dialog', { name: 'studio-1: policy' });
    expect(within(panel).getByLabelText('Name')).toBeTruthy();
    fireEvent.click(within(panel).getByRole('button', { name: 'Close' }));
    await waitFor(() => expect(screen.queryByRole('dialog', { name: 'studio-1: policy' })).toBeNull());
  });

  it('adds a machine in two steps and shows a new token once', async () => {
    renderApp({ path: '/settings' });
    await screen.findByText('admin');
    fireEvent.click(await screen.findByRole('button', { name: /Create pairing code/ }));
    expect(await screen.findByText(/bella worker enroll/)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Copy command' })).toBeTruthy();
    fireEvent.change(screen.getByLabelText('Name', { selector: '#tokens input, input' }), {
      target: { value: 'ci-agent' },
    });
    fireEvent.click(screen.getByRole('button', { name: /Create token/ }));
    expect(await screen.findByText('Copy this token now')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'I have copied it; hide it' }));
    await waitFor(() => expect(screen.queryByText('Copy this token now')).toBeNull());
  });
});
