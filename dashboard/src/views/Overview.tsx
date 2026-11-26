// The landing page: is everything OK, what is running where, what waits,
// and what just happened. It only reads, and each part fails on its own.

import type { UseQueryResult } from '@tanstack/react-query';
import { useState, type ReactNode } from 'react';
import { Link } from 'react-router';
import { useEnvironments, useFleet, useJobs, useNow, useQueue } from '../api/hooks';
import { useActivity, useLive } from '../api/live';
import type { EventRecord, FleetOverview, JobView, QueueView } from '../api/schemas';
import { JobLink, MachineLink, Platform } from '../components/bits';
import { Icon, type IconName } from '../components/icons';
import { MiniLoad } from '../components/LoadBar';
import {
  Callout,
  Card,
  Empty,
  ErrorPanel,
  FlagSummary,
  Ident,
  Loadable,
  Loading,
  PageHeader,
  StatusBadge,
} from '../components/ui';
import { machineCapacity } from '../lib/capacity';
import { describeEvent } from '../lib/eventText';
import { formatDuration, formatTimestamp, shortId } from '../lib/format';
import {
  aliveEnvironments,
  attemptRows,
  attentionItems,
  failuresSince,
  longestWait,
  type AttemptRow,
  type Since,
} from '../lib/overview';
import { isOnline, machineFlags, machineStatus, phaseStatus, queueLine } from '../lib/status';

/** Events listed before "show all". */
export const ACTIVITY_SHOWN = 12;
/** Queued jobs listed here; the rest are on the Queue page. */
export const QUEUE_SHOWN = 5;
/** The most the API returns in one list. */
const ACTIVE_LIMIT = 1000;
/** Recently changed jobs read to count failures. */
const RECENT_LIMIT = 200;
const HOUR = 3_600_000;

export function OverviewView() {
  const fleet = useFleet();
  const active = useJobs({ active: true, limit: ACTIVE_LIMIT });
  const recent = useJobs({ limit: RECENT_LIMIT });
  const queue = useQueue();
  const environments = useEnvironments();
  const activity = useActivity();

  const rows = fleet.data ? attemptRows(fleet.data, active.data, activity.phases) : undefined;
  const attention = attentionItems(fleet.data, queue.data);

  return (
    <div className="overview">
      <PageHeader title="Overview" description="Your build fleet at a glance: machines, what runs, what waits." />
      <Attention items={attention} />
      <Headlines fleet={fleet} queue={queue} recent={recent} rows={rows} />

      <div className="grid-2">
        <div className="stack-gap">
          <Card
            title="Machines"
            actions={
              <Link to="/machines" className="card-link">
                All machines <Icon name="chevronRight" size={14} />
              </Link>
            }
            flush
          >
            <Loadable query={fleet} what="the machines" lines={6}>
              {(data) => <MachineList fleet={data} />}
            </Loadable>
          </Card>
          <ServicesLine environments={environments} />
          <Card title="Recent activity" description="What the controller recorded, newest first.">
            <ActivityFeed fleet={fleet.data} jobs={active.data} queue={queue.data} />
          </Card>
        </div>

        <div className="stack-gap">
          <Card
            title="Running now"
            actions={
              <Link to="/jobs?show=running" className="card-link">
                All running <Icon name="chevronRight" size={14} />
              </Link>
            }
            flush
          >
            <Loadable query={fleet} what="running jobs" lines={4}>
              {() => (
                <>
                  {active.error ? (
                    <ErrorPanel
                      error={active.error}
                      onRetry={() => void active.refetch()}
                      context="The job list could not be read, so times may be missing."
                    />
                  ) : null}
                  <NowList rows={rows ? [...rows.launching, ...rows.running] : []} />
                </>
              )}
            </Loadable>
          </Card>

          <Card
            title="Next in the queue"
            actions={
              <Link to="/queue" className="card-link">
                Full queue <Icon name="chevronRight" size={14} />
              </Link>
            }
            flush
          >
            <Loadable query={queue} what="the queue" lines={3}>
              {(data) => <QueuePreview queue={data} readAt={queue.dataUpdatedAt} />}
            </Loadable>
          </Card>
        </div>
      </div>
    </div>
  );
}

// --------------------------------------------------------------- attention

