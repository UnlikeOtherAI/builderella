// What waits, in order, and why: one line per job, with the machine-by-
// machine explanation one click away. Below it what is in progress, and the
// fair share between projects for whoever wants it.

import { Fragment, useState } from 'react';
import { Link } from 'react-router';
import { useFleet, useJobSpec, useJobs, useNow, useQueue } from '../api/hooks';
import { useActivity } from '../api/live';
import type { FleetOverview, JobView, ProjectShare, QueueEntry, QueueView as Queue } from '../api/schemas';
import { JobLink, MachineLink } from '../components/bits';
import { Icon } from '../components/icons';
import { QueueReasonView } from '../components/QueueReasonView';
import {
  Card,
  Chip,
  Empty,
  ErrorPanel,
  Fact,
  Facts,
  Help,
  Loadable,
  PageHeader,
  StatusBadge,
} from '../components/ui';
import { formatDuration, formatPercent, formatResources } from '../lib/format';
import { describeConstraints, sessionLabel } from '../lib/job';
import { PRIORITY_ORDER, priorityLabel, type Tone } from '../lib/labels';
import { attemptRows, type AttemptRow } from '../lib/overview';
import { phaseStatus, queueLine } from '../lib/status';

/** The most the API returns in one list. */
const ACTIVE_LIMIT = 1000;

const PRIORITY_TONE: Record<string, Tone> = { urgent: 'bad', interactive: 'info' };

