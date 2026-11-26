// Every job in one table: filter by what it is doing, search by job, recipe,
// project or machine.

import { useSearchParams } from 'react-router';
import { Link } from 'react-router';
import { useJobs, useNow } from '../api/hooks';
import type { JobView } from '../api/schemas';
import { JobLink, JobStatus } from '../components/bits';
import { Icon } from '../components/icons';
import { Ago, Card, Empty, ErrorPanel, Loading, PageHeader, Tabs } from '../components/ui';
import { formatDuration } from '../lib/format';
import { currentAttempt, sessionLabel } from '../lib/job';
import { isFailure, jobGroup } from '../lib/status';

/** The most the API returns in one list. */
export const ACTIVE_LIMIT = 1000;
/** Finished jobs come from the most recently changed ones. */
export const RECENT_LIMIT = 200;

export const JOB_FILTERS = ['all', 'running', 'queued', 'finished', 'failed'] as const;
export type JobFilter = (typeof JOB_FILTERS)[number];

const FILTER_LABEL: Record<JobFilter, string> = {
  all: 'All',
  running: 'Running',
  queued: 'Queued',
  finished: 'Finished',
  failed: 'Failed',
};

export function matchesFilter(job: JobView, filter: JobFilter): boolean {
  switch (filter) {
    case 'all':
      return true;
    case 'failed':
      return isFailure(job);
    default:
      return jobGroup(job) === filter;
  }
}

export function matchesSearch(job: JobView, search: string): boolean {
  const needle = search.trim().toLowerCase();
  if (needle === '') return true;
  const attempt = currentAttempt(job);
  return [job.id, job.recipe, job.project_name, attempt?.machine_name ?? '', sessionLabel(job)].some((text) =>
    text.toLowerCase().includes(needle),
  );
}

/** Active jobs first-hand, then recently changed ones, each once. */
export function mergeJobs(active: JobView[] | undefined, recent: JobView[] | undefined): JobView[] {
  const byId = new Map<string, JobView>();
  for (const job of recent ?? []) byId.set(job.id, job);
  for (const job of active ?? []) byId.set(job.id, job);
  return [...byId.values()].sort((a, b) => b.updated_at - a.updated_at);
}

const ID_PATTERN = /^job_[0-9a-f]{32}$/;

export function JobsView() {
  const [params, setParams] = useSearchParams();
  const shown = params.get('show');
  const filter: JobFilter = (JOB_FILTERS as readonly string[]).includes(shown ?? '') ? (shown as JobFilter) : 'all';
  const search = params.get('q') ?? '';
  const active = useJobs({ active: true, limit: ACTIVE_LIMIT });
  const recent = useJobs({ limit: RECENT_LIMIT });

  const update = (next: { show?: JobFilter; q?: string }) => {
    const merged = new URLSearchParams(params);
    const show = next.show ?? filter;
    const q = next.q ?? search;
    if (show === 'all') merged.delete('show');
    else merged.set('show', show);
    if (q === '') merged.delete('q');
    else merged.set('q', q);
    setParams(merged, { replace: true });
  };

  const loaded = active.data !== undefined || recent.data !== undefined;
  const all = mergeJobs(active.data, recent.data);
  const searched = all.filter((job) => matchesSearch(job, search));
  const rows = searched.filter((job) => matchesFilter(job, filter));
  const typedId = search.trim();

  return (
    <>
      <PageHeader title="Jobs" description="Everything submitted to the fleet: waiting, running and finished." />
      <Card flush>
        <div className="toolbar">
          <Tabs
            label="Show jobs"
            idBase="jobs"
            value={filter}
            onChange={(id) => update({ show: id as JobFilter })}
            tabs={JOB_FILTERS.map((id) => ({
              id,
              label: FILTER_LABEL[id],
              count: loaded ? searched.filter((job) => matchesFilter(job, id)).length : undefined,
            }))}
          />
          <label className="search">
            <Icon name="search" size={16} />
            <span className="sr-only">Search jobs</span>
            <input
              type="search"
              value={search}
              placeholder="Search job, recipe, project, machine"
              onChange={(event) => update({ q: event.target.value })}
              spellCheck={false}
              autoComplete="off"
            />
          </label>
        </div>
        <div role="tabpanel" id={`jobs-panel-${filter}`} aria-labelledby={`jobs-tab-${filter}`}>
          {active.error && recent.error ? (
            <ErrorPanel error={active.error} onRetry={() => void active.refetch()} />
          ) : !loaded ? (
            <Loading what="jobs" lines={8} />
          ) : (
            <>
              {active.error || recent.error ? (
                <ErrorPanel
                  error={active.error ?? recent.error}
                  onRetry={() => {
                    void active.refetch();
                    void recent.refetch();
                  }}
                  context={
                    active.error
                      ? 'Active jobs could not be read; only recently changed jobs are listed.'
                      : 'Recently finished jobs could not be read; only active jobs are listed.'
                  }
                />
              ) : null}
              {rows.length === 0 ? (
                <JobsEmpty filter={filter} search={search} total={all.length} />
              ) : (
                <JobTable jobs={rows} />
              )}
              {ID_PATTERN.test(typedId) && !all.some((job) => job.id === typedId) ? (
                <p className="card-foot">
                  <Link to={`/jobs/${typedId}`}>Open {typedId}</Link>: it is not among the jobs listed here.
                </p>
              ) : null}
              {active.data && active.data.length >= ACTIVE_LIMIT ? (
                <p className="card-foot muted">
                  Showing the newest {ACTIVE_LIMIT} unfinished jobs, the most the controller returns at once.
                </p>
              ) : null}
              <p className="card-foot muted">
                Finished jobs: the {RECENT_LIMIT} most recently changed. Search by a full job id to open any job.
              </p>
            </>
          )}
        </div>
      </Card>
    </>
  );
}

