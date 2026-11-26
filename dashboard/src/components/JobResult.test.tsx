import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { Fixtures, type JobView } from '../api/schemas';
import { summarizeJob } from '../lib/job';
import { rawFixtures } from '../test/fixtures';
import { JobResult, JobResultChips } from './JobResult';

afterEach(cleanup);

const { jobs } = Fixtures.parse(rawFixtures());
const find = (test: (job: JobView) => boolean, what: string) => {
  const job = jobs.find(test);
  if (!job) throw new Error(`no fixture job that ${what}`);
  return job;
};
const settled = find((j) => j.outcome?.type === 'succeeded' && j.cleanup === 'released', 'passed and settled');
const cleaning = find(
  (j) => j.outcome?.type === 'succeeded' && j.cleanup === 'processes_stopped',
  'passed with cleanup pending',
);
const quarantined = find(
  (j) => j.outcome?.type === 'succeeded' && j.cleanup === 'quarantined',
  'passed with cleanup quarantined',
);
const failed = find((j) => j.outcome?.type === 'failed', 'failed');

describe('JobResult', () => {
  it('shows a settled pass as settled', () => {
    const { container } = render(<JobResult job={settled} />);
    expect(screen.getByText('Passed')).toBeTruthy();
    expect(screen.getByText('Released')).toBeTruthy();
    expect(screen.getByText('Settled')).toBeTruthy();
    expect(screen.queryByText('Not settled')).toBeNull();
    expect(container.querySelector('.result-settled')).not.toBeNull();
    expect(screen.getByText(/Cleanup is confirmed and its resources are released/)).toBeTruthy();
  });

  it('does not let a pass with pending cleanup look settled', () => {
    const { container } = render(<JobResult job={cleaning} />);
    // The result is still a pass...
    expect(screen.getByText('Passed')).toBeTruthy();
    // ...but the resources are reported separately, and as not free.
    expect(screen.getByText('Processes stopped')).toBeTruthy();
    expect(screen.getByText('Not settled')).toBeTruthy();
    expect(screen.queryByText('Settled')).toBeNull();
    expect(screen.queryByText('Released')).toBeNull();
    expect(container.querySelector('.result-open')).not.toBeNull();
    expect(screen.getByText(/Cleanup is still in progress, so its resources are not free yet/)).toBeTruthy();
  });

  it('does not let a pass with quarantined cleanup look settled', () => {
    const { container } = render(<JobResult job={quarantined} />);
    expect(screen.getByText('Passed')).toBeTruthy();
    expect(screen.getByText('Quarantined')).toBeTruthy();
    expect(screen.getByText('Not settled')).toBeTruthy();
    expect(container.querySelector('.result-open')).not.toBeNull();
    expect(screen.getByText(/Cleanup could not be verified, so its capacity stays held/)).toBeTruthy();
  });

  it('names the failing step and its exit code', () => {
    render(<JobResult job={failed} />);
    expect(screen.getByText('Failed')).toBeTruthy();
    expect(screen.getByText('step "test" exited with code 101')).toBeTruthy();
    expect(screen.getByText('Settled')).toBeTruthy();
  });

  it('gives the three passes three different summaries', () => {
    const headlines = [settled, cleaning, quarantined].map((job) => summarizeJob(job).headline);
    expect(new Set(headlines).size).toBe(3);
    expect(summarizeJob(settled).settled).toBe(true);
    expect(summarizeJob(cleaning).settled).toBe(false);
    expect(summarizeJob(quarantined).settled).toBe(false);
  });

  it('treats an unknown cleanup state as resources still held', () => {
    const odd: JobView = { ...settled, cleanup: 'awaiting_audit' };
    render(<JobResult job={odd} />);
    expect(screen.getByText('awaiting_audit')).toBeTruthy();
    expect(screen.getByText('Not settled')).toBeTruthy();
  });
});

describe('JobResultChips', () => {
  it('adds a cleanup marker to a pass that is not settled', () => {
    render(<JobResultChips job={settled} />);
    expect(screen.getByText('Passed')).toBeTruthy();
    expect(screen.queryByText(/cleanup/)).toBeNull();
    cleanup();

    render(<JobResultChips job={cleaning} />);
    expect(screen.getByText('Passed')).toBeTruthy();
    expect(screen.getByText('cleanup pending')).toBeTruthy();
    cleanup();

    render(<JobResultChips job={quarantined} />);
    expect(screen.getByText('cleanup quarantined')).toBeTruthy();
  });
});
