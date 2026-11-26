import { useState } from 'react';
import { Link } from 'react-router';
import { useFleet, useJobSpec, useJobs, useNow, useQueue } from '../api/hooks';
import type { FleetOverview, JobView, ProjectShare, QueueEntry, QueueView as Queue } from '../api/schemas';
import { JobResultChips } from '../components/JobResult';
import { QueueReasonView } from '../components/QueueReasonView';
import { Chip, Empty, ErrorPanel, Ident, Loadable } from '../components/ui';
import { formatDuration, formatPercent, formatResources } from '../lib/format';
import { currentAttempt, describeConstraints, sessionLabel } from '../lib/job';
import { PRIORITY_ORDER, phaseLabel, priorityLabel, type Tone } from '../lib/labels';

const PRIORITY_TONE: Record<string, Tone> = { urgent: 'warn', interactive: 'info' };

function PriorityChip({ priority }: { priority: string }) {
  return <Chip tone={PRIORITY_TONE[priority] ?? 'neutral'}>{priorityLabel(priority)}</Chip>;
}

export function QueueView() {
  const queue = useQueue();
  const active = useJobs({ active: true });
  const fleet = useFleet();
  const [project, setProject] = useState('');
  const [priority, setPriority] = useState('');

  return (
    <>
      <h1>Queue</h1>
      <Loadable query={queue} what="the queue">
        {(data) => {
          const waitingIds = new Set(data.entries.map((entry) => entry.job.id));
          const running = (active.data ?? []).filter(
            (job) => !waitingIds.has(job.id) && job.phase !== 'queued' && job.phase !== 'finished',
          );
          const projects = [
            ...new Set([
              ...data.shares.map((share) => share.name),
              ...data.entries.map((entry) => entry.job.project_name),
              ...running.map((job) => job.project_name),
            ]),
          ].sort();
          const priorities = [
            ...PRIORITY_ORDER,
            // A priority a newer server added still gets a filter entry.
            ...new Set(
              [...data.entries.map((e) => e.job.priority), ...running.map((j) => j.priority)].filter(
                (p) => !PRIORITY_ORDER.includes(p),
              ),
            ),
          ];
          const keep = (job: JobView) =>
            (project === '' || job.project_name === project) &&
            (priority === '' || job.priority === priority);
          const entries = data.entries.filter((entry) => keep(entry.job));
          const runningShown = running.filter(keep);
          const filtered = project !== '' || priority !== '';

          return (
            <>
              <form className="filters" onSubmit={(event) => event.preventDefault()}>
                <div className="field">
                  <label htmlFor="filter-project">Project</label>
                  <select id="filter-project" value={project} onChange={(e) => setProject(e.target.value)}>
                    <option value="">All projects</option>
                    {projects.map((name) => (
                      <option key={name} value={name}>
                        {name}
                      </option>
                    ))}
                  </select>
                </div>
                <div className="field">
                  <label htmlFor="filter-priority">Priority</label>
                  <select
                    id="filter-priority"
                    value={priority}
                    onChange={(e) => setPriority(e.target.value)}
                  >
                    <option value="">All priorities</option>
                    {priorities.map((value) => (
                      <option key={value} value={value}>
                        {priorityLabel(value)}
                      </option>
                    ))}
                  </select>
                </div>
                {filtered ? (
                  <button
                    type="button"
                    className="btn"
                    onClick={() => {
                      setProject('');
                      setPriority('');
                    }}
                  >
                    Clear filters
                  </button>
                ) : null}
              </form>

              <section aria-labelledby="waiting-title">
                <h2 id="waiting-title">
                  Waiting ({entries.length}
                  {filtered ? ` of ${data.entries.length}` : ''})
                </h2>
                {data.entries.length === 0 ? (
                  <Empty title="Nothing is waiting">Every submitted job has been placed on a machine.</Empty>
                ) : entries.length === 0 ? (
                  <Empty title="No waiting job matches these filters" />
                ) : (
                  <ol className="queue">
                    {entries.map((entry) => (
                      <QueueRow key={entry.job.id} entry={entry} />
                    ))}
                  </ol>
                )}
              </section>

              <section aria-labelledby="running-title">
                <h2 id="running-title">
                  In progress ({runningShown.length}
                  {filtered ? ` of ${running.length}` : ''})
                </h2>
                <p className="muted">
                  Jobs that are uploading their snapshot or hold a reservation on a machine.
                </p>
                {active.error && active.data === undefined ? (
                  <ErrorPanel error={active.error} onRetry={() => void active.refetch()} />
                ) : active.data === undefined ? (
                  <p className="state state-loading" role="status">
                    Loading jobs in progress…
                  </p>
                ) : running.length === 0 ? (
                  <Empty title="Nothing is in progress">No job currently holds a machine.</Empty>
                ) : runningShown.length === 0 ? (
                  <Empty title="No job in progress matches these filters" />
                ) : (
                  <RunningTable jobs={runningShown} />
                )}
              </section>

              <FairnessPanel queue={data} fleet={fleet.data} />
            </>
          );
        }}
      </Loadable>
    </>
  );
}

