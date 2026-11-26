// The fetch layer. Every call goes to same-origin `/api/v1` with the bearer
// token, and every successful response is parsed with its zod schema before
// it is returned.

import { z } from 'zod';
import {
  ApiErrorBody,
  ArtifactView,
  EnvironmentView,
  FleetOverview,
  JobView,
  LogPage,
  MachineView,
  PairingCodeResponse,
  ProjectView,
  QueueView,
  TokenResponse,
  WhoAmI,
  type CreatePairingCodeRequest,
  type CreateTokenRequest,
  type ProjectPolicy,
  type UpdateMachineRequest,
} from './schemas';

export const API_PREFIX = '/api/v1';

// ------------------------------------------------------------------ errors

/** The controller answered with an error status. */
export class ApiFailure extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = 'ApiFailure';
  }
}

/** A response did not have the shape this dashboard was built against. */
export class SchemaDriftError extends Error {
  constructor(
    /** What was being read, e.g. `GET /fleet`. */
    readonly where: string,
    /** One line per mismatch, with its path in the response. */
    readonly issues: string,
  ) {
    super(`${where}: response did not match the expected shape\n${issues}`);
    this.name = 'SchemaDriftError';
  }
}

/** The request never produced a response. */
export class NetworkError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'NetworkError';
  }
}

/** Parse `value` with `schema`, or throw a `SchemaDriftError` naming `where`. */
export function parseOrThrow<S extends z.ZodType>(schema: S, value: unknown, where: string): z.output<S> {
  const result = schema.safeParse(value);
  if (!result.success) throw new SchemaDriftError(where, z.prettifyError(result.error));
  return result.data;
}

// -------------------------------------------------------------- mock switch

/**
 * Mock mode serves the committed fixtures instead of calling a controller.
 * `?mock=1` turns it on for one page load; `VITE_MOCK=1` builds it in.
 */
export function detectMock(search: string, buildFlag: string | undefined): boolean {
  return buildFlag === '1' || new URLSearchParams(search).get('mock') === '1';
}

export const isMock = detectMock(
  typeof window === 'undefined' ? '' : window.location.search,
  import.meta.env.VITE_MOCK as string | undefined,
);

// ------------------------------------------------------------------- token

const TOKEN_KEY = 'bella.token';
const listeners = new Set<() => void>();
let memoryToken: string | null = null;
let loaded = false;

function readStored(): string | null {
  try {
    return window.localStorage.getItem(TOKEN_KEY);
  } catch {
    // Storage can be blocked; the token then lasts for this page only.
    return null;
  }
}

export function getToken(): string | null {
  if (!loaded) {
    memoryToken = readStored();
    loaded = true;
  }
  return memoryToken;
}

function store(token: string | null): void {
  memoryToken = token;
  loaded = true;
  try {
    if (token === null) window.localStorage.removeItem(TOKEN_KEY);
    else window.localStorage.setItem(TOKEN_KEY, token);
  } catch {
    // See readStored.
  }
  for (const listener of listeners) listener();
}

export const setToken = (token: string) => store(token);
/** Sign out, or forget a token the controller rejected. */
export const clearToken = () => store(null);

export function subscribeToken(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

// --------------------------------------------------------------- transport

export interface RawResponse {
  status: number;
  /** Parsed JSON body, or `undefined` when there was none. */
  body: unknown;
  /** True when a body was present but was not JSON. */
  notJson?: boolean;
}

export type Transport = (
  method: string,
  path: string,
  body: unknown,
  token: string | null,
  signal?: AbortSignal,
) => Promise<RawResponse>;

const fetchTransport: Transport = async (method, path, body, token, signal) => {
  const headers: Record<string, string> = { accept: 'application/json' };
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  let response: Response;
  try {
    response = await fetch(API_PREFIX + path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
      cache: 'no-store',
    });
  } catch (error) {
    if (signal?.aborted) throw error;
    throw new NetworkError(
      `Could not reach the controller (${error instanceof Error ? error.message : String(error)}).`,
    );
  }
  const text = await response.text();
  if (text === '') return { status: response.status, body: undefined };
  try {
    return { status: response.status, body: JSON.parse(text) as unknown };
  } catch {
    return { status: response.status, body: undefined, notJson: true };
  }
};

let transport: Transport = fetchTransport;

/** Replace the transport: the mock layer in mock mode, a stub in tests. */
export function setTransport(next: Transport): void {
  transport = next;
}

interface RequestOptions {
  body?: unknown;
  signal?: AbortSignal;
  /** Use this token instead of the stored one (the sign-in check). */
  token?: string;
}

