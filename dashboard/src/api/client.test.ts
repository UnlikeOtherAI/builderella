import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { z } from 'zod';
import { rawFixtures } from '../test/fixtures';
import {
  ApiFailure,
  SchemaDriftError,
  api,
  clearToken,
  describeError,
  detectMock,
  getToken,
  request,
  setToken,
  setTransport,
  type RawResponse,
} from './client';
import { createMock } from './mock';
import { Fixtures, WhoAmI } from './schemas';

const respond = (response: RawResponse) => {
  const calls: { method: string; path: string; body: unknown; token: string | null }[] = [];
  setTransport(async (method, path, body, token) => {
    calls.push({ method, path, body, token });
    return response;
  });
  return calls;
};

beforeEach(() => clearToken());
afterEach(() => clearToken());

describe('request', () => {
  it('sends the stored token and returns the parsed body', async () => {
    setToken('secret-token');
    const calls = respond({ status: 200, body: rawFixtures().whoami });
    const who = await request('GET', '/whoami', WhoAmI);
    expect(who.role).toBe('admin');
    expect(calls).toEqual([{ method: 'GET', path: '/whoami', body: undefined, token: 'secret-token' }]);
  });

  it('fails loudly when the body has the wrong shape', async () => {
    const broken = rawFixtures().whoami;
    delete broken.role;
    respond({ status: 200, body: broken });
    const error = await request('GET', '/whoami', WhoAmI).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(SchemaDriftError);
    expect((error as SchemaDriftError).where).toBe('GET /whoami');
    expect((error as SchemaDriftError).issues).toContain('role');
    expect(describeError(error).title).toMatch(/does not understand/);
  });

  it('fails loudly when a success body is not JSON', async () => {
    respond({ status: 200, body: undefined, notJson: true });
    await expect(request('GET', '/whoami', WhoAmI)).rejects.toBeInstanceOf(SchemaDriftError);
  });

  it('turns an error body into an ApiFailure with its code', async () => {
    respond({ status: 409, body: { code: 'invalid_state', message: 'the job already finished' } });
    const error = await api.cancelJob('job_x').catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ApiFailure);
    expect(error).toMatchObject({ status: 409, code: 'invalid_state', message: 'the job already finished' });
    expect(describeError(error).detail).toContain('the job already finished');
  });

  it('forgets the stored token when the controller rejects it', async () => {
    setToken('revoked');
    respond({ status: 401, body: { code: 'unauthorized', message: 'unknown token' } });
    await expect(api.fleet()).rejects.toMatchObject({ status: 401 });
    expect(getToken()).toBeNull();
  });

  it('keeps the stored token when only a sign-in candidate is rejected', async () => {
    setToken('still-good');
    const calls = respond({ status: 401, body: { code: 'unauthorized', message: 'unknown token' } });
    await expect(api.whoami(undefined, 'typo')).rejects.toMatchObject({ status: 401 });
    expect(calls[0]?.token).toBe('typo');
    expect(getToken()).toBe('still-good');
  });

  it('encodes path segments and omits empty query values', async () => {
    const calls = respond({ status: 200, body: [] });
    await api.jobs({ project: 'a b', active: true, limit: undefined });
    await request('GET', `/jobs/${encodeURIComponent('x/y')}`, z.unknown());
    expect(calls.map((c) => c.path)).toEqual(['/jobs?project=a+b&active=true', '/jobs/x%2Fy']);
  });
});

describe('detectMock', () => {
  it('is on for ?mock=1 or the build flag, and off otherwise', () => {
    expect(detectMock('?mock=1', undefined)).toBe(true);
    expect(detectMock('?a=b&mock=1', undefined)).toBe(true);
    expect(detectMock('', '1')).toBe(true);
    expect(detectMock('?mock=0', undefined)).toBe(false);
    expect(detectMock('', undefined)).toBe(false);
    expect(detectMock('?mockery=1', '0')).toBe(false);
  });
});

