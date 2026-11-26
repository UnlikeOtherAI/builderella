// The whole fleet on one page: machines, what is being launched, what is
// running, what waits, what is kept alive, and what just happened. Every
// section reads only, and fails on its own.

import { useId, type ReactNode } from 'react';
import { Link } from 'react-router';
import type { UseQueryResult } from '@tanstack/react-query';
import { useEnvironments, useFleet, useJobs, useNow, useQueue } from '../api/hooks';
import { useActivity, useLive } from '../api/live';
import type {
  EnvironmentView,
  EventRecord,
  FleetOverview,
  JobView,
  MachineView,
  QueueView,
} from '../api/schemas';
import { Chip, Empty, ErrorPanel, Ident, Loadable, Loading } from '../components/ui';
import { describeEvent } from '../lib/eventText';
import {
  formatBytes,
  formatCpu,
  formatDuration,
  formatResources,
  formatTimestamp,
  shortId,
} from '../lib/format';
import {
  enforcementDetail,
  enforcementLabel,
  environmentStateLabel,
  environmentStateTone,
  flagLabel,
  lastSeenText,
  lifecycleLabel,
  osLabel,
  phaseLabel,
  priorityLabel,
  reasonLabel,
  reasonTone,
} from '../lib/labels';
import {
  LIVENESS,
  aliveEnvironments,
  attemptPhaseLabel,
  attemptRows,
  liveness,
  livenessCounts,
  type AttemptRow,
  type Since,
} from '../lib/overview';
import { Countdown } from './Environments';

/** Queued jobs listed here; the rest are on the Queue page. */
export const QUEUE_SHOWN = 10;
/** The most the API returns; held attempts older than this lose their times. */
const JOB_LIMIT = 1000;

export function OverviewView() {
  const fleet = useFleet();
  const jobs = useJobs({ active: true, limit: JOB_LIMIT });
  const queue = useQueue();
  const environments = useEnvironments();
  const activity = useActivity();

  const rows = fleet.data ? attemptRows(fleet.data, jobs.data, activity.phases) : undefined;
  const alive = environments.data ? aliveEnvironments(environments.data) : undefined;

  return (
    <div className="overview">
      <h1>Overview</h1>
      <Totals
        fleet={fleet}
        launching={rows?.launching.length}
        running={rows?.running.length}
        queue={queue}
        environments={environments}
        alive={alive?.length}
      />
      {fleet.data?.reconciling ? (
        <p className="note note-warn" role="status">
          The controller is reconciling with its workers after a restart and is not starting new work
          until that is done.
        </p>
      ) : null}

      <Section title="Machines" count={fleet.data?.machines.length}>
        <Loadable query={fleet} what="the machines">
          {(data) => <Machines fleet={data} />}
        </Loadable>
      </Section>

      <Section title="Launching" count={rows?.launching.length}>
        <Attempts fleet={fleet} jobs={jobs} rows={rows?.launching} launching />
      </Section>

      <Section title="Running" count={rows?.running.length}>
        <Attempts fleet={fleet} jobs={jobs} rows={rows?.running} launching={false} />
      </Section>

      <Section title="Queue" count={queue.data?.entries.length}>
        <Loadable query={queue} what="the queue">
          {(data) => <QueueTable queue={data} readAt={queue.dataUpdatedAt} />}
        </Loadable>
      </Section>

      <Section title="Alive" count={alive?.length}>
        <Loadable query={environments} what="environments">
          {(data) => <Alive environments={aliveEnvironments(data)} />}
        </Loadable>
      </Section>

      <Section title="Activity">
        <ActivityFeed fleet={fleet.data} jobs={jobs.data} queue={queue.data} />
      </Section>
    </div>
  );
}

function Section({ title, count, children }: { title: string; count?: number; children: ReactNode }) {
  const id = useId();
  return (
    <section aria-labelledby={id}>
      <h2 id={id}>
        {title}
        {count === undefined ? '' : ` (${count})`}
      </h2>
      {children}
    </section>
  );
}

// ------------------------------------------------------------------ totals

function Total({
  label,
  query,
  value,
  detail,
}: {
  label: string;
  /** The query the number is counted from. */
  query: UseQueryResult<unknown>;
  value: ReactNode;
  detail?: string;
}) {
  const unknown = query.data === undefined;
  const failed = unknown && query.error !== null;
  return (
    <div className="total">
      <dt>{label}</dt>
      <dd>
        {unknown ? <span className="muted">{failed ? 'unknown' : '…'}</span> : value}
        {!unknown && detail ? <span className="total-detail"> {detail}</span> : null}
      </dd>
    </div>
  );
}

