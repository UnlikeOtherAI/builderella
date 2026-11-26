import { describe, expect, it } from 'vitest';
import { AvailabilityFlag, type EventRecord } from '../api/schemas';
import { KNOWN_EVENT_KINDS, describeEvent } from './eventText';

const names = { machine: (id: string) => (id === 'mch_1' ? 'builder-1' : id) };
const event = (kind: string, data?: unknown): EventRecord => ({
  seq: 1,
  at: 0,
  entity: 'job',
  entity_id: 'x',
  kind,
  data,
});
const say = (kind: string, data?: unknown) => describeEvent(event(kind, data), names);

/** Every kind `bella-ledger` writes, including those built from a phase or reason. */
const LEDGER_KINDS = `
  fleet.created fleet.controller_started pairing_code.created pairing_code.consumed
  principal.created principal.revoked project.created project.policy_changed snapshot.registered
  machine.enrolled machine.connected machine.disconnected machine.reconciled machine.flags_changed
  machine.local_control_changed machine.inventory_changed machine.policy_changed machine.renamed
  machine.retired machine.revoked machine.stale machine.stale_cleared machine.unreachable
  machine.unreachable_cleared machine.draining machine.draining_cleared
  job.queued job.offered job.preparing job.executing job.collecting job.cleaning job.quarantined
  job.cancel_requested job.finished
  attempt.offered attempt.accepted attempt.start_committed attempt.start_delivered
  attempt.preparing attempt.started attempt.step_changed attempt.environment_ready
  attempt.collecting attempt.artifact_published attempt.exited attempt.cleanup attempt.released
  attempt.quarantined attempt.uncertain attempt.uncertain_cleared attempt.log_truncated
  attempt.rejected attempt.expired attempt.revoked attempt.cancelled attempt.forgotten
  environment.ready environment.renewed environment.finishing environment.ended
  environment.quarantined
`
  .trim()
  .split(/\s+/);

describe('describeEvent', () => {
  it.each(LEDGER_KINDS)('has a sentence for %s, with or without data', (kind) => {
    for (const data of [undefined, null, {}, { machine_id: 'mch_1' }]) {
      const text = say(kind, data);
      expect(text, kind).toBeTruthy();
      expect(text).not.toContain('undefined');
      expect(text).not.toContain(kind);
    }
  });

  it('covers its own table with the ledger kinds', () => {
    expect(KNOWN_EVENT_KINDS.filter((kind) => !LEDGER_KINDS.includes(kind))).toEqual([]);
  });

  it('names the machine, the step, the outcome and the reason', () => {
    expect(say('attempt.offered', { machine_id: 'mch_1' })).toBe('Offered to builder-1');
    expect(say('attempt.started', { machine_id: 'mch_1' })).toBe('Started on builder-1');
    expect(say('attempt.released', { machine_id: 'mch_1' })).toBe('Resources released on builder-1');
    expect(
      say('attempt.step_changed', { event: { step: 'test', state: 'failed', exit_code: 101 } }),
    ).toBe('Step test: failed with code 101');
    expect(say('attempt.exited', { event: { outcome: { type: 'succeeded' } } })).toBe('Exited: passed');
    expect(say('job.finished', { outcome: { type: 'failed', step: 'test', exit_code: 1 } })).toBe(
      'Finished: failed (step "test" exited with code 1)',
    );
    expect(say('job.queued', { recipe: 'ci' })).toBe('Submitted and queued');
    expect(say('job.queued', { retry: true })).toBe('Queued again for another attempt');
    expect(say('job.cancel_requested')).toBe('Cancellation requested');
    expect(say('machine.disconnected', { reason: 'controller restarted' })).toBe(
      'Disconnected: controller restarted',
    );
    expect(say('machine.flags_changed', { flags: ['disk_low'] })).toBe('Reports: disk low');
  });

  it('says a controller condition was set or cleared', () => {
    expect(say('machine.stale')).toBe('Stale: no recent heartbeat');
    expect(say('machine.stale_cleared')).toBe('Reporting again; no longer stale');
    expect(say('machine.unreachable')).toBe('Unreachable');
  });

  it('returns null for a kind it does not know, so the raw name is shown', () => {
    for (const kind of ['cloud.overflow_evaluated', 'job.paused', 'machine.exploded', 'toString']) {
      expect(say(kind), kind).toBeNull();
    }
    // Flags without a controller condition are not invented.
    expect(AvailabilityFlag.known).toContain('upgrading');
    expect(say('machine.upgrading')).toBeNull();
  });
});