function JobsEmpty({ filter, search, total }: { filter: JobFilter; search: string; total: number }) {
  if (total === 0) {
    return (
      <Empty title="No jobs yet" icon="jobs">
        Jobs appear here once an agent or the CLI submits one, for example with <code>bella run</code>.
      </Empty>
    );
  }
  if (search.trim() !== '') {
    return (
      <Empty title="No job matches this search" icon="search">
        Nothing {filter === 'all' ? '' : `${FILTER_LABEL[filter].toLowerCase()} `}matches "{search.trim()}".
      </Empty>
    );
  }
  const text: Record<JobFilter, string> = {
    all: '',
    running: 'Nothing is running right now.',
    queued: 'Nothing is waiting for a machine.',
    finished: 'No job has finished recently.',
    failed: 'No recent job failed.',
  };
  return <Empty title={text[filter]} icon="jobs" />;
}

function startedText(job: JobView, now: number) {
  const attempt = currentAttempt(job);
  if (jobGroup(job) === 'queued') {
    return <span>waiting {formatDuration(Math.max(now - job.created_at, 0))}</span>;
  }
  const at = attempt?.started_at ?? attempt?.offered_at;
  if (at === null || at === undefined) return <span className="muted">never started</span>;
  return <Ago at={at} now={now} />;
}

function durationText(job: JobView, now: number) {
  const attempt = currentAttempt(job);
  const started = attempt?.started_at;
  if (started === null || started === undefined) return <span className="muted">—</span>;
  const end = attempt?.exited_at;
  if (end === null || end === undefined) {
    return job.phase === 'finished' ? (
      <span className="muted">—</span>
    ) : (
      <span>{formatDuration(Math.max(now - started, 0))} so far</span>
    );
  }
  return <span>{formatDuration(Math.max(end - started, 0))}</span>;
}

function JobTable({ jobs }: { jobs: JobView[] }) {
  const now = useNow(1000);
  return (
    <div className="table-wrap">
      <table className="table table-stack">
        <thead>
          <tr>
            <th scope="col">Status</th>
            <th scope="col">Job</th>
            <th scope="col">Project</th>
            <th scope="col">Machine</th>
            <th scope="col">Started</th>
            <th scope="col">Duration</th>
          </tr>
        </thead>
        <tbody>
          {jobs.map((job) => {
            const attempt = currentAttempt(job);
            return (
              <tr key={job.id}>
                <td data-label="Status">
                  <JobStatus job={job} />
                </td>
                <td data-label="Job">
                  <JobLink id={job.id} recipe={job.recipe} />
                </td>
                <td data-label="Project">{job.project_name}</td>
                <td data-label="Machine">
                  {attempt ? (
                    <Link to={`/machines/${attempt.machine_id}`}>{attempt.machine_name}</Link>
                  ) : (
                    <span className="muted">—</span>
                  )}
                </td>
                <td data-label="Started" className="nowrap">
                  {startedText(job, now)}
                </td>
                <td data-label="Duration" className="nowrap">
                  {durationText(job, now)}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