function Constraints({ job }: { job: JobView }) {
  // Lists carry no spec; it is frozen per job, so it is read once and kept.
  const spec = useJobSpec(job.id, !job.spec);
  const resolved = job.spec ?? spec.data;
  if (resolved) {
    return (
      <ul className="plain">
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

function QueueRow({ entry }: { entry: QueueEntry }) {
  const { job } = entry;
  return (
    <li className="card queue-row">
      <div className="queue-row-head">
        <span className="position" title="Position in the queue">
          #{entry.position}
        </span>
        <Link to={`/jobs/${job.id}`} className="queue-job">
          {job.recipe} <Ident value={job.id} />
        </Link>
        <PriorityChip priority={job.priority} />
        <span className="muted">waiting {formatDuration(entry.waiting_ms)}</span>
      </div>
      <dl className="facts facts-wide">
        <div className="fact">
          <dt>Project</dt>
          <dd>{job.project_name}</dd>
        </div>
        <div className="fact">
          <dt>Session</dt>
          <dd>{sessionLabel(job)}</dd>
        </div>
        <div className="fact">
          <dt>Asks for (peak)</dt>
          <dd>{formatResources(job.envelope.peak)}</dd>
        </div>
        <div className="fact">
          <dt>Platform constraints</dt>
          <dd>
            <Constraints job={job} />
          </dd>
        </div>
      </dl>
      {job.queue_reason ? (
        <QueueReasonView reason={job.queue_reason} />
      ) : (
        <p className="muted">The controller gave no reason for the wait.</p>
      )}
      {job.cancel_requested ? <p className="note note-warn">Cancellation was requested.</p> : null}
    </li>
  );
}

function RunningTable({ jobs }: { jobs: JobView[] }) {
  const now = useNow(5000);
  return (
    <div className="table-wrap">
      <table className="stack">
        <thead>
          <tr>
            <th scope="col">Job</th>
            <th scope="col">Project</th>
            <th scope="col">Session</th>
            <th scope="col">Priority</th>
            <th scope="col">Phase</th>
            <th scope="col">Machine</th>
            <th scope="col">Holds</th>
            <th scope="col">Since submitted</th>
          </tr>
        </thead>
        <tbody>
          {jobs.map((job) => {
            const attempt = currentAttempt(job);
            return (
              <tr key={job.id}>
                <td data-label="Job">
                  <Link to={`/jobs/${job.id}`}>
                    {job.recipe} <Ident value={job.id} />
                  </Link>
                </td>
                <td data-label="Project">{job.project_name}</td>
                <td data-label="Session">{sessionLabel(job)}</td>
                <td data-label="Priority">
                  <PriorityChip priority={job.priority} />
                </td>
                <td data-label="Phase">
                  <span className="chips">
                    <Chip tone={job.phase === 'quarantined' ? 'bad' : 'info'}>{phaseLabel(job.phase)}</Chip>
                    {attempt?.uncertain ? (
                      <Chip tone="warn" title="The worker is out of contact; the attempt may still be running.">
                        uncertain
                      </Chip>
                    ) : null}
                    {job.outcome ? <JobResultChips job={job} /> : null}
                  </span>
                </td>
                <td data-label="Machine">{attempt ? attempt.machine_name : 'none yet'}</td>
                <td data-label="Holds">{attempt ? formatResources(attempt.reserved) : 'nothing'}</td>
                <td data-label="Since submitted">{formatDuration(Math.max(now - job.created_at, 0))}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

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

function FairnessPanel({ queue, fleet }: { queue: Queue; fleet: FleetOverview | undefined }) {
  const totalWeight = queue.shares.reduce((sum, share) => sum + share.weight, 0);
  return (
    <section aria-labelledby="fairness-title">
      <h2 id="fairness-title">Fair share between projects</h2>
      <p className="muted">
        Capacity is shared between projects by weight. Opening more sessions or accounts in a project
        does not add weight. "Reserved share" is the larger of the project's CPU and memory
        reservations as a part of what the fleet's building machines can give to managed work.
      </p>
      {queue.shares.length === 0 ? (
        <Empty title="No project has queued or running work" />
      ) : (
        <div className="table-wrap">
          <table className="stack">
            <thead>
              <tr>
                <th scope="col">Project</th>
                <th scope="col">Weight</th>
                <th scope="col">Queued</th>
                <th scope="col">Running</th>
                <th scope="col">Reserved</th>
                <th scope="col">Reserved share</th>
              </tr>
            </thead>
            <tbody>
              {queue.shares.map((share) => {
                const dominant = dominantShare(share, fleet);
                return (
                  <tr key={share.project_id}>
                    <td data-label="Project">{share.name}</td>
                    <td data-label="Weight">
                      {share.weight}
                      {totalWeight > 0 ? (
                        <span className="muted"> ({formatPercent(share.weight / totalWeight)} of weights)</span>
                      ) : null}
                    </td>
                    <td data-label="Queued">{share.queued}</td>
                    <td data-label="Running">{share.running}</td>
                    <td data-label="Reserved">{formatResources(share.reserved)}</td>
                    <td data-label="Reserved share">
                      {dominant === null ? 'needs fleet data' : formatPercent(dominant)}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
