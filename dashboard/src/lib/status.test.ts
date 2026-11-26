import { describe, expect, it } from 'vitest';
import {
  AvailabilityFlag,
  Fixtures,
  JobPhase,
  QueueReason,
  QueueReasonCode,
  type JobView,
  type MachineView,
  type Outcome,
} from '../api/schemas';
import { rawFixtures } from '../test/fixtures';
import { reasonTone } from './labels';
import {
  JOB_WORDS,
  MACHINE_WORDS,
  isFailure,
  isOnline,
  jobFlags,
  jobGroup,
  jobStatus,
  machineFlags,
  machineStatus,
  phaseStatus,
  queueLine,
} from './status';

const data = () => Fixtures.parse(rawFixtures());
const machine = (name: string): MachineView => {
  const found = data().fleet.machines.find((m) => m.name === name);
  if (!found) throw new Error(`no fixture machine ${name}`);
  return found;
};
const job = (phase: string, outcome?: Outcome, extra: Partial<JobView> = {}): JobView => ({
  ...data().jobs[0]!,
  phase,
  outcome,
  ...extra,
});

describe('machine status', () => {
  it('gives each fixture machine the word a person would expect', () => {
    expect(machineStatus(machine('builder-1')).label).toBe('Busy');
    expect(machineStatus(machine('builder-2')).label).toBe('Draining');
    expect(machineStatus(machine('studio-1'))).toMatchObject({ label: 'Paused', precise: 'Paused by owner' });
    expect(machineStatus(machine('linux-1')).label).toBe('Stale');
    expect(machineStatus(machine('handheld-1'))).toMatchObject({ label: 'Offline', precise: 'Unreachable' });
    expect(machineStatus(machine('infra-1'))).toMatchObject({ label: 'Paused', precise: 'Builds disabled' });
  });

  it('maps every lifecycle and contact state', () => {
    const idle = { ...machine('builder-1'), attempts: [] };
    expect(machineStatus(idle).label).toBe('Idle');
    expect(machineStatus({ ...idle, connected: false })).toMatchObject({ label: 'Offline', precise: 'Disconnected' });
    expect(machineStatus({ ...idle, flags: ['sleeping'] })).toMatchObject({ label: 'Offline', precise: 'Sleeping' });
    expect(machineStatus({ ...idle, lifecycle: 'retired' }).label).toBe('Retired');
    expect(machineStatus({ ...idle, lifecycle: 'revoked' })).toMatchObject({ label: 'Retired', precise: 'Revoked' });
    expect(machineStatus({ ...idle, lifecycle: 'enrolling' }).label).toBe('Offline');
    expect(machineStatus({ ...idle, lifecycle: 'probing' }).precise).toContain('Probing');
    expect(machineStatus({ ...idle, local_control: { paused: false, draining: true } }).label).toBe('Draining');
    // A flag the dashboard does not know leaves the word alone and shows as itself.
    const odd = { ...idle, flags: ['moonlighting'] };
    expect(machineStatus(odd).label).toBe('Idle');
    expect(machineFlags(odd).map((f) => f.label)).toEqual(['moonlighting']);
  });

  it('shows a lifecycle it does not know raw', () => {
    const status = machineStatus({ ...machine('builder-1'), lifecycle: 'hibernating' });
    expect(status).toMatchObject({ key: 'unknown', label: 'hibernating', tone: 'neutral' });
  });

  it('uses only the words of the vocabulary, each with a meaning', () => {
    const words = new Set(Object.values(MACHINE_WORDS).map((w) => w.label));
    for (const m of data().fleet.machines) expect(words.has(machineStatus(m).label)).toBe(true);
    for (const word of Object.values(MACHINE_WORDS)) expect(word.meaning.length).toBeGreaterThan(10);
  });

  it('counts paused and draining machines as online, offline and stale ones as not', () => {
    expect(isOnline(machineStatus(machine('studio-1')))).toBe(true);
    expect(isOnline(machineStatus(machine('builder-2')))).toBe(true);
    expect(isOnline(machineStatus(machine('linux-1')))).toBe(false);
    expect(isOnline(machineStatus(machine('handheld-1')))).toBe(false);
  });

  it('lists conditions the word does not already say, most important first', () => {
    expect(machineFlags(machine('builder-2')).map((f) => f.flag)).toEqual(['quarantined', 'upgrading']);
    expect(machineFlags(machine('builder-2'))[0]).toMatchObject({ label: 'Stuck cleanup', tone: 'bad' });
    expect(machineFlags(machine('linux-1')).map((f) => f.flag)).toEqual(['overheated', 'disk_low']);
    expect(machineFlags(machine('handheld-1')).map((f) => f.flag)).toEqual(['on_battery']);
    expect(machineFlags(machine('studio-1')).map((f) => f.flag)).toEqual(['pressure_limited']);
    expect(machineFlags(machine('infra-1'))).toEqual([]);
    for (const flag of AvailabilityFlag.known) {
      const chip = machineFlags({ ...machine('builder-1'), flags: [flag] })[0];
      if (chip) expect(chip.label, flag).not.toContain('_');
    }
  });
});