export function QueueView() {
  const queue = useQueue();
  const fleet = useFleet();
  const [project, setProject] = useState('');
  const [priority, setPriority] = useState('');

  return (
    <>
      <PageHeader
        title="Queue"
        description="Jobs waiting for a machine, in the order they will be offered, with why each one waits."
      />
      <Loadable query={queue} what="the queue" lines={8}>
        {(data) => {
          const projects = [
            ...new Set([...data.shares.map((s) => s.name), ...data.entries.map((e) => e.job.project_name)]),
          ].sort();
          const priorities = [
            ...PRIORITY_ORDER,
            // A priority a newer server added still gets a filter entry.
            ...new Set(data.entries.map((e) => e.job.priority).filter((p) => !PRIORITY_ORDER.includes(p))),
          ];
          const keep = (job: Pick<JobView, 'project_name' | 'priority'>) =>
            (project === '' || job.project_name === project) && (priority === '' || job.priority === priority);
          const entries = data.entries.filter((entry) => keep(entry.job));
          const filtered = project !== '' || priority !== '';
          return (
            <>
              <Card
                flush
                title={`Waiting (${filtered ? `${entries.length} of ${data.entries.length}` : data.entries.length})`}
                actions={
                  <form className="filters" onSubmit={(event) => event.preventDefault()}>
                    <label className="select">
                      <span className="sr-only">Project</span>
                      <select value={project} onChange={(e) => setProject(e.target.value)}>
                        <option value="">All projects</option>
                        {projects.map((name) => (
                          <option key={name} value={name}>
                            {name}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className="select">
                      <span className="sr-only">Priority</span>
                      <select value={priority} onChange={(e) => setPriority(e.target.value)}>
                        <option value="">All priorities</option>
                        {priorities.map((value) => (
                          <option key={value} value={value}>
                            {priorityLabel(value)}
                          </option>
                        ))}
                      </select>
                    </label>
                    {filtered ? (
                      <button
                        type="button"
                        className="btn btn-small"
                        onClick={() => {
                          setProject('');
                          setPriority('');
                        }}
                      >
                        Clear
                      </button>
                    ) : null}
                  </form>
                }
              >
                {data.entries.length === 0 ? (
                  <Empty title="Nothing is waiting" icon="queue">
                    Every submitted job has a machine. New jobs wait here until one has room.
                  </Empty>
                ) : entries.length === 0 ? (
                  <Empty title="No waiting job matches these filters" icon="search" />
                ) : (
                  <WaitingTable entries={entries} readAt={queue.dataUpdatedAt} />
                )}
              </Card>
              <InProgress fleet={fleet} keep={keep} />
              <FairShare queue={data} fleet={fleet.data} />
            </>
          );
        }}
      </Loadable>
    </>
  );
}

// ----------------------------------------------------------------- waiting

function WaitingTable({ entries, readAt }: { entries: QueueEntry[]; readAt: number }) {
  useNow(1000);
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  // `waiting_ms` was true when the queue was read; it has grown since.
  const age = Math.max(Date.now() - readAt, 0);
  const toggle = (id: string) =>
    setOpen((previous) => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  return (
    <div className="table-wrap">
      <table className="table table-stack queue-table">
        <thead>
          <tr>
            <th scope="col">
              <span className="sr-only">Details</span>
            </th>
            <th scope="col" className="num">
              #
            </th>
            <th scope="col">Job</th>
            <th scope="col">Project</th>
            <th scope="col">Priority</th>
            <th scope="col">Waiting</th>
            <th scope="col">Why it waits</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((entry) => {
            const { job } = entry;
            const line = queueLine(job.queue_reason);
            const expanded = open.has(job.id);
            const detailId = `queue-detail-${job.id}`;
            return (
              <Fragment key={job.id}>
                <tr className={expanded ? 'row-expanded' : undefined}>
                  <td className="cell-toggle">
                    <button
                      type="button"
                      className="icon-btn icon-btn-small"
                      aria-expanded={expanded}
                      aria-controls={detailId}
                      aria-label={`${expanded ? 'Hide' : 'Show'} why ${job.recipe} waits`}
                      onClick={() => toggle(job.id)}
                    >
                      <Icon name={expanded ? 'chevronDown' : 'chevronRight'} size={16} />
                    </button>
                  </td>
                  <td data-label="Position" className="num">
                    {entry.position}
                  </td>
                  <td data-label="Job">
                    <JobLink id={job.id} recipe={job.recipe} />
                  </td>
                  <td data-label="Project">{job.project_name}</td>
                  <td data-label="Priority">
                    <Chip tone={PRIORITY_TONE[job.priority] ?? 'neutral'}>{priorityLabel(job.priority)}</Chip>
                  </td>
                  <td data-label="Waiting" className="nowrap">
                    {formatDuration(entry.waiting_ms + age)}
                  </td>
                  <td data-label="Why it waits" className="cell-reason">
                    <span className={`reason-line tone-${line.tone}`}>
                      <span className="dot" aria-hidden="true" />
                      <strong>{line.headline}</strong>
                      <span className="reason-message-inline" title={line.message}>
                        {line.message}
                      </span>
                    </span>
                    {job.cancel_requested ? <Chip tone="warn">Cancelling</Chip> : null}
                  </td>
                </tr>
                {expanded ? (
                  <tr className="row-detail" id={detailId}>
                    <td colSpan={7}>
                      <WaitingDetail job={job} />
                    </td>
                  </tr>
                ) : null}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function Constraints({ job }: { job: JobView }) {
  // Lists carry no spec; it is frozen per job, so it is read once and kept.
  const spec = useJobSpec(job.id, !job.spec);
  const resolved = job.spec ?? spec.data;
  if (resolved) {
    return (
      <ul className="plain-list">
        {describeConstraints(resolved).map((line) => (
          <li key={line}>{line}</li>
        ))}
      </ul>
    );
  }
  if (spec.error) return <span className="muted">Could not be read; open the job for details.</span>;
  if (spec.data === null) return <span className="muted">Not reported by the controller.</span>;
  return <span className="muted">Loading…</span>;
}

function WaitingDetail({ job }: { job: JobView }) {
  return (
    <div className="detail-grid">
      <div>
        {job.queue_reason ? (
          <QueueReasonView reason={job.queue_reason} />
        ) : (
          <p className="muted">The controller gave no reason for the wait.</p>
        )}
      </div>
      <Facts>
        <Fact label="Submitted by">{sessionLabel(job)}</Fact>
        <Fact label="Asks for (peak)">{formatResources(job.envelope.peak)}</Fact>
        <Fact label="Where it may run">
          <Constraints job={job} />
        </Fact>
      </Facts>
    </div>
  );
}

// ------------------------------------------------------------- in progress

function InProgress({
  fleet,
  keep,
}: {
  fleet: ReturnType<typeof useFleet>;
  keep: (job: Pick<JobView, 'project_name' | 'priority'>) => boolean;
}) {
  // Held attempts come from the fleet, which lists every one of them; the
  // job list only adds times and the jobs still uploading their snapshot.
  const active = useJobs({ active: true, limit: ACTIVE_LIMIT });
  const activity = useActivity();
  const now = useNow(1000);
  const rows = fleet.data ? attemptRows(fleet.data, active.data, activity.phases) : undefined;
  const held = rows ? [...rows.launching, ...rows.running] : [];
  const capturing = (active.data ?? []).filter((job) => job.phase === 'capturing');
  const priorityOf = new Map((active.data ?? []).map((job) => [job.id, job.priority]));
  const shownHeld = held.filter((row) =>
    keep({ project_name: row.attempt.project_name, priority: priorityOf.get(row.attempt.job_id) ?? '' }),
  );
  const shownCapturing = capturing.filter(keep);
  const total = shownHeld.length + shownCapturing.length;
  return (
    <Card
      flush
      title={`In progress (${total})`}
      description="Jobs uploading their files or holding a machine."
      actions={
        <Link to="/jobs?show=running" className="card-link">
          In Jobs <Icon name="chevronRight" size={14} />
        </Link>
      }
    >
      {active.error ? (
        <ErrorPanel
          error={active.error}
          onRetry={() => void active.refetch()}
          context="The job list could not be read, so times and uploading jobs may be missing."
        />
      ) : null}
      <Loadable query={fleet} what="jobs in progress" lines={3}>
        {() =>
          total === 0 ? (
            <Empty title="Nothing is in progress" icon="activity">
              No job holds a machine right now.
            </Empty>
          ) : (
            <ul className="rows" aria-label="Jobs in progress">
              {shownCapturing.map((job) => (
                <li key={job.id} className="row now-row">
                  <span className="row-status">
                    <StatusBadge status={phaseStatus(job.phase)} />
                  </span>
                  <span className="row-main">
                    <JobLink id={job.id} recipe={job.recipe} />
                    <span className="row-sub">{job.project_name}, uploading its files</span>
                  </span>
                  <span className="row-time">{formatDuration(Math.max(now - job.created_at, 0))}</span>
                </li>
              ))}
              {shownHeld.map((row: AttemptRow) => (
                <li key={row.attempt.attempt_id} className="row now-row">
                  <span className="row-status">
                    <StatusBadge status={phaseStatus(row.attempt.phase)} live />
                  </span>
                  <span className="row-main">
                    <JobLink id={row.attempt.job_id} recipe={row.attempt.recipe} />
                    <span className="row-sub">
                      {row.attempt.project_name} on <MachineLink id={row.machineId} name={row.machineName} />, holds{' '}
                      {formatResources(row.attempt.reserved)}
                      {row.attempt.uncertain ? <span className="text-warn"> · out of contact</span> : null}
                    </span>
                  </span>
                  <span className="row-time">
                    {row.runningSince ? (
                      <>
                        {formatDuration(Math.max(now - row.runningSince.at, 0))}
                        {row.runningSince.sinceOffer ? <span className="muted"> since offered</span> : null}
                      </>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </span>
                </li>
              ))}
            </ul>
          )
        }
      </Loadable>
    </Card>
  );
}

// -------------------------------------------------------------- fair share

/** The larger of a project's CPU and memory share of the fleet's managed ceiling. */
function dominantShare(share: ProjectShare, fleet: FleetOverview | undefined): number | null {
  if (!fleet) return null;
  let cpu = 0;
  let memory = 0;
  for (const machine of fleet.machines) {
    if (!machine.policy.accept_builds || machine.lifecycle !== 'ready') continue;
    cpu += machine.managed_ceiling.cpu_milli;
    memory += machine.managed_ceiling.memory_bytes;
  }
  if (cpu === 0 || memory === 0) return null;
  return Math.max(share.reserved.cpu_milli / cpu, share.reserved.memory_bytes / memory);
}

function FairShare({ queue, fleet }: { queue: Queue; fleet: FleetOverview | undefined }) {
  const totalWeight = queue.shares.reduce((sum, share) => sum + share.weight, 0);
  return (
    <details className="card disclosure-card">
      <summary>
        <span className="disclosure-title">Fair share between projects</span>
        <span className="muted">
          Projects take turns by weight, so one busy project cannot starve the others.
        </span>
      </summary>
      <div className="card-body">
        <p className="muted">
          Opening more sessions or accounts in a project does not add weight. "Share in use" is the larger of the
          project's CPU and memory reservations, as a part of what the fleet's building machines can give to jobs{' '}
          <Help label="Why the larger of CPU and memory">
            A project holding most of the memory blocks others even with little CPU, so its share is counted by
            whichever resource it uses more of.
          </Help>
          .
        </p>
        {queue.shares.length === 0 ? (
          <Empty title="No project has queued or running work" icon="folder" />
        ) : (
          <div className="table-wrap">
            <table className="table table-stack">
              <thead>
                <tr>
                  <th scope="col">Project</th>
                  <th scope="col">Weight</th>
                  <th scope="col" className="num">
                    Queued
                  </th>
                  <th scope="col" className="num">
                    Running
                  </th>
                  <th scope="col">Reserved</th>
                  <th scope="col">Share in use</th>
                </tr>
              </thead>
              <tbody>
                {queue.shares.map((share) => {
                  const dominant = dominantShare(share, fleet);
                  return (
                    <tr key={share.project_id}>
                      <th scope="row" data-label="Project">
                        {share.name}
                      </th>
                      <td data-label="Weight">
                        {share.weight}
                        {totalWeight > 0 ? (
                          <span className="muted"> ({formatPercent(share.weight / totalWeight)} of all weight)</span>
                        ) : null}
                      </td>
                      <td data-label="Queued" className="num">
                        {share.queued}
                      </td>
                      <td data-label="Running" className="num">
                        {share.running}
                      </td>
                      <td data-label="Reserved">{formatResources(share.reserved)}</td>
                      <td data-label="Share in use">{dominant === null ? 'needs fleet data' : formatPercent(dominant)}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </details>
  );
}