export async function request<S extends z.ZodType>(
  method: string,
  path: string,
  schema: S,
  options: RequestOptions = {},
): Promise<z.output<S>> {
  const where = `${method} ${path.split('?')[0]}`;
  const usingStored = options.token === undefined;
  const token = usingStored ? getToken() : (options.token ?? null);
  const response = await transport(method, path, options.body, token, options.signal);

  if (response.status < 200 || response.status >= 300) {
    const parsed = ApiErrorBody.safeParse(response.body);
    const failure = parsed.success
      ? new ApiFailure(response.status, parsed.data.code, parsed.data.message)
      : new ApiFailure(
          response.status,
          `http_${response.status}`,
          `The controller answered ${where} with HTTP ${response.status} and no readable error.`,
        );
    // A rejected stored token is forgotten, which returns the app to sign-in.
    if (response.status === 401 && usingStored) clearToken();
    throw failure;
  }
  if (response.notJson) throw new SchemaDriftError(where, 'the response body is not JSON');
  return parseOrThrow(schema, response.body, where);
}

function query(params: Record<string, string | number | boolean | null | undefined>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== '') search.set(key, String(value));
  }
  const text = search.toString();
  return text ? `?${text}` : '';
}

const seg = encodeURIComponent;

export interface ListJobsQuery {
  project?: string;
  active?: boolean;
  limit?: number;
}

export interface LogQuery {
  step?: string;
  cursor?: string;
  tail?: boolean;
  limit_bytes?: number;
}

export const api = {
  whoami: (signal?: AbortSignal, token?: string) => request('GET', '/whoami', WhoAmI, { signal, token }),
  fleet: (signal?: AbortSignal) => request('GET', '/fleet', FleetOverview, { signal }),
  queue: (signal?: AbortSignal) => request('GET', '/queue', QueueView, { signal }),
  jobs: (q: ListJobsQuery, signal?: AbortSignal) =>
    request('GET', `/jobs${query({ ...q })}`, z.array(JobView), { signal }),
  job: (id: string, signal?: AbortSignal) => request('GET', `/jobs/${seg(id)}`, JobView, { signal }),
  cancelJob: (id: string) => request('POST', `/jobs/${seg(id)}/cancel`, JobView),
  logs: (id: string, q: LogQuery, signal?: AbortSignal) =>
    request('GET', `/jobs/${seg(id)}/logs${query({ ...q })}`, LogPage, { signal }),
  artifacts: (id: string, signal?: AbortSignal) =>
    request('GET', `/jobs/${seg(id)}/artifacts`, z.array(ArtifactView), { signal }),
  environments: (signal?: AbortSignal) =>
    request('GET', '/environments', z.array(EnvironmentView), { signal }),
  environment: (id: string, signal?: AbortSignal) =>
    request('GET', `/environments/${seg(id)}`, EnvironmentView, { signal }),
  renewEnvironment: (id: string) => request('POST', `/environments/${seg(id)}/renew`, EnvironmentView),
  finishEnvironment: (id: string) => request('POST', `/environments/${seg(id)}/finish`, EnvironmentView),
  projects: (signal?: AbortSignal) => request('GET', '/projects', z.array(ProjectView), { signal }),
  putProject: (name: string, policy: ProjectPolicy) =>
    request('PUT', `/projects/${seg(name)}`, ProjectView, { body: policy }),
  updateMachine: (id: string, body: UpdateMachineRequest) =>
    request('PATCH', `/machines/${seg(id)}`, MachineView, { body }),
  createPairingCode: (body: CreatePairingCodeRequest) =>
    request('POST', '/enroll/pairing-codes', PairingCodeResponse, { body }),
  createToken: (body: CreateTokenRequest) => request('POST', '/tokens', TokenResponse, { body }),
};

// ------------------------------------------------------- describing errors

export interface ErrorText {
  title: string;
  detail: string;
}

/** What went wrong, in words a person can act on. */
export function describeError(error: unknown): ErrorText {
  if (error instanceof SchemaDriftError) {
    return {
      title: 'The controller sent data this dashboard does not understand',
      detail:
        `${error.where} did not match the expected shape, so nothing from it is shown ` +
        `rather than risk showing wrong data. The dashboard and the controller are probably ` +
        `different versions.\n${error.issues}`,
    };
  }
  if (error instanceof ApiFailure) {
    const titles: Record<number, string> = {
      401: 'The token was not accepted',
      403: 'Not allowed',
      404: 'Not found',
      409: 'Not possible in the current state',
      502: 'The controller is not answering',
      503: 'The controller is unavailable',
      504: 'The controller is not answering',
    };
    return {
      title: titles[error.status] ?? `The controller refused the request (HTTP ${error.status})`,
      detail: `${error.message} (${error.code})`,
    };
  }
  if (error instanceof NetworkError) {
    return { title: 'The controller could not be reached', detail: error.message };
  }
  return {
    title: 'Something went wrong in the dashboard',
    detail: error instanceof Error ? error.message : String(error),
  };
}
