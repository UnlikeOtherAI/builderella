import { cleanup, render, screen, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { Fixtures, QueueReason, QueueReasonCode } from '../api/schemas';
import { reasonLabel, reasonTone } from '../lib/labels';
import { rawFixtures } from '../test/fixtures';
import { QueueReasonView } from './QueueReasonView';

afterEach(cleanup);

const fixtures = Fixtures.parse(rawFixtures());
const reasonOf = (code: string) => {
  const entry = fixtures.queue.entries.find((e) => e.job.queue_reason?.code === code);
  if (!entry?.job.queue_reason) throw new Error(`no fixture job waits for ${code}`);
  return entry.job.queue_reason;
};

describe('QueueReasonView', () => {
  it('says in plain words why a job waits, with the controller explanation', () => {
    const reason = reasonOf('waiting_for_capacity');
    render(<QueueReasonView reason={reason} />);
    // Once as the heading, once on the machine that is itself short of capacity.
    expect(screen.getAllByText('Waiting for capacity')).toHaveLength(2);
    expect(screen.getByText(reason.message)).toBeTruthy();
    // The wire code itself is not what a person reads.
    expect(screen.queryByText('waiting_for_capacity')).toBeNull();
  });

  it('separates blockers that waiting can clear from those it cannot', () => {
    const reason = reasonOf('waiting_for_capacity');
    render(<QueueReasonView reason={reason} />);
    expect(screen.getByText(/6 machines checked: 2 temporary, 4 permanent/)).toBeTruthy();

    const temporary = screen.getByText('Temporary: waiting can clear these').nextElementSibling as HTMLElement;
    const permanent = screen.getByText('Permanent: waiting will not change these')
      .nextElementSibling as HTMLElement;
    expect(within(temporary).getAllByRole('listitem')).toHaveLength(2);
    expect(within(permanent).getAllByRole('listitem')).toHaveLength(4);
    expect(within(temporary).getByText('builder-2')).toBeTruthy();
    expect(within(temporary).getByText('Machine draining')).toBeTruthy();
    expect(within(permanent).getByText('handheld-1')).toBeTruthy();
    expect(within(permanent).getByText('Machine too small')).toBeTruthy();
    expect(within(permanent).queryByText('builder-2')).toBeNull();
  });

  it('has a plain-language label for every reason code the controller sends', () => {
    for (const code of QueueReasonCode.known) {
      expect(reasonLabel(code), code).not.toBe(code);
      expect(reasonLabel(code), code).not.toContain('_');
    }
  });

  it('names heat, upgrades, environment limits and placement choices as themselves', () => {
    const codes = new Set<string>();
    for (const entry of fixtures.queue.entries) {
      const reason = entry.job.queue_reason;
      if (!reason) continue;
      codes.add(reason.code);
      for (const blocker of reason.blockers) codes.add(blocker.code);
    }
    for (const code of ['overheated', 'machine_upgrading', 'environment_quota', 'placement_constraint']) {
      expect(codes.has(code), `no fixture uses ${code}`).toBe(true);
    }

    render(<QueueReasonView reason={reasonOf('overheated')} />);
    // Once as the heading, once on the machine that is too hot.
    expect(screen.getAllByText('Machine too hot')).toHaveLength(2);
    cleanup();

    render(<QueueReasonView reason={reasonOf('environment_quota')} />);
    expect(screen.getByText('Project at its environment limit')).toBeTruthy();
    expect(screen.queryByText(/machines? checked/)).toBeNull();
    cleanup();

    render(<QueueReasonView reason={reasonOf('reserved_for_older_job')} />);
    expect(screen.getByText('Machine being upgraded')).toBeTruthy();
    cleanup();

    // A machine the job did not ask for stays excluded however long it waits.
    render(<QueueReasonView reason={reasonOf('machine_paused')} />);
    const permanent = screen.getByText('Permanent: waiting will not change these')
      .nextElementSibling as HTMLElement;
    expect(within(permanent).getByText('Not the machine, pool or platform the job asked for')).toBeTruthy();
    expect(reasonTone('placement_constraint')).toBe('bad');
  });

  it('shows no machine list when nothing is blocking', () => {
    render(<QueueReasonView reason={reasonOf('schedulable')} />);
    expect(screen.getByText('Ready to start')).toBeTruthy();
    expect(screen.queryByText(/machines? checked/)).toBeNull();
  });

  it('shows a reason code it does not know as the raw value', () => {
    const reason = QueueReason.parse({
      code: 'waiting_for_license_seat',
      message: 'All 3 compiler license seats are in use.',
      blockers: [
        { machine: 'builder-1', code: 'license_seat_busy', message: 'seat held by another job', permanent: false },
      ],
    });
    render(<QueueReasonView reason={reason} />);
    expect(screen.getByText('waiting_for_license_seat')).toBeTruthy();
    expect(screen.getByText('All 3 compiler license seats are in use.')).toBeTruthy();
    expect(screen.getByText('license_seat_busy')).toBeTruthy();
    expect(screen.getByText(/1 machine checked: 1 temporary, 0 permanent/)).toBeTruthy();
  });
});