function Totals({
  fleet,
  queue,
  environments,
  launching,
  running,
  alive,
}: {
  fleet: UseQueryResult<FleetOverview>;
  queue: UseQueryResult<QueueView>;
  environments: UseQueryResult<EnvironmentView[]>;
  launching: number | undefined;
  running: number | undefined;
  alive: number | undefined;
}) {
  const machines = fleet.data?.machines ?? [];
  const counts = livenessCounts(machines);
  const trouble = (['stale', 'unreachable', 'disconnected'] as const)
    .filter((key) => counts[key] > 0)
    .map((key) => `${counts[key]} ${key}`)
    .join(', ');
  return (
    <dl className="totals" aria-label="Fleet totals">
      <Total
        label="Machines connected"
        query={fleet}
        value={`${counts.connected} of ${machines.length}`}
        detail={trouble}
      />
      <Total label="Queued" query={queue} value={queue.data?.entries.length} />
      <Total label="Launching" query={fleet} value={launching} />
      <Total label="Running" query={fleet} value={running} />
      <Total label="Environments alive" query={environments} value={alive} />
    </dl>
  );
}

// ---------------------------------------------------------------- machines

function Meter({ reserved, ceiling, text }: { reserved: number; ceiling: number; text: string }) {
  const over = reserved > ceiling;
  const share = ceiling > 0 ? Math.min(reserved / ceiling, 1) : reserved > 0 ? 1 : 0;
  return (
    <span className={over ? 'meter meter-over' : 'meter'}>
      <span className="meter-text">
        {text}
        {over ? <strong> over ceiling</strong> : null}
      </span>
      <span className="meter-bar" aria-hidden="true">
        <span style={{ width: `${share * 100}%` }} />
      </span>
    </span>
  );
}

function Enforcement({ machine }: { machine: MachineView }) {
  const { cpu, memory, verified } = machine.enforcement;
  const soft = [cpu, memory].some((level) => level !== 'hard');
  const text =
    cpu === memory
      ? enforcementLabel(cpu)
      : `CPU ${enforcementLabel(cpu)}, memory ${enforcementLabel(memory)}`;
  const detail = [...new Set([cpu, memory])]
    .map((level) => `${enforcementLabel(level)}: ${enforcementDetail(level)}`)
    .join('. ');
  return (
    <>
      <span className={soft ? 'enforcement-tag enforcement-soft' : 'enforcement-tag'} title={detail}>
        {text}
      </span>
      {verified ? null : <span className="sub">not self-tested</span>}
    </>
  );
}

function MachineState({ machine }: { machine: MachineView }) {
  // Staleness and reachability are the liveness column's to say.
  const flags: string[] = machine.flags.filter((flag) => flag !== 'stale' && flag !== 'unreachable');
  // The owner's local controls count even before a flag reports them.
  if (machine.local_control.paused && !flags.includes('user_paused')) flags.push('user_paused');
  if (machine.local_control.draining && !flags.includes('draining')) flags.push('draining');
  if (machine.lifecycle === 'ready' && flags.length === 0) return <span className="muted">Ready</span>;
  return (
    <span className="chips">
      {machine.lifecycle !== 'ready' ? <Chip tone="warn">{lifecycleLabel(machine.lifecycle)}</Chip> : null}
      {flags.map((flag) => (
        <Chip key={flag} tone="warn">
          {flagLabel(flag)}
        </Chip>
      ))}
    </span>
  );
}

