// Mock mode: an in-memory stand-in for the controller, seeded from
// `src/mocks/fixtures.json` (generated from the real Rust types). It answers
// the same routes with the same shapes, and applies the write operations to
// its own copy so that forms and actions can be tried end to end.
//
// It is a development aid, not a model of the scheduler: nothing here moves
// on its own, and the rules it applies to writes are approximations.

import type { RawResponse, Transport } from './client';
import {
  Fixtures,
  type EnvironmentView,
  type JobView,
  type LogPage,
  type MachinePolicy,
  type ProjectPolicy,
} from './schemas';
import { leaseStatus } from '../lib/lease';
import { managedCeiling } from '../lib/policy';

const TIME_KEYS = new Set([
  'at',
  'server_time',
  'created_at',
  'updated_at',
  'offered_at',
  'started_at',
  'exited_at',
  'released_at',
  'finished_at',
  'sampled_at',
  'last_seen_at',
  'last_activity_at',
  'lease_until',
  'effective_deadline',
  'expires_at',
  'boost_until',
]);

/** Shift every timestamp so the fixtures' fixed "now" becomes the real now. */
export function rebaseTimes(value: unknown, shiftMs: number): unknown {
  if (Array.isArray(value)) return value.map((item) => rebaseTimes(item, shiftMs));
  if (value !== null && typeof value === 'object') {
    const out: Record<string, unknown> = {};
    for (const [key, child] of Object.entries(value)) {
      out[key] =
        TIME_KEYS.has(key) && typeof child === 'number' ? child + shiftMs : rebaseTimes(child, shiftMs);
    }
    return out;
  }
  return value;
}

const ok = (body: unknown): RawResponse => ({ status: 200, body: structuredClone(body) });
const fail = (status: number, code: string, message: string): RawResponse => ({
  status,
  body: { code, message },
});