describe('mock mode', () => {
  const NOW = 2_000_000_000_000;
  const start = (role?: string) => {
    const mock = createMock(rawFixtures(), { delayMs: 0, now: () => NOW, role });
    setTransport(mock.transport);
    return mock;
  };

  it('answers every read with a body the schemas accept', async () => {
    start();
    const fixtures = Fixtures.parse(rawFixtures());
    const fleet = await api.fleet();
    expect(fleet.machines).toHaveLength(6);
    // Timestamps are moved so the fixtures' fixed "now" is the present.
    expect(fleet.server_time).toBe(NOW);
    expect((await api.queue()).entries).toHaveLength(fixtures.queue.entries.length);
    expect(await api.projects()).toHaveLength(4);
    expect(await api.environments()).toHaveLength(fixtures.environments.length);
    expect((await api.whoami()).role).toBe('admin');

    const list = await api.jobs({});
    expect(list).toHaveLength(fixtures.jobs.length);
    expect(list.every((job) => job.spec === undefined)).toBe(true);
    expect((await api.jobs({ active: true })).every((job) => job.phase !== 'finished')).toBe(true);
    for (const job of list) {
      const detail = await api.job(job.id);
      expect(detail.spec).toBeDefined();
      await api.artifacts(job.id);
      await api.logs(job.id, {});
    }
    for (const environment of fixtures.environments) await api.environment(environment.id);
    await expect(api.job(`job_${'0'.repeat(32)}`)).rejects.toMatchObject({ status: 404, code: 'not_found' });
  });

  it('pages a finished log by cursor to its truncated end', async () => {
    start();
    const failed = Fixtures.parse(rawFixtures()).jobs.find((j) => j.outcome?.type === 'failed');
    if (!failed) throw new Error('fixture has no failed job');
    const first = await api.logs(failed.id, {});
    expect(first).toMatchObject({ complete: false, truncated: false });
    const second = await api.logs(failed.id, { cursor: first.next_cursor });
    expect(second).toMatchObject({ complete: true, truncated: true });
    const third = await api.logs(failed.id, { cursor: second.next_cursor });
    expect(third.entries).toEqual([]);
    expect((await api.logs(failed.id, { step: 'test' })).entries.every((e) => e.step === 'test')).toBe(true);
  });

  it('applies a cancel as a request, not as a release', async () => {
    start();
    const running = Fixtures.parse(rawFixtures()).jobs.find((j) => j.phase === 'executing');
    if (!running) throw new Error('fixture has no executing job');
    const after = await api.cancelJob(running.id);
    expect(after.cancel_requested).toBe(true);
    expect(after.phase).toBe('executing');
    expect(after.cleanup).toBe('requested');
    expect(after.outcome ?? null).toBeNull();
  });

  it('renews a live lease and refuses an ended one', async () => {
    start();
    const { environments } = Fixtures.parse(rawFixtures());
    const ready = environments.find((e) => e.state === 'ready');
    const ended = environments.find((e) => e.state === 'ended');
    if (!ready || !ended) throw new Error('fixture environments are missing');
    const renewed = await api.renewEnvironment(ready.id);
    expect(renewed.lease.last_activity_at).toBe(NOW);
    expect(renewed.effective_deadline).toBeGreaterThan(NOW);
    await expect(api.renewEnvironment(ended.id)).rejects.toMatchObject({ status: 409, code: 'invalid_state' });
    expect((await api.finishEnvironment(ready.id)).state).toBe('finishing');
  });

  it('recomputes the ceiling when a machine policy is saved', async () => {
    start();
    const machine = (await api.fleet()).machines[0];
    if (!machine) throw new Error('fixture has no machine');
    const saved = await api.updateMachine(machine.id, {
      name: 'builder-one',
      policy: { ...machine.policy, foreground_cpu_percent: 50 },
    });
    expect(saved.name).toBe('builder-one');
    expect(saved.policy.revision).toBe(machine.policy.revision + 1);
    expect(saved.managed_ceiling.cpu_milli).toBe(machine.inventory.hardware.logical_cpus * 500);
    expect((await api.updateMachine(machine.id, { drain: true })).flags).toContain('draining');
    expect((await api.updateMachine(machine.id, { retire: true })).lifecycle).toBe('retired');
  });

  it('creates pairing codes and tokens for an admin only', async () => {
    start();
    const code = await api.createPairingCode({ profile: 'dedicated_builder', ttl_secs: 120 });
    expect(code.expires_at).toBe(NOW + 120_000);
    expect(code.enrollment).toContain(code.code);
    expect((await api.createToken({ name: 'ci', role: 'developer' })).token.length).toBeGreaterThan(20);
    expect((await api.putProject('atlas', { weight: 300, cloud: 'manual', source_lan_only: false, allow_urgent: true })).policy.weight).toBe(300);

    start('viewer');
    expect((await api.whoami()).role).toBe('viewer');
    await expect(api.createToken({ name: 'x', role: 'admin' })).rejects.toMatchObject({ status: 403 });
    await expect(api.createPairingCode({ profile: 'dedicated_builder' })).rejects.toMatchObject({ status: 403 });
    const anyMachine = (await api.fleet()).machines[0];
    await expect(api.updateMachine(anyMachine?.id ?? '', { drain: true })).rejects.toMatchObject({ status: 403 });
  });
});
