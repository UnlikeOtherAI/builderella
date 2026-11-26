import { describe, expect, it } from 'vitest';
import { z } from 'zod';
import { rawFixtures } from '../test/fixtures';
import {
  ArtifactView,
  AvailabilityFlag,
  Enforcement,
  EnvironmentState,
  EnvironmentView,
  EventRecord,
  Fixtures,
  FleetOverview,
  JobPhase,
  JobView,
  LogPage,
  MachinePolicy,
  MachineView,
  Outcome,
  PairingCodeResponse,
  ProjectView,
  QueueView,
  TokenResponse,
  WhoAmI,
} from './schemas';

describe('fixtures generated from the Rust types', () => {
  it('parse as one document', () => {
    const result = Fixtures.safeParse(rawFixtures());
    expect(result.success ? '' : z.prettifyError(result.error)).toBe('');
  });

  it('parse with the schema of each response', () => {
    const raw = rawFixtures();
    const parts: [string, z.ZodType, unknown][] = [
      ['whoami', WhoAmI, raw.whoami],
      ['fleet', FleetOverview, raw.fleet],
      ['queue', QueueView, raw.queue],
      ['jobs', z.array(JobView), raw.jobs],
      ['log_pages', z.array(LogPage), raw.log_pages],
      ['artifacts', z.array(ArtifactView), raw.artifacts],
      ['environments', z.array(EnvironmentView), raw.environments],
      ['projects', z.array(ProjectView), raw.projects],
      ['events', z.array(EventRecord), raw.events],
      ['pairing_code', PairingCodeResponse, raw.pairing_code],
      ['token', TokenResponse, raw.token],
    ];
    for (const [name, schema, value] of parts) {
      const result = schema.safeParse(value);
      expect(result.success ? '' : `${name}: ${z.prettifyError(result.error)}`).toBe('');
    }
  });

  it('cover every availability flag, job phase, enforcement level and environment state', () => {
    const data = Fixtures.parse(rawFixtures());
    const flags = new Set(data.fleet.machines.flatMap((m) => m.flags));
    expect([...AvailabilityFlag.known].filter((f) => !flags.has(f))).toEqual([]);

    const phases = new Set(data.jobs.map((j) => j.phase));
    expect([...JobPhase.known].filter((p) => !phases.has(p))).toEqual([]);

    const levels = new Set(
      data.fleet.machines.flatMap((m) => [m.enforcement.cpu, m.enforcement.memory, m.enforcement.disk]),
    );
    expect([...Enforcement.known].filter((l) => !levels.has(l))).toEqual([]);

    const states = new Set(data.environments.map((e) => e.state));
    expect([...EnvironmentState.known].filter((s) => !states.has(s))).toEqual([]);
  });

  it('include the situations the views must tell apart', () => {
    const { jobs, queue } = Fixtures.parse(rawFixtures());
    const failed = jobs.find((j) => j.outcome?.type === 'failed');
    expect(failed?.outcome).toMatchObject({ type: 'failed', step: 'test', exit_code: 101 });
    expect(jobs.some((j) => j.attempts.some((a) => a.uncertain))).toBe(true);
    expect(jobs.some((j) => j.phase === 'quarantined' && j.cleanup === 'quarantined')).toBe(true);
    expect(jobs.some((j) => j.attempts.length > 1)).toBe(true);
    expect(jobs.some((j) => j.outcome?.type === 'succeeded' && j.cleanup !== 'released')).toBe(true);
    expect(jobs.some((j) => j.outcome?.type === 'succeeded' && j.cleanup === 'released')).toBe(true);
    // Lists carry no spec; the detail view does.
    expect(queue.entries.every((e) => e.job.spec === undefined)).toBe(true);
    expect(jobs.every((j) => j.spec !== undefined)).toBe(true);
    expect(queue.entries.some((e) => e.job.queue_reason?.blockers.some((b) => b.permanent))).toBe(true);
    expect(queue.entries.some((e) => e.job.queue_reason?.blockers.some((b) => !b.permanent))).toBe(true);
    expect(new Set(queue.entries.map((e) => e.job.priority)).size).toBeGreaterThanOrEqual(4);
  });
});

describe('a response with the wrong shape is rejected', () => {
  const machine = () => rawFixtures().fleet.machines[0];

  it('when a required field is missing', () => {
    const broken = machine();
    delete broken.reserved;
    const result = MachineView.safeParse(broken);
    expect(result.success).toBe(false);
    expect(result.success ? [] : result.error.issues.map((i) => i.path.join('.'))).toContain('reserved');
  });

  it('when a number arrives as a string', () => {
    const broken = machine();
    broken.managed_ceiling.cpu_milli = '28800';
    expect(MachineView.safeParse(broken).success).toBe(false);
  });

  it('when a field was renamed', () => {
    const broken = rawFixtures().fleet;
    broken.machine_list = broken.machines;
    delete broken.machines;
    expect(FleetOverview.safeParse(broken).success).toBe(false);
  });

  it('when an identifier has the wrong prefix', () => {
    const broken = rawFixtures().jobs[0];
    broken.id = broken.project_id;
    expect(JobView.safeParse(broken).success).toBe(false);
  });

  it('when a known tagged variant lacks its fields', () => {
    expect(Outcome.safeParse({ type: 'resource_exhausted' }).success).toBe(false);
    expect(Outcome.safeParse({ type: 'failed', exit_code: 'one' }).success).toBe(false);
    expect(Outcome.safeParse({ detail: 'no type' }).success).toBe(false);
  });

  it('when a boolean is missing from a log page', () => {
    const broken = rawFixtures().log_pages[0];
    delete broken.complete;
    expect(LogPage.safeParse(broken).success).toBe(false);
  });
});

describe('what a newer server may add is accepted', () => {
  it('keeps an unknown enum value as the raw string', () => {
    const raw = rawFixtures().fleet.machines[0];
    raw.flags = ['defragmenting'];
    raw.lifecycle = 'hibernating';
    raw.enforcement.cpu = 'quantum';
    const parsed = MachineView.parse(raw);
    expect(parsed.flags).toEqual(['defragmenting']);
    expect(parsed.lifecycle).toBe('hibernating');
    expect(parsed.enforcement.cpu).toBe('quantum');
  });

  it('turns an unknown tagged variant into `unknown` with its name', () => {
    expect(Outcome.parse({ type: 'preempted', by: 'job_x' })).toEqual({ type: 'unknown', raw: 'preempted' });
    const job = rawFixtures().jobs[0];
    job.spec.placement = { type: 'region', region: 'eu' };
    expect(JobView.parse(job).spec?.placement).toEqual({ type: 'unknown', raw: 'region' });
  });

  it('ignores unknown fields, and accepts null for an absent optional one', () => {
    const job = rawFixtures().jobs[0];
    job.added_in_a_later_version = { anything: true };
    job.outcome = null;
    job.environment_id = null;
    const parsed = JobView.parse(job);
    expect(parsed.outcome ?? null).toBeNull();
    expect('added_in_a_later_version' in parsed).toBe(false);
  });

  it('keeps unknown policy fields, because the policy is sent back whole', () => {
    const policy = rawFixtures().fleet.machines[0].policy;
    policy.quiet_hours = '22:00-07:00';
    expect(MachinePolicy.parse(policy)).toMatchObject({ quiet_hours: '22:00-07:00' });
  });

  it('reads u64::MAX (no disk cap) as a number', () => {
    const parsed = FleetOverview.parse(rawFixtures().fleet);
    expect(parsed.machines[0]?.managed_ceiling.disk_bytes).toBeGreaterThanOrEqual(2 ** 63);
  });
});
