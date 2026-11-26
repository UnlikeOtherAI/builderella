import { describe, expect, it } from 'vitest';
import { Fixtures, type Lease } from '../api/schemas';
import { rawFixtures } from '../test/fixtures';
import { deadlineAfterRenewal, formatCountdown, leaseStatus } from './lease';

// The same terms as the tests in `bella_core::lease`.
const spec = { idle_timeout_secs: 900, lease_secs: 1800, max_lifetime_secs: 14_400 };
const fresh: Lease = { spec, created_at: 0, last_activity_at: 0, lease_until: 1_800_000 };

describe('leaseStatus', () => {
  it('ends an unused environment by the idle timeout', () => {
    const before = leaseStatus(fresh, 899_000);
    expect(before).toMatchObject({ limit: 'idle', deadline: 900_000, ended: false, remainingMs: 1000 });
    expect(leaseStatus(fresh, 900_000)).toMatchObject({ ended: true, remainingMs: 0 });
  });

  it('ends a used environment by its lease', () => {
    // Touched every minute: the idle timer keeps moving, the lease does not.
    const touched: Lease = { ...fresh, last_activity_at: 1_740_000 };
    expect(leaseStatus(touched, 1_750_000)).toMatchObject({
      limit: 'lease',
      deadline: 1_800_000,
      remainingMs: 50_000,
    });
  });

  it('names the maximum lifetime when the lease is clamped to it', () => {
    const old: Lease = {
      spec,
      created_at: 0,
      last_activity_at: 14_000_000,
      // A renewal at 14 000 s would run to 15 800 s; it is clamped to 14 400 s.
      lease_until: 14_400_000,
    };
    const status = leaseStatus(old, 14_100_000);
    expect(status).toMatchObject({ limit: 'maximum', deadline: 14_400_000, remainingMs: 300_000 });
    expect(status.hardDeadline).toBe(14_400_000);
  });

  it("shows the controller's deadline and picks the limit nearest to it", () => {
    const status = leaseStatus(fresh, 0, 1_800_500);
    expect(status.deadline).toBe(1_800_500);
    expect(status.limit).toBe('lease');
  });

  it('never reports negative time left', () => {
    expect(leaseStatus(fresh, 5_000_000)).toMatchObject({ ended: true, remainingMs: 0 });
  });

  it('agrees with the controller on every fixture environment', () => {
    const { environments } = Fixtures.parse(rawFixtures());
    expect(environments.length).toBeGreaterThan(0);
    const limits = new Set<string>();
    for (const environment of environments) {
      const status = leaseStatus(environment.lease, 0);
      expect([environment.id, status.deadline]).toEqual([environment.id, environment.effective_deadline]);
      if (environment.state === 'ready' || environment.state === 'starting') limits.add(status.limit);
    }
    // The live environments show each kind of limit once.
    expect([...limits].sort()).toEqual(['idle', 'lease', 'maximum']);
  });
});

describe('deadlineAfterRenewal', () => {
  it('extends the lease and resets the idle timer', () => {
    const renewed = deadlineAfterRenewal(fresh, 600_000);
    // min(idle 600+900, lease 600+1800, hard 14 400) seconds
    expect(renewed).toMatchObject({ deadline: 1_500_000, limit: 'idle' });
  });

  it('cannot pass the maximum lifetime', () => {
    const late: Lease = { ...fresh, last_activity_at: 13_900_000, lease_until: 14_400_000 };
    expect(deadlineAfterRenewal(late, 14_000_000)).toMatchObject({
      deadline: 14_400_000,
      limit: 'maximum',
    });
  });

  it('is impossible once the lease has ended', () => {
    expect(deadlineAfterRenewal(fresh, 1_000_000)).toBeNull();
  });
});

describe('formatCountdown', () => {
  it('counts down in clock form and stops at zero', () => {
    expect(formatCountdown(3_849_000)).toBe('1:04:09');
    expect(formatCountdown(750_000)).toBe('12:30');
    expect(formatCountdown(6_500)).toBe('0:07');
    expect(formatCountdown(0)).toBe('0:00');
    expect(formatCountdown(-4000)).toBe('0:00');
  });
});