function Machines({ fleet }: { fleet: FleetOverview }) {
  const now = useNow(1000);
  if (fleet.machines.length === 0) {
    return (
      <Empty title="No machines are enrolled">
        Every enrolled machine appears here with its liveness, load and running jobs. An
        administrator can add one under Administration.
      </Empty>
    );
  }
  return (
    <div className="table-wrap">
      <table className="stack compact">
        <thead>
          <tr>
            <th scope="col">Machine</th>
            <th scope="col">Liveness</th>
            <th scope="col">State</th>
            <th scope="col">Enforcement</th>
            <th scope="col">CPU reserved / ceiling</th>
            <th scope="col">Memory reserved / ceiling</th>
            <th scope="col">Running now</th>
          </tr>
        </thead>
        <tbody>
          {fleet.machines.map((machine) => {
            const alive = liveness(machine);
            const { platform } = machine.inventory.os;
            return (
              <tr key={machine.id} className={`contact-${alive}`}>
                <th scope="row">
                  <Link to={`/fleet/${machine.id}`}>{machine.name}</Link>
                  <span className="sub">
                    {osLabel(platform.os)} {platform.arch}
                  </span>
                </th>
                <td data-label="Liveness">
                  <Chip tone={LIVENESS[alive].tone} title={LIVENESS[alive].detail}>
                    {LIVENESS[alive].label}
                  </Chip>
                  <span className="sub">{lastSeenText(machine, now)}</span>
                </td>
                <td data-label="State">
                  <MachineState machine={machine} />
                </td>
                <td data-label="Enforcement">
                  <Enforcement machine={machine} />
                </td>
                <td data-label="CPU reserved / ceiling">
                  <Meter
                    reserved={machine.reserved.cpu_milli}
                    ceiling={machine.managed_ceiling.cpu_milli}
                    text={`${formatCpu(machine.reserved.cpu_milli).replace(/ CPUs?$/, '')} / ${formatCpu(machine.managed_ceiling.cpu_milli)}`}
                  />
                </td>
                <td data-label="Memory reserved / ceiling">
                  <Meter
                    reserved={machine.reserved.memory_bytes}
                    ceiling={machine.managed_ceiling.memory_bytes}
                    text={`${formatBytes(machine.reserved.memory_bytes)} / ${formatBytes(machine.managed_ceiling.memory_bytes)}`}
                  />
                </td>
                <td data-label="Running now">
                  {machine.attempts.length === 0 ? (
                    <span className="muted">idle</span>
                  ) : (
                    <ul className="plain running-list">
                      {machine.attempts.map((attempt) => (
                        <li key={attempt.attempt_id}>
                          <Link
                            to={`/jobs/${attempt.job_id}`}
                            title={`${attempt.recipe}, ${attempt.project_name}`}
                          >
                            {shortId(attempt.job_id)}
                          </Link>
                          {attempt.phase === 'executing' ? null : (
                            <span className="muted"> {phaseLabel(attempt.phase).toLowerCase()}</span>
                          )}
                          {attempt.uncertain ? <span className="muted"> (uncertain)</span> : null}
                        </li>
                      ))}
                    </ul>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ----------------------------------------------------- launching, running

function JobLink({ id, recipe }: { id: string; recipe?: string }) {
  return (
    <Link to={`/jobs/${id}`}>
      {recipe ? `${recipe} ` : ''}
      <Ident value={id} />
    </Link>
  );
}

function Elapsed({ since, now }: { since: Since | null; now: number }) {
  if (!since) return <span className="muted">unknown</span>;
  return (
    <span title={`since ${formatTimestamp(since.at)}`}>
      {formatDuration(Math.max(now - since.at, 0))}
      {since.sinceOffer ? <span className="muted"> since offered</span> : null}
    </span>
  );
}

function Phase({ row }: { row: AttemptRow }) {
  const { phase, uncertain } = row.attempt;
  const tone = phase === 'quarantined' ? 'bad' : phase === 'cleaning' ? 'warn' : 'info';
  return (
    <>
      <span className="chips">
        <Chip tone={tone}>{attemptPhaseLabel(row)}</Chip>
        {uncertain ? (
          <Chip tone="warn" title="The worker is out of contact; the attempt may still be running.">
            uncertain
          </Chip>
        ) : null}
      </span>
      {phase === 'cleaning' || phase === 'quarantined' ? (
        <span className="sub">resources still held</span>
      ) : null}
    </>
  );
}

function Attempts({
  fleet,
  jobs,
  rows,
  launching,
}: {
  fleet: UseQueryResult<FleetOverview>;
  jobs: UseQueryResult<JobView[]>;
  rows: AttemptRow[] | undefined;
  launching: boolean;
}) {
  const now = useNow(1000);
  return (
    <Loadable query={fleet} what={launching ? 'launching attempts' : 'running attempts'}>
      {() => (
        <>
          {jobs.error ? (
            <ErrorPanel
              error={jobs.error}
              onRetry={() => void jobs.refetch()}
              context="The job list could not be read, so times may be missing or out of date."
            />
          ) : null}
          {!rows || rows.length === 0 ? (
            launching ? (
              <Empty title="Nothing is being launched">
                Jobs offered to a machine or preparing their workspace appear here until they start
                executing.
              </Empty>
            ) : (
              <Empty title="Nothing is running">
                Jobs that are executing, collecting or cleaning up appear here until their resources
                are released.
              </Empty>
            )
          ) : (
            <div className="table-wrap">
              <table className="stack compact">
                <thead>
                  <tr>
                    <th scope="col">Job</th>
                    <th scope="col">Project</th>
                    <th scope="col">Machine</th>
                    <th scope="col">Phase</th>
                    <th scope="col">{launching ? 'In this phase' : 'Elapsed'}</th>
                    {launching ? null : <th scope="col">Holds</th>}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((row) => (
                    <tr key={row.attempt.attempt_id}>
                      <td data-label="Job">
                        <JobLink id={row.attempt.job_id} recipe={row.attempt.recipe} />
                      </td>
                      <td data-label="Project">{row.attempt.project_name}</td>
                      <td data-label="Machine">
                        <Link to={`/fleet/${row.machineId}`}>{row.machineName}</Link>
                      </td>
                      <td data-label="Phase">
                        <Phase row={row} />
                      </td>
                      <td data-label={launching ? 'In this phase' : 'Elapsed'}>
                        <Elapsed since={launching ? row.phaseSince : row.runningSince} now={now} />
                      </td>
                      {launching ? null : (
                        <td data-label="Holds">{formatResources(row.attempt.reserved)}</td>
                      )}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </Loadable>
  );
}

// ------------------------------------------------------------------- queue

function QueueTable({ queue, readAt }: { queue: QueueView; readAt: number }) {
  useNow(1000);
  if (queue.entries.length === 0) {
    return (
      <Empty title="Nothing is waiting">
        Jobs that have no machine yet appear here in order, with why they wait.
      </Empty>
    );
  }
  // `waiting_ms` was true when the queue was read; it has grown since.
  const age = Math.max(Date.now() - readAt, 0);
  const shown = queue.entries.slice(0, QUEUE_SHOWN);
  return (
    <>
      <div className="table-wrap">
        <table className="stack compact">
          <thead>
            <tr>
              <th scope="col">Position</th>
              <th scope="col">Job</th>
              <th scope="col">Project</th>
              <th scope="col">Priority</th>
              <th scope="col">Waited</th>
              <th scope="col">Why it waits</th>
            </tr>
          </thead>
          <tbody>
            {shown.map(({ job, position, waiting_ms }) => (
              <tr key={job.id}>
                <td data-label="Position">{position}</td>
                <td data-label="Job">
                  <JobLink id={job.id} recipe={job.recipe} />
                </td>
                <td data-label="Project">{job.project_name}</td>
                <td data-label="Priority">{priorityLabel(job.priority)}</td>
                <td data-label="Waited">{formatDuration(waiting_ms + age)}</td>
                <td data-label="Why it waits">
                  {job.queue_reason ? (
                    <>
                      <Chip tone={reasonTone(job.queue_reason.code)}>
                        {reasonLabel(job.queue_reason.code)}
                      </Chip>
                      <span className="sub">{job.queue_reason.message}</span>
                    </>
                  ) : (
                    <span className="muted">No reason given</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p>
        {queue.entries.length > shown.length
          ? `Showing the first ${shown.length} of ${queue.entries.length}. `
          : ''}
        <Link to="/queue">Open the full queue</Link>
      </p>
    </>
  );
}

// ------------------------------------------------------------------- alive

function Alive({ environments }: { environments: EnvironmentView[] }) {
  const now = useNow(1000);
  if (environments.length === 0) {
    return (
      <Empty title="Nothing is kept alive">
        Environments, which keep a job's services running, appear here with their endpoints and
        deadline.
      </Empty>
    );
  }
  return (
    <>
      <div className="table-wrap">
        <table className="stack compact">
          <thead>
            <tr>
              <th scope="col">Environment</th>
              <th scope="col">State</th>
              <th scope="col">Job</th>
              <th scope="col">Machine</th>
              <th scope="col">Endpoints</th>
              <th scope="col">Lease deadline</th>
            </tr>
          </thead>
          <tbody>
            {environments.map((environment) => (
              <tr key={environment.id}>
                <td data-label="Environment">
                  <Link to={`/environments/${environment.id}`}>
                    <Ident value={environment.id} />
                  </Link>
                </td>
                <td data-label="State">
                  <Chip tone={environmentStateTone(environment.state)}>
                    {environmentStateLabel(environment.state)}
                  </Chip>
                </td>
                <td data-label="Job">
                  <JobLink id={environment.job_id} />
                </td>
                <td data-label="Machine">
                  <Link to={`/fleet/${environment.machine_id}`}>{environment.machine_name}</Link>
                </td>
                <td data-label="Endpoints">
                  {environment.endpoints.length === 0 ? (
                    <span className="muted">none published</span>
                  ) : (
                    <ul className="plain">
                      {environment.endpoints.map((endpoint) => (
                        <li key={`${endpoint.step}/${endpoint.host}:${endpoint.port}`}>
                          <span className="tag">{endpoint.step}</span>{' '}
                          <code className="wrap">
                            {endpoint.host}:{endpoint.port}
                          </code>
                        </li>
                      ))}
                    </ul>
                  )}
                </td>
                <td data-label="Lease deadline">
                  <Countdown environment={environment} now={now} />
                  {environment.state === 'ready' || environment.state === 'starting' ? (
                    <span className="sub">{formatTimestamp(environment.effective_deadline)}</span>
                  ) : null}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p>
        <Link to="/environments">Open Environments</Link>
      </p>
    </>
  );
}

// ---------------------------------------------------------------- activity

interface Names {
  machine: (id: string) => string;
  recipe: (jobId: string) => string | undefined;
}

function Subject({ event, names }: { event: EventRecord; names: Names }) {
  if (event.job_id) return <JobLink id={event.job_id} recipe={names.recipe(event.job_id)} />;
  switch (event.entity) {
    case 'machine':
      return <Link to={`/fleet/${event.entity_id}`}>{names.machine(event.entity_id)}</Link>;
    case 'environment':
      return (
        <Link to={`/environments/${event.entity_id}`}>
          environment <Ident value={event.entity_id} />
        </Link>
      );
    default:
      return <span className="muted">{event.entity}</span>;
  }
}

function ActivityFeed({
  fleet,
  jobs,
  queue,
}: {
  fleet: FleetOverview | undefined;
  jobs: JobView[] | undefined;
  queue: QueueView | undefined;
}) {
  const { events } = useActivity();
  const live = useLive();

  const machines = new Map((fleet?.machines ?? []).map((m) => [m.id, m.name]));
  const recipes = new Map<string, string>();
  for (const event of events) {
    const recipe = (event.data as { recipe?: unknown } | null | undefined)?.recipe;
    if (event.job_id && typeof recipe === 'string') recipes.set(event.job_id, recipe);
  }
  for (const job of jobs ?? []) recipes.set(job.id, job.recipe);
  for (const entry of queue?.entries ?? []) recipes.set(entry.job.id, entry.job.recipe);
  for (const machine of fleet?.machines ?? []) {
    for (const attempt of machine.attempts) recipes.set(attempt.job_id, attempt.recipe);
  }
  const names: Names = {
    machine: (id) => machines.get(id) ?? shortId(id),
    recipe: (id) => recipes.get(id),
  };

  const down = live.status !== 'open' && live.status !== 'connecting';
  return (
    <>
      {down ? (
        <div className="state state-error" role="alert">
          <p className="state-title">The event stream is not connected</p>
          <p className="state-detail">
            Nothing new appears here until it is back; the sections above refresh on a timer.
            {live.detail ? ` ${live.detail}` : ''}
          </p>
        </div>
      ) : null}
      {live.drift ? (
        <p className="note note-warn">An event was not understood and is missing from this feed.</p>
      ) : null}
      {events.length === 0 ? (
        live.status === 'connecting' ? (
          <Loading what="recent events" />
        ) : (
          <Empty title="No recent events">
            Offers, starts, exits, releases, machine changes and submitted or cancelled jobs appear
            here as they happen.
          </Empty>
        )
      ) : (
        <p className="muted">
          The latest {events.length} events, newest first. A job's phase changes that only repeat an
          attempt event are left out.
        </p>
      )}
      {/* Present before the first event, so additions are announced. */}
      <ol
        className={events.length > 0 ? 'activity card' : 'activity'}
        aria-label="Recent events"
        aria-live="polite"
        aria-relevant="additions"
      >
        {events.map((event) => {
          const text = describeEvent(event, names);
          return (
            <li key={event.seq}>
              <time dateTime={new Date(event.at).toISOString()} title={formatTimestamp(event.at)}>
                {formatTimestamp(event.at).slice(11)}
              </time>
              <span>
                <Subject event={event} names={names} />
                {': '}
                {text ?? <code>{event.kind}</code>}
              </span>
            </li>
          );
        })}
      </ol>
    </>
  );
}