function Attention({ items }: { items: ReturnType<typeof attentionItems> }) {
  if (items.length === 0) return null;
  const shown = items.slice(0, 4);
  return (
    <section className="attention" aria-labelledby="attention-title">
      <h2 id="attention-title" className="attention-title">
        <Icon name="alert" /> Needs attention
      </h2>
      <ul>
        {shown.map((item) => (
          <li key={item.key} className={`attention-item tone-${item.tone}`}>
            <span className="dot" aria-hidden="true" />
            <span className="attention-text">{item.text}</span>
            <Link to={item.to} className="attention-link">
              Open<span className="sr-only">: {item.text}</span>
            </Link>
          </li>
        ))}
      </ul>
      {items.length > shown.length ? (
        <p className="attention-more">
          {items.length - shown.length} more: see <Link to="/machines">Machines</Link> and{' '}
          <Link to="/queue">Queue</Link>.
        </p>
      ) : null}
    </section>
  );
}

// --------------------------------------------------------------- headlines

function Tile({
  label,
  icon,
  to,
  query,
  value,
  detail,
  tone,
}: {
  label: string;
  icon: IconName;
  to: string;
  /** The query the number comes from. */
  query: UseQueryResult<unknown>;
  value: ReactNode;
  detail?: ReactNode;
  tone?: 'bad' | 'warn';
}) {
  const unknown = query.data === undefined;
  const failed = unknown && query.error !== null;
  return (
    <Link to={to} className={`tile${tone ? ` tile-${tone}` : ''}`}>
      <span className="tile-label">
        <Icon name={icon} size={16} /> {label}
      </span>
      <span className="tile-value">
        {unknown ? (
          failed ? (
            <span className="muted">unknown</span>
          ) : (
            <span className="skeleton-line skeleton-short" aria-label="loading" />
          )
        ) : (
          value
        )}
      </span>
      <span className="tile-detail">{unknown ? ' ' : (detail ?? ' ')}</span>
    </Link>
  );
}

function Headlines({
  fleet,
  queue,
  recent,
  rows,
}: {
  fleet: UseQueryResult<FleetOverview>;
  queue: UseQueryResult<QueueView>;
  recent: UseQueryResult<JobView[]>;
  rows: ReturnType<typeof attemptRows> | undefined;
}) {
  const now = useNow(5000);
  const machines = fleet.data?.machines ?? [];
  const statuses = machines.map(machineStatus).filter((status) => status.key !== 'retired');
  const online = statuses.filter(isOnline).length;
  const stale = statuses.filter((s) => s.key === 'stale').length;
  const offline = statuses.filter((s) => s.key === 'offline').length;
  const down = [stale ? `${stale} stale` : '', offline ? `${offline} offline` : ''].filter(Boolean).join(', ');

  const stuck = (queue.data?.entries ?? []).filter((e) => queueLine(e.job.queue_reason).stuck).length;
  const longest = queue.data ? longestWait(queue.data) : undefined;
  const age = Math.max(Date.now() - queue.dataUpdatedAt, 0);
  const failures = recent.data ? failuresSince(recent.data, now - HOUR, RECENT_LIMIT) : undefined;

  return (
    <dl className="tiles" aria-label="Fleet in numbers">
      <div>
        <dt className="sr-only">Machines online</dt>
        <dd>
          <Tile
            label="Machines online"
            icon="machines"
            to="/machines"
            query={fleet}
            value={
              <>
                {online} <span className="tile-of">of {statuses.length}</span>
              </>
            }
            detail={down || 'all in contact'}
            tone={offline > 0 ? 'bad' : stale > 0 ? 'warn' : undefined}
          />
        </dd>
      </div>
      <div>
        <dt className="sr-only">Running</dt>
        <dd>
          <Tile
            label="Running"
            icon="activity"
            to="/jobs?show=running"
            query={fleet}
            value={rows ? rows.running.length : null}
            detail={rows && rows.launching.length > 0 ? `${rows.launching.length} launching` : 'none launching'}
          />
        </dd>
      </div>
      <div>
        <dt className="sr-only">Queued</dt>
        <dd>
          <Tile
            label="Queued"
            icon="queue"
            to="/queue"
            query={queue}
            value={queue.data?.entries.length}
            detail={stuck > 0 ? `${stuck} can't run` : 'all can run'}
            tone={stuck > 0 ? 'bad' : undefined}
          />
        </dd>
      </div>
      <div>
        <dt className="sr-only">Waiting longest</dt>
        <dd>
          <Tile
            label="Waiting longest"
            icon="clock"
            to={longest ? `/jobs/${longest.job.id}` : '/queue'}
            query={queue}
            value={longest ? formatDuration(longest.waiting_ms + age) : '—'}
            detail={longest ? `${longest.job.recipe}, ${longest.job.project_name}` : 'nothing waits'}
          />
        </dd>
      </div>
      <div>
        <dt className="sr-only">Failed in the last hour</dt>
        <dd>
          <Tile
            label="Failed, last hour"
            icon="alert"
            to="/jobs?show=failed"
            query={recent}
            value={failures ? `${failures.count}${failures.atLeast ? '+' : ''}` : null}
            detail={failures && failures.count > 0 ? 'open the list' : 'no failures'}
            tone={failures && failures.count > 0 ? 'bad' : undefined}
          />
        </dd>
      </div>
    </dl>
  );
}