function randomHex(length: number): string {
  const bytes = new Uint8Array(length / 2);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

const withoutSpec = (job: JobView): JobView => ({ ...job, spec: undefined });

export interface Mock {
  transport: Transport;
  /** Event stream: replays the fixture events once, then stays open. */
  connect: (after: number | null, signal: AbortSignal) => AsyncIterable<string>;
}

export interface MockOptions {
  /** Role reported by `/whoami`, to try the read-only administration view. */
  role?: string;
  /** Simulated latency per request. */
  delayMs?: number;
  now?: () => number;
}

export function createMock(rawFixtures: unknown, options: MockOptions = {}): Mock {
  const now = options.now ?? (() => Date.now());
  const delayMs = options.delayMs ?? 120;
  const fixedNow = (rawFixtures as { fleet?: { server_time?: unknown } }).fleet?.server_time;
  const shift = typeof fixedNow === 'number' ? now() - fixedNow : 0;
  const state = Fixtures.parse(rebaseTimes(rawFixtures, shift));
  if (options.role) state.whoami.role = options.role;

  const isAdmin = () => state.whoami.role === 'admin';
  const forbidden = () => fail(403, 'forbidden', 'This action needs the admin role.');
  const notFound = (what: string) => fail(404, 'not_found', `${what} not found`);

  function logs(job: JobView, params: URLSearchParams): LogPage {
    const pages = state.log_pages;
    const finished = job.phase === 'finished';
    const cursor = params.get('cursor');
    const step = params.get('step');
    // A job that is still running has produced only the first page so far.
    const available = finished ? pages.length : Math.min(pages.length, 1);
    let index = 0;
    if (cursor) index = pages.findIndex((page) => page.next_cursor === cursor) + 1 || available;
    else if (params.get('tail') === 'true') index = Math.max(available - 1, 0);
    const page = job.attempts.length > 0 && index < available ? pages[index] : undefined;
    if (!page) {
      return { entries: [], next_cursor: cursor ?? '', truncated: false, complete: finished };
    }
    return {
      entries: step ? page.entries.filter((entry) => entry.step === step) : page.entries,
      next_cursor: page.next_cursor,
      truncated: finished && page.truncated,
      complete: finished && index === available - 1,
    };
  }

  function cancel(job: JobView): JobView {
    job.cancel_requested = true;
    job.updated_at = now();
    if (job.phase === 'queued' || job.phase === 'capturing') {
      // Nothing was running, so there is nothing to clean up.
      job.phase = 'finished';
      job.outcome = { type: 'cancelled' };
      job.queue_reason = undefined;
      state.queue.entries = state.queue.entries.filter((entry) => entry.job.id !== job.id);
    } else if (job.cleanup === 'not_started') {
      job.cleanup = 'requested';
    }
    return job;
  }

  function renew(env: EnvironmentView): RawResponse {
    if (env.state !== 'ready' && env.state !== 'starting') {
      return fail(409, 'invalid_state', `the environment is ${env.state} and cannot be renewed`);
    }
    const at = now();
    const status = leaseStatus(env.lease, at);
    if (status.ended) {
      return fail(
        409,
        'invalid_state',
        status.limit === 'maximum'
          ? 'the environment reached its maximum lifetime and cannot be renewed'
          : `the lease already ended (${status.limit === 'idle' ? 'idle timeout' : 'lease expired'})`,
      );
    }
    env.lease.last_activity_at = at;
    env.lease.lease_until = Math.min(at + env.lease.spec.lease_secs * 1000, status.hardDeadline);
    env.effective_deadline = leaseStatus(env.lease, at).deadline;
    return ok(env);
  }

  function updateMachine(id: string, body: unknown): RawResponse {
    const machine = state.fleet.machines.find((m) => m.id === id);
    if (!machine) return notFound('machine');
    const patch = (body ?? {}) as {
      name?: unknown;
      policy?: MachinePolicy;
      drain?: unknown;
      retire?: unknown;
    };
    if (patch.name !== undefined) {
      if (typeof patch.name !== 'string' || patch.name.trim() === '') {
        return fail(400, 'bad_request', 'the machine name must not be empty');
      }
      machine.name = patch.name.trim();
    }
    if (patch.policy !== undefined) {
      const policy = patch.policy;
      if (policy.foreground_cpu_percent > 100 || policy.foreground_memory_percent > 100) {
        return fail(400, 'bad_request', 'a foreground share cannot exceed 100%');
      }
      machine.policy = { ...policy, revision: machine.policy.revision + 1 };
      const ceiling = managedCeiling(machine.policy, machine.inventory.hardware);
      machine.managed_ceiling = {
        ...machine.managed_ceiling,
        cpu_milli: ceiling.cpu_milli,
        memory_bytes: ceiling.memory_bytes,
      };
      const disabled = machine.flags.filter((flag) => flag !== 'builds_disabled');
      machine.flags = policy.accept_builds ? disabled : [...disabled, 'builds_disabled'];
    }
    if (typeof patch.drain === 'boolean') {
      machine.local_control.draining = patch.drain;
      const rest = machine.flags.filter((flag) => flag !== 'draining');
      machine.flags = patch.drain ? ['draining', ...rest] : rest;
    }
    if (patch.retire === true) {
      machine.lifecycle = 'retired';
      machine.connected = false;
    }
    return ok(machine);
  }

  function putProject(name: string, body: unknown): RawResponse {
    const policy = body as ProjectPolicy;
    if (typeof policy?.weight !== 'number' || policy.weight < 1) {
      return fail(400, 'bad_request', 'weight must be at least 1');
    }
    let project = state.projects.find((p) => p.name === name);
    if (project) {
      project.policy = policy;
    } else {
      project = { id: `prj_${randomHex(32)}`, name, policy, created_at: now() };
      state.projects.push(project);
    }
    for (const share of state.queue.shares) {
      if (share.name === name) share.weight = policy.weight;
    }
    return ok(project);
  }

  function route(method: string, fullPath: string, body: unknown): RawResponse {
    const [path = '', search = ''] = fullPath.split('?');
    const params = new URLSearchParams(search);
    const parts = path.split('/').filter(Boolean).map(decodeURIComponent);
    const [a, b, c] = parts;
    const key = `${method} ${a ?? ''}`;

    if (key === 'GET whoami') return ok(state.whoami);
    if (key === 'GET fleet') return ok({ ...state.fleet, server_time: now() });
    if (key === 'GET queue') return ok(state.queue);
    if (key === 'GET projects' && !b) return ok(state.projects);
    if (key === 'PUT projects' && b) return isAdmin() ? putProject(b, body) : forbidden();

    if (key === 'GET machines' && b) {
      const machine = state.fleet.machines.find((m) => m.id === b);
      return machine ? ok(machine) : notFound('machine');
    }
    if (key === 'PATCH machines' && b) return isAdmin() ? updateMachine(b, body) : forbidden();

    if (key === 'GET jobs' && !b) {
      let jobs = state.jobs;
      const project = params.get('project');
      if (project) jobs = jobs.filter((job) => job.project_name === project);
      if (params.get('active') === 'true') jobs = jobs.filter((job) => job.phase !== 'finished');
      const limit = Number(params.get('limit') ?? '0');
      const sorted = [...jobs].sort((x, y) => y.updated_at - x.updated_at);
      return ok((limit > 0 ? sorted.slice(0, limit) : sorted).map(withoutSpec));
    }
    if ((a === 'jobs' && b) || (a === 'environments' && b)) {
      if (a === 'jobs') {
        const job = state.jobs.find((j) => j.id === b);
        if (!job) return notFound('job');
        if (method === 'GET' && !c) return ok(job);
        if (method === 'GET' && c === 'logs') return ok(logs(job, params));
        if (method === 'GET' && c === 'artifacts') {
          return ok(state.artifacts.filter((artifact) => artifact.job_id === job.id));
        }
        if (method === 'POST' && c === 'cancel') {
          if (state.whoami.role === 'viewer') {
            return fail(403, 'forbidden', 'A viewer cannot cancel jobs.');
          }
          if (job.phase === 'finished') {
            return fail(409, 'invalid_state', 'the job already finished');
          }
          return ok(cancel(job));
        }
      } else {
        const env = state.environments.find((e) => e.id === b);
        if (!env) return notFound('environment');
        if (method === 'GET' && !c) return ok(env);
        if (method === 'POST' && state.whoami.role === 'viewer') {
          return fail(403, 'forbidden', 'A viewer cannot change environments.');
        }
        if (method === 'POST' && c === 'renew') return renew(env);
        if (method === 'POST' && c === 'finish') {
          if (env.state === 'ended' || env.state === 'finishing') {
            return fail(409, 'invalid_state', `the environment is already ${env.state}`);
          }
          env.state = 'finishing';
          env.ended_reason = 'finish requested from the dashboard';
          return ok(env);
        }
      }
    }
    if (key === 'GET environments' && !b) return ok(state.environments);

    if (method === 'POST' && a === 'enroll' && b === 'pairing-codes') {
      if (!isAdmin()) return forbidden();
      const ttl = (body as { ttl_secs?: unknown } | null)?.ttl_secs;
      const code = `${randomHex(4)}-${randomHex(4)}`.toUpperCase();
      return ok({
        code,
        expires_at: now() + (typeof ttl === 'number' && ttl > 0 ? ttl : 600) * 1000,
        enrollment: state.pairing_code.enrollment.replace(/code=.*$/, `code=${code}`),
      });
    }
    if (key === 'POST tokens') {
      if (!isAdmin()) return forbidden();
      const name = (body as { name?: unknown } | null)?.name;
      if (typeof name !== 'string' || name.trim() === '') {
        return fail(400, 'bad_request', 'the token needs a name');
      }
      return ok({ principal_id: `prn_${randomHex(32)}`, token: `bella_mock_${randomHex(40)}` });
    }
    return fail(404, 'not_found', `no route for ${method} ${path}`);
  }

  const transport: Transport = async (method, path, body, _token, signal) => {
    if (delayMs > 0) await new Promise((resolve) => setTimeout(resolve, delayMs));
    if (signal?.aborted) throw new DOMException('aborted', 'AbortError');
    return route(method, path, body === undefined ? undefined : structuredClone(body));
  };

  async function* connect(after: number | null, signal: AbortSignal): AsyncGenerator<string> {
    yield '';
    for (const event of state.events) {
      if (after === null || event.seq > after) {
        yield `id: ${event.seq}\nevent: ${event.kind}\ndata: ${JSON.stringify(event)}\n\n`;
      }
    }
    // Stay connected, as an idle controller would.
    await new Promise<void>((resolve) => {
      if (signal.aborted) resolve();
      else signal.addEventListener('abort', () => resolve(), { once: true });
    });
  }

  return { transport, connect };
}