describe('job status', () => {
  it('maps every phase onto the vocabulary', () => {
    const expected: Record<string, string> = {
      capturing: 'Queued',
      queued: 'Queued',
      offered: 'Launching',
      preparing: 'Launching',
      executing: 'Running',
      collecting: 'Running',
      cleaning: 'Cleaning up',
      quarantined: 'Cleaning up',
    };
    for (const phase of JobPhase.known) {
      if (phase === 'finished') continue;
      expect(jobStatus(job(phase)).label, phase).toBe(expected[phase]);
      expect(phaseStatus(phase).precise.length).toBeGreaterThan(0);
    }
  });

  it('maps every outcome of a finished job', () => {
    const cases: [Outcome, string][] = [
      [{ type: 'succeeded' }, 'Succeeded'],
      [{ type: 'failed', step: 'test', exit_code: 1 }, 'Failed'],
      [{ type: 'rejected', detail: 'no such recipe' }, 'Failed'],
      [{ type: 'cancelled' }, 'Cancelled'],
      [{ type: 'timed_out', step: 'build' }, 'Timed out'],
      [{ type: 'infra_interrupted', detail: 'machine rebooted' }, 'Interrupted'],
      [{ type: 'resource_exhausted', detail: 'out of memory' }, 'Out of resources'],
    ];
    for (const [outcome, label] of cases) {
      expect(jobStatus(job('finished', outcome)).label, outcome.type).toBe(label);
    }
    expect(jobStatus(job('finished', { type: 'rejected', detail: 'no such recipe' })).precise).toContain(
      'no such recipe',
    );
    expect(new Set(Object.values(JOB_WORDS).map((w) => w.label)).size).toBe(10);
  });

  it('shows a phase or an outcome it does not know raw', () => {
    expect(jobStatus(job('levitating'))).toMatchObject({ key: 'unknown', label: 'levitating' });
    expect(jobStatus(job('finished', { type: 'unknown', raw: 'vetoed' }))).toMatchObject({
      key: 'unknown',
      label: 'vetoed',
    });
    expect(jobStatus(job('finished')).label).toBe('Finished');
  });

  it('keeps the phase while cleanup runs, even when the result is known', () => {
    expect(jobStatus(job('cleaning', { type: 'succeeded' })).label).toBe('Cleaning up');
  });

  it('flags stuck cleanup, a pending cancel and a machine out of contact', () => {
    const jobs = data().jobs;
    const stuck = jobs.find((j) => j.phase === 'quarantined')!;
    expect(jobFlags(stuck).map((f) => f.label)).toContain('Stuck cleanup');
    const cancelling = jobs.find((j) => j.cancel_requested)!;
    expect(jobFlags(cancelling).map((f) => f.label)).toContain('Cancelling');
    const lost = jobs.find((j) => j.attempts.some((a) => a.uncertain))!;
    expect(jobFlags(lost).map((f) => f.label)).toContain('Out of contact');
    expect(jobFlags(jobs.find((j) => j.phase === 'queued')!)).toEqual([]);
  });

  it('sorts jobs into the list filters', () => {
    expect(jobGroup(job('capturing'))).toBe('queued');
    expect(jobGroup(job('queued'))).toBe('queued');
    for (const phase of ['offered', 'preparing', 'executing', 'collecting', 'cleaning', 'quarantined']) {
      expect(jobGroup(job(phase)), phase).toBe('running');
    }
    expect(jobGroup(job('finished', { type: 'succeeded' }))).toBe('finished');
    expect(isFailure(job('finished', { type: 'failed' }))).toBe(true);
    expect(isFailure(job('finished', { type: 'timed_out' }))).toBe(true);
    expect(isFailure(job('finished', { type: 'succeeded' }))).toBe(false);
    expect(isFailure(job('finished', { type: 'cancelled' }))).toBe(false);
    // Not finished yet, so not a failure yet.
    expect(isFailure(job('cleaning', { type: 'failed' }))).toBe(false);
  });
});

describe('queue line', () => {
  it("leads with Can't run when waiting will not help", () => {
    const entries = data().queue.entries;
    const reason = (code: string) => entries.find((e) => e.job.queue_reason?.code === code)!.job.queue_reason;
    expect(queueLine(reason('no_compatible_machine'))).toMatchObject({
      headline: "Can't run: no compatible machine",
      stuck: true,
      tone: 'bad',
    });
    expect(queueLine(reason('waiting_for_capacity'))).toMatchObject({
      headline: 'Waiting for capacity',
      stuck: false,
    });
    expect(queueLine(reason('waiting_for_capacity')).message).toContain('7.2 more CPUs');
    expect(queueLine(reason('schedulable')).headline).toBe('Ready to start');
  });

  it("says Can't run for exactly the reasons waiting cannot clear", () => {
    for (const code of QueueReasonCode.known) {
      const line = queueLine(QueueReason.parse({ code, message: 'm', blockers: [] }));
      expect(line.stuck, code).toBe(reasonTone(code) === 'bad');
      expect(line.headline.startsWith("Can't run"), code).toBe(line.stuck);
      expect(line.headline, code).not.toContain('_');
    }
  });

  it('shows an unknown code raw, and a missing reason honestly', () => {
    expect(queueLine(QueueReason.parse({ code: 'license_wait', message: 'm', blockers: [] })).headline).toBe(
      'license_wait',
    );
    expect(queueLine(undefined)).toMatchObject({ headline: 'Waiting', message: 'The controller gave no reason.' });
  });
});