// ---------------------------------------------------------------- machines

function MachineList({ fleet }: { fleet: FleetOverview }) {
  if (fleet.machines.length === 0) {
    return (
      <Empty
        title="No machines yet"
        icon="machines"
        action={
          <Link to="/settings#add-machine" className="btn btn-primary btn-small">
            <Icon name="plus" size={14} /> Add your first machine
          </Link>
        }
      >
        Machines run the jobs. Create a pairing code in Settings, then run{' '}
        <code>bella worker enroll</code> on the machine.
      </Empty>
    );
  }
  return (
    <ul className="rows" aria-label="Machines">
      {fleet.machines.map((machine) => {
        const status = machineStatus(machine);
        const capacity = machineCapacity(machine);
        const count = machine.attempts.length;
        const flags = machineFlags(machine, status);
        return (
          <li key={machine.id} className="row machine-row">
            <span className="row-main">
              <MachineLink id={machine.id} name={machine.name} />
              <span className="row-sub row-sub-flags">
                {flags.length > 0 ? <FlagSummary flags={flags} /> : <Platform machine={machine} />}
              </span>
            </span>
            <span className="row-status">
              <StatusBadge status={status} />
            </span>
            <MiniLoad cpu={capacity.cpu} memory={capacity.memory} />
            <span className="row-count" title={`${count} ${count === 1 ? 'job' : 'jobs'} on this machine`}>
              {count > 0 ? `${count} ${count === 1 ? 'job' : 'jobs'}` : <span className="muted">no jobs</span>}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

// --------------------------------------------------------------------- now

function Elapsed({ since, now }: { since: Since | null; now: number }) {
  if (!since) return <span className="muted">—</span>;
  return (
    <span title={`${since.sinceOffer ? 'Since it was offered' : 'Since'} ${formatTimestamp(since.at)}`}>
      {formatDuration(Math.max(now - since.at, 0))}
      {since.sinceOffer ? <span className="muted"> since offered</span> : null}
    </span>
  );
}

/** Running jobs listed here; the rest are on the Jobs page. */
export const NOW_SHOWN = 6;

function NowList({ rows }: { rows: AttemptRow[] }) {
  const now = useNow(1000);
  if (rows.length === 0) {
    return (
      <Empty title="Nothing is running" icon="activity">
        Jobs appear here from the moment a machine takes them until their resources are released.
      </Empty>
    );
  }
  return (
    <>
    <ul className="rows" aria-label="Running and launching jobs">
      {rows.slice(0, NOW_SHOWN).map((row) => {
        const status = phaseStatus(row.attempt.phase);
        const shown = row.accepted ? { ...status, precise: 'Accepted by the machine, starting' } : status;
        const launching = status.key === 'launching';
        return (
          <li key={row.attempt.attempt_id} className="row now-row">
            <span className="row-status">
              <StatusBadge status={shown} live />
            </span>
            <span className="row-main">
              <JobLink id={row.attempt.job_id} recipe={row.attempt.recipe} />
              <span className="row-sub">
                {row.attempt.project_name} on <MachineLink id={row.machineId} name={row.machineName} />
                {row.attempt.uncertain ? <span className="text-warn"> · out of contact</span> : null}
                {row.attempt.phase === 'quarantined' ? <span className="text-bad"> · stuck cleanup</span> : null}
              </span>
            </span>
            <span className="row-time">
              <Elapsed since={launching ? row.phaseSince : row.runningSince} now={now} />
            </span>
          </li>
        );
      })}
    </ul>
    {rows.length > NOW_SHOWN ? (
      <p className="card-foot">
        {rows.length - NOW_SHOWN} more. <Link to="/jobs?show=running">See all running jobs</Link>
      </p>
    ) : null}
    </>
  );
}

function QueuePreview({ queue, readAt }: { queue: QueueView; readAt: number }) {
  useNow(1000);
  if (queue.entries.length === 0) {
    return (
      <Empty title="Nothing is waiting" icon="queue">
        Submitted jobs wait here until a machine has room for them.
      </Empty>
    );
  }
  // `waiting_ms` was true when the queue was read; it has grown since.
  const age = Math.max(Date.now() - readAt, 0);
  const shown = queue.entries.slice(0, QUEUE_SHOWN);
  return (
    <>
      <ol className="rows" aria-label="Next in the queue">
        {shown.map(({ job, position, waiting_ms }) => {
          const line = queueLine(job.queue_reason);
          return (
            <li key={job.id} className="row queue-row">
              <span className="row-position" aria-label={`Position ${position}`}>
                {position}
              </span>
              <span className="row-main">
                <JobLink id={job.id} recipe={job.recipe} />
                <span className={`row-sub reason-line tone-${line.tone}`} title={`${line.headline}. ${line.message}`}>
                  <span className="dot" aria-hidden="true" />
                  {line.headline}
                </span>
              </span>
              <span className="row-time">{formatDuration(waiting_ms + age)}</span>
            </li>
          );
        })}
      </ol>
      {queue.entries.length > shown.length ? (
        <p className="card-foot">
          {queue.entries.length - shown.length} more waiting. <Link to="/queue">Open the queue</Link>
        </p>
      ) : null}
    </>
  );
}

function ServicesLine({ environments }: { environments: ReturnType<typeof useEnvironments> }) {
  const alive = environments.data ? aliveEnvironments(environments.data) : undefined;
  const ready = alive?.filter((environment) => environment.state === 'ready').length ?? 0;
  return (
    <Link to="/services" className="card services-line">
      <Icon name="services" />
      <span>
        <strong>Services</strong>{' '}
        <span className="muted">
          {alive === undefined
            ? environments.error
              ? 'could not be read'
              : 'loading…'
            : alive.length === 0
              ? 'none kept running'
              : `${alive.length} kept running, ${ready} ready`}
        </span>
      </span>
      <Icon name="chevronRight" size={16} />
    </Link>
  );
}

// ---------------------------------------------------------------- activity

interface Names {
  machine: (id: string) => string;
  recipe: (jobId: string) => string | undefined;
}

const capitalized = (word: string) => word.charAt(0).toUpperCase() + word.slice(1);

function Subject({ event, names }: { event: EventRecord; names: Names }) {
  if (event.job_id) return <JobLink id={event.job_id} recipe={names.recipe(event.job_id)} />;
  switch (event.entity) {
    case 'machine':
      return <Link to={`/machines/${event.entity_id}`}>{names.machine(event.entity_id)}</Link>;
    case 'environment':
      return (
        <Link to={`/services/${event.entity_id}`}>
          service <Ident value={event.entity_id} />
        </Link>
      );
    default:
      return <span className="muted">{capitalized(event.entity)}</span>;
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
  const [all, setAll] = useState(false);
  const shown = all ? events : events.slice(0, ACTIVITY_SHOWN);

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
        <Callout tone="warn" title="Live updates are not connected" role="alert">
          Nothing new appears here until they are back; the rest of the page refreshes on a timer.
          {live.detail ? ` ${live.detail}` : ''}
        </Callout>
      ) : null}
      {live.drift ? <p className="note note-warn">An event was not understood and is missing from this list.</p> : null}
      {events.length === 0 ? (
        live.status === 'connecting' ? (
          <Loading what="recent events" />
        ) : (
          <Empty title="No recent events" icon="activity">
            Jobs being placed, started, finished and cleaned up, and machines coming and going, appear here as
            they happen.
          </Empty>
        )
      ) : null}
      {/* Present before the first event, so additions are announced. */}
      <ol className="activity" aria-label="Recent events" aria-live="polite" aria-relevant="additions">
        {shown.map((event) => {
          const text = describeEvent(event, names);
          return (
            <li key={event.seq}>
              <time dateTime={new Date(event.at).toISOString()} title={formatTimestamp(event.at)}>
                {formatTimestamp(event.at).slice(11)}
              </time>
              <span className="activity-text">
                <Subject event={event} names={names} />
                <span className="activity-what">{text ?? <code>{event.kind}</code>}</span>
              </span>
            </li>
          );
        })}
      </ol>
      {events.length > shown.length ? (
        <button type="button" className="btn btn-small" onClick={() => setAll(true)}>
          Show all {events.length} recent events
        </button>
      ) : null}
    </>
  );
}
