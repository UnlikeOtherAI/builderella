import { cleanup, render, screen, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { Fixtures, QueueReason } from '../api/schemas';
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
