// Environment lease arithmetic, mirroring `bella_core::lease::Lease`.
//
// An environment ends at the earliest of three limits: the idle timeout
// (counted from the last real interaction), the renewable lease, and the
// maximum lifetime that no renewal can pass.

import type { Lease } from '../api/schemas';

export type LeaseLimit = 'idle' | 'lease' | 'maximum';

export interface LeaseStatus {
  /** When the environment ends if nothing else happens. */
  deadline: number;
  /** Which limit produces that deadline. */
  limit: LeaseLimit;
  remainingMs: number;
  ended: boolean;
  idleDeadline: number;
  leaseDeadline: number;
  hardDeadline: number;
}

export function leaseDeadlines(lease: Lease) {
  return {
    idleDeadline: lease.last_activity_at + lease.spec.idle_timeout_secs * 1000,
    leaseDeadline: lease.lease_until,
    hardDeadline: lease.created_at + lease.spec.max_lifetime_secs * 1000,
  };
}

/**
 * Where a lease stands at `now`. `serverDeadline` is the controller's own
 * `effective_deadline`; when given it is the deadline shown, and the limit
 * is the one whose own deadline is nearest to it.
 *
 * When limits coincide the stricter one is named: the maximum lifetime
 * before the lease, the lease before the idle timeout. A lease clamped to
 * the maximum lifetime therefore reads as "maximum", which is the limit a
 * renewal cannot move.
 */
export function leaseStatus(lease: Lease, now: number, serverDeadline?: number): LeaseStatus {
  const d = leaseDeadlines(lease);
  const computed = Math.min(d.idleDeadline, d.leaseDeadline, d.hardDeadline);
  const deadline = serverDeadline ?? computed;

  const candidates: [LeaseLimit, number][] = [
    ['maximum', d.hardDeadline],
    ['lease', d.leaseDeadline],
    ['idle', d.idleDeadline],
  ];
  let limit: LeaseLimit = 'maximum';
  let best = Number.POSITIVE_INFINITY;
  for (const [name, at] of candidates) {
    const distance = Math.abs(at - deadline);
    if (distance < best) {
      best = distance;
      limit = name;
    }
  }

  return {
    deadline,
    limit,
    remainingMs: Math.max(deadline - now, 0),
    ended: now >= deadline,
    ...d,
  };
}

/**
 * The deadline a renewal at `now` would produce, or `null` when the lease
 * has already ended and cannot be renewed. Used to say up front whether
 * renewing can help; the controller's response is what is shown afterwards.
 */
export function deadlineAfterRenewal(lease: Lease, now: number): LeaseStatus | null {
  if (leaseStatus(lease, now).ended) return null;
  const hard = lease.created_at + lease.spec.max_lifetime_secs * 1000;
  const renewed: Lease = {
    ...lease,
    last_activity_at: now,
    lease_until: Math.min(now + lease.spec.lease_secs * 1000, hard),
  };
  return leaseStatus(renewed, now);
}

/** `1:04:09`, `12:30`, `0:07`. For a ticking countdown. */
export function formatCountdown(ms: number): string {
  const total = Math.max(0, Math.ceil(ms / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const ss = String(seconds).padStart(2, '0');
  if (hours > 0) return `${hours}:${String(minutes).padStart(2, '0')}:${ss}`;
  return `${minutes}:${ss}`;
}
