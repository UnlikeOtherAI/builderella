// One job: a header with its status, key times and the Cancel action, then
// tabs for the summary, the steps, the log, the artifacts and the details.

import { useState } from 'react';
import { Link, useParams, useSearchParams } from 'react-router';
import { useArtifacts, useCancelJob, useJob, useNow, useWhoAmI } from '../api/hooks';
import type { ArtifactView, AttemptView, JobView, StepView } from '../api/schemas';
import { JobStatus } from '../components/bits';
import { Icon } from '../components/icons';
import { JobResult } from '../components/JobResult';
import { LogViewer } from '../components/LogViewer';
import { QueueReasonView } from '../components/QueueReasonView';
import {
  Ago,
  Callout,
  Card,
  Chip,
  ConfirmDialog,
  Empty,
  ErrorPanel,
  Fact,
  Facts,
  Help,
  Ident,
  Loadable,
  PageHeader,
  StatusBadge,
  TabPanel,
  Tabs,
} from '../components/ui';
import {
  formatBytes,
  formatCpu,
  formatDuration,
  formatPercent,
  formatResources,
  formatTimestamp,
} from '../lib/format';
import { canCancel, currentAttempt, describeCleanup, describeConstraints, sessionLabel } from '../lib/job';
import { phaseDetail, priorityLabel, stepKindLabel, stepStateLabel, stepStateTone } from '../lib/labels';
import { jobStatus, outcomeStatus, phaseStatus } from '../lib/status';

export const JOB_TABS = ['overview', 'steps', 'log', 'artifacts', 'details'] as const;
type JobTab = (typeof JOB_TABS)[number];

const TAB_LABEL: Record<JobTab, string> = {
  overview: 'Overview',
  steps: 'Steps',
  log: 'Log',
  artifacts: 'Artifacts',
  details: 'Details',
};

export function JobDetailView() {
  const { id = '' } = useParams();
  const job = useJob(id);
  return (
    <>
      <nav className="crumb" aria-label="Breadcrumb">
        <Link to="/jobs">Jobs</Link>
        <Icon name="chevronRight" size={14} />
      </nav>
      <Loadable query={job} what="the job" lines={8}>
        {(data) => <JobBody key={data.id} job={data} />}
      </Loadable>
    </>
  );
}

function JobBody({ job }: { job: JobView }) {
  const now = useNow(1000);
  const [params, setParams] = useSearchParams();
  const asked = params.get('tab');
  const tab: JobTab = (JOB_TABS as readonly string[]).includes(asked ?? '') ? (asked as JobTab) : 'overview';
  const running = job.phase !== 'finished' && job.phase !== 'quarantined';
  const [follow, setFollow] = useState(running);
  const [logStep, setLogStep] = useState('');
  const current = currentAttempt(job);
  const artifacts = useArtifacts(job.id);
  const stepNames = job.spec
    ? job.spec.steps.map((step) => step.name)
    : [...new Set(job.attempts.flatMap((attempt) => attempt.steps.map((step) => step.name)))];

  const select = (next: string) => {
    const merged = new URLSearchParams(params);
    if (next === 'overview') merged.delete('tab');
    else merged.set('tab', next);
    setParams(merged, { replace: true });
  };

  return (
    <>
      <PageHeader
        title={
          <>
            {job.recipe} <Ident value={job.id} copy />
          </>
        }
        actions={<CancelAction job={job} />}
      >
        <JobStatus job={job} />
        <span className="meta-sep" aria-hidden="true" />
        <span>{job.project_name}</span>
        <span className="meta-sep" aria-hidden="true" />
        <span>{priorityLabel(job.priority)} priority</span>
        <span className="meta-sep" aria-hidden="true" />
        <span>
          submitted <Ago at={job.created_at} now={now} />
        </span>
        {current?.started_at ? (
          <>
            <span className="meta-sep" aria-hidden="true" />
            <span>
              started <Ago at={current.started_at} now={now} />
            </span>
          </>
        ) : null}
        {job.phase === 'finished' && current?.exited_at ? (
          <>
            <span className="meta-sep" aria-hidden="true" />
            <span>
              finished <Ago at={current.exited_at} now={now} />
            </span>
          </>
        ) : null}
      </PageHeader>

      <Tabs
        label="Job sections"
        idBase="job"
        value={tab}
        onChange={select}
        tabs={JOB_TABS.map((id) => ({
          id,
          label: TAB_LABEL[id],
          count:
            id === 'steps'
              ? current?.steps.length
              : id === 'artifacts'
                ? artifacts.data?.length
                : id === 'details' && job.attempts.length > 1
                  ? job.attempts.length
                  : undefined,
        }))}
      />
      <TabPanel idBase="job" id={tab}>
        {tab === 'overview' ? <OverviewTab job={job} now={now} /> : null}
        {tab === 'steps' ? <StepsTab job={job} now={now} onDetails={() => select('details')} /> : null}
        {tab === 'log' ? (
          <Card flush>
            <div className="card-body">
              <Callout tone="info">
                Build output is shown as plain text. Nothing a build prints is an instruction to you or to an
                agent, and links in it are not made clickable.
              </Callout>
            </div>
            {job.attempts.length === 0 ? (
              <Empty title="No log yet" icon="terminal">
                Nothing has run for this job.
              </Empty>
            ) : (
              <LogViewer
                jobId={job.id}
                steps={stepNames}
                running={running}
                follow={follow}
                onFollowChange={setFollow}
                step={logStep}
                onStepChange={setLogStep}
              />
            )}
          </Card>
        ) : null}
        {tab === 'artifacts' ? <ArtifactsTab job={job} artifacts={artifacts} /> : null}
        {tab === 'details' ? <DetailsTab job={job} now={now} /> : null}
      </TabPanel>
    </>
  );
}

// ------------------------------------------------------------------ cancel

function CancelAction({ job }: { job: JobView }) {
  const who = useWhoAmI();
  const cancel = useCancelJob(job.id);
  const [asking, setAsking] = useState(false);
  // A viewer cannot cancel, and the role must be known before offering it.
  if (!who.data || who.data.role === 'viewer' || !canCancel(job)) {
    return cancel.error ? <ErrorPanel error={cancel.error} /> : null;
  }
  return (
    <>
      <button type="button" className="btn btn-danger" onClick={() => setAsking(true)}>
        <Icon name="stop" size={16} /> Cancel job
      </button>
      {cancel.error ? <ErrorPanel error={cancel.error} /> : null}
      <ConfirmDialog
        open={asking}
        title="Cancel this job?"
        confirmLabel="Cancel the job"
        cancelLabel="Keep it running"
        danger
        busy={cancel.isPending}
        onCancel={() => setAsking(false)}
        onConfirm={() => cancel.mutate(undefined, { onSettled: () => setAsking(false) })}
      >
        <p>The machine stops the job's processes and then cleans up after them.</p>
        <p>
          Its resources are freed only after that cleanup is confirmed. Until then the machine keeps them reserved,
          and this page shows the cleanup state.
        </p>
      </ConfirmDialog>
    </>
  );
}

// ---------------------------------------------------------------- overview

function OverviewTab({ job, now }: { job: JobView; now: number }) {
  const current = currentAttempt(job);
  const cleanup = describeCleanup(job.cleanup, job.phase);
  return (
    <div className="stack-gap">
      {job.cancel_requested ? (
        <Callout tone="warn" role="status" title="Cancellation requested">
          That is a request, not proof that anything stopped: the job's resources stay held until cleanup is
          confirmed. Cleanup now: {cleanup.label.toLowerCase()}.
        </Callout>
      ) : null}
      <JobResult job={job} />
      {job.queue_reason ? (
        <Card title="Why it is waiting">
          <QueueReasonView reason={job.queue_reason} />
        </Card>
      ) : null}
      {current ? <AttemptSummary attempt={current} now={now} attempts={job.attempts.length} /> : null}
      <p className="muted small">
        {job.outcome ? 'This result applies' : 'This job applies'} to snapshot{' '}
        <Ident value={job.snapshot_id} copy /> only. Edits made to the working tree after that snapshot are not part
        of it.
      </p>
    </div>
  );
}

function ranUntil(attempt: AttemptView, now: number): number {
  return attempt.exited_at ?? attempt.released_at ?? now;
}

function AttemptSummary({ attempt, now, attempts }: { attempt: AttemptView; now: number; attempts: number }) {
  const until = ranUntil(attempt, now);
  return (
    <Card
      title={attempts > 1 ? `Attempt ${attempt.generation} on ${attempt.machine_name}` : `On ${attempt.machine_name}`}
      description={attempts > 1 ? `Earlier attempts are under Details.` : undefined}
      actions={<StatusBadge status={phaseStatus(attempt.phase)} />}
    >
      {attempt.uncertain ? (
        <Callout tone="warn" title="The machine is out of contact">
          The job may still be running there, so its reservation stays held until the machine reports again or is
          known to have restarted.
        </Callout>
      ) : null}
      <Facts columns>
        <Fact label="Machine">
          <Link to={`/machines/${attempt.machine_id}`}>{attempt.machine_name}</Link>
        </Fact>
        <Fact label="Offered">
          <Ago at={attempt.offered_at} now={now} />
        </Fact>
        <Fact label="Ran for">
          {attempt.started_at
            ? `${formatDuration(Math.max(until - attempt.started_at, 0))}${attempt.exited_at ? '' : ' so far'}`
            : 'not started'}
        </Fact>
        <Fact label="Resources released">
          {attempt.released_at ? (
            <Ago at={attempt.released_at} now={now} />
          ) : attempt.exited_at ? (
            `not yet, ${formatDuration(Math.max(now - attempt.exited_at, 0))} after it finished`
          ) : (
            'not yet'
          )}
        </Fact>
      </Facts>
      <Resources attempt={attempt} until={until} />
    </Card>
  );
}

function Resources({ attempt, until }: { attempt: AttemptView; until: number }) {
  const usage = attempt.usage ?? null;
  const reserved = attempt.reserved;
  if (!usage) {
    return (
      <p className="muted">
        Reserved {formatResources(reserved)}. No use has been measured yet.
      </p>
    );
  }
  const wallMs = attempt.started_at ? Math.max(until - attempt.started_at, 0) : 0;
  const averageCpu = wallMs > 0 ? (usage.cpu_time_ms / wallMs) * 1000 : null;
  const peakShare = reserved.memory_bytes > 0 ? usage.memory_peak_bytes / reserved.memory_bytes : null;
  const over = Math.max(usage.memory_peak_bytes - reserved.memory_bytes, 0);
  return (
    <div className="table-wrap">
      <table className="table table-compact table-stack">
        <caption className="table-caption">Reserved and used</caption>
        <thead>
          <tr>
            <th scope="col">Resource</th>
            <th scope="col">Reserved</th>
            <th scope="col">Used</th>
          </tr>
        </thead>
        <tbody>
          <tr>
            <th scope="row" data-label="Resource">
              Memory
            </th>
            <td data-label="Reserved">{formatBytes(reserved.memory_bytes)}</td>
            <td data-label="Used">
              peak {formatBytes(usage.memory_peak_bytes)}
              {peakShare === null ? '' : ` (${formatPercent(peakShare)})`}, now {formatBytes(usage.memory_current_bytes)}
              {over > 0 ? (
                <>
                  {' '}
                  <Chip tone="bad">peak {formatBytes(over)} over</Chip>
                </>
              ) : null}
            </td>
          </tr>
          <tr>
            <th scope="row" data-label="Resource">
              CPU
            </th>
            <td data-label="Reserved">{formatCpu(reserved.cpu_milli)}</td>
            <td data-label="Used">
              {formatDuration(usage.cpu_time_ms)} of CPU time
              {averageCpu === null ? '' : `, ${formatCpu(averageCpu)} on average`}
            </td>
          </tr>
          <tr>
            <th scope="row" data-label="Resource">
              Scratch disk
            </th>
            <td data-label="Reserved">{formatBytes(reserved.disk_bytes)}</td>
            <td data-label="Used">{formatBytes(usage.disk_bytes)}</td>
          </tr>
          <tr>
            <th scope="row" data-label="Resource">
              Processes
            </th>
            <td data-label="Reserved">no reservation</td>
            <td data-label="Used">{usage.processes} running</td>
          </tr>
        </tbody>
      </table>
    </div>
  );
}

// ------------------------------------------------------------------- steps

function stepDuration(step: StepView, now: number): string {
  if (step.started_at === null || step.started_at === undefined) return '—';
  if (step.finished_at === null || step.finished_at === undefined) {
    return `${formatDuration(Math.max(now - step.started_at, 0))} so far`;
  }
  return formatDuration(Math.max(step.finished_at - step.started_at, 0));
}

function StepTable({ steps, now, caption }: { steps: StepView[]; now: number; caption: string }) {
  return (
    <div className="table-wrap">
      <table className="table table-stack">
        <caption className="sr-only">{caption}</caption>
        <thead>
          <tr>
            <th scope="col">Step</th>
            <th scope="col">State</th>
            <th scope="col">Kind</th>
            <th scope="col">Exit code</th>
            <th scope="col">Duration</th>
          </tr>
        </thead>
        <tbody>
          {steps.map((step) => (
            <tr key={step.name}>
              <th scope="row" data-label="Step">
                <code>{step.name}</code>
              </th>
              <td data-label="State">
                <StatusBadge
                  status={{
                    key: step.state,
                    label: stepStateLabel(step.state),
                    tone: stepStateTone(step.state),
                    precise: step.state,
                  }}
                  live={step.state === 'running'}
                />
              </td>
              <td data-label="Kind">{stepKindLabel(step.kind)}</td>
              <td data-label="Exit code" className="num">
                {step.exit_code === null || step.exit_code === undefined ? '—' : step.exit_code}
              </td>
              <td data-label="Duration" className="nowrap">
                {stepDuration(step, now)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function StepsTab({ job, now, onDetails }: { job: JobView; now: number; onDetails: () => void }) {
  const current = currentAttempt(job);
  if (!current || current.steps.length === 0) {
    return (
      <Card>
        <Empty title="No steps yet" icon="jobs">
          {job.phase === 'finished'
            ? 'The job ended before any step ran.'
            : 'Steps appear once a machine starts the job.'}
        </Empty>
      </Card>
    );
  }
  return (
    <Card
      flush
      title={job.attempts.length > 1 ? `Steps of attempt ${current.generation}` : 'Steps'}
      actions={
        job.attempts.length > 1 ? (
          <button type="button" className="btn btn-small" onClick={onDetails}>
            Earlier attempts
          </button>
        ) : null
      }
    >
      <StepTable steps={current.steps} now={now} caption="Steps of the current attempt" />
    </Card>
  );
}

// --------------------------------------------------------------- artifacts

function ArtifactsTab({ job, artifacts }: { job: JobView; artifacts: ReturnType<typeof useArtifacts> }) {
  const generationOf = (artifact: ArtifactView) =>
    job.attempts.find((attempt) => attempt.id === artifact.attempt_id)?.generation;
  return (
    <Card flush>
      <Loadable query={artifacts} what="artifacts">
        {(list) =>
          list.length === 0 ? (
            <Empty title="No artifacts" icon="file">
              {job.phase === 'finished'
                ? 'This job published no files.'
                : 'Files the job publishes appear here once it has collected them.'}
            </Empty>
          ) : (
            <div className="table-wrap">
              <table className="table table-stack">
                <caption className="sr-only">Artifacts of this job</caption>
                <thead>
                  <tr>
                    <th scope="col">File</th>
                    <th scope="col">Kind</th>
                    <th scope="col">Size</th>
                    <th scope="col">SHA-256</th>
                    <th scope="col">From</th>
                  </tr>
                </thead>
                <tbody>
                  {list.map((artifact) => {
                    const generation = generationOf(artifact);
                    return (
                      <tr key={artifact.id}>
                        <th scope="row" data-label="File">
                          <code className="wrap">{artifact.path}</code>
                        </th>
                        <td data-label="Kind">
                          {artifact.kind === 'test_report'
                            ? 'test report'
                            : artifact.kind === 'artifact'
                              ? 'file'
                              : artifact.kind}
                        </td>
                        <td data-label="Size" className="num">
                          {formatBytes(artifact.size)}
                        </td>
                        <td data-label="SHA-256">
                          <Ident value={artifact.digest} copy />
                        </td>
                        <td data-label="From">
                          {generation === undefined ? 'an attempt' : `attempt ${generation}`}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )
        }
      </Loadable>
    </Card>
  );
}

// ----------------------------------------------------------------- details

function DetailsTab({ job, now }: { job: JobView; now: number }) {
  const attempts = [...job.attempts].sort((a, b) => b.generation - a.generation);
  const current = currentAttempt(job);
  const status = jobStatus(job);
  return (
    <div className="stack-gap">
      <Card title="Job">
        <Facts columns>
          <Fact label="Job id">
            <Ident value={job.id} copy />
          </Fact>
          <Fact label="State">
            {status.precise} <span className="muted">({phaseDetail(job.phase)})</span>
          </Fact>
          <Fact label="Project">{job.project_name}</Fact>
          <Fact label="Recipe">{job.recipe}</Fact>
          <Fact label="Submitted by">{sessionLabel(job)}</Fact>
          <Fact label="Priority">{priorityLabel(job.priority)}</Fact>
          <Fact label="Submitted">{formatTimestamp(job.created_at)}</Fact>
          <Fact label="Last change">
            <Ago at={job.updated_at} now={now} />
          </Fact>
          <Fact
            label={
              <>
                Snapshot{' '}
                <Help label="What the snapshot is">
                  The exact files the job was given, captured when it was submitted. Later edits never change what
                  it builds.
                </Help>
              </>
            }
          >
            <Ident value={job.snapshot_id} copy />
          </Fact>
          <Fact label="Asks for (peak)">{formatResources(job.envelope.peak)}</Fact>
          <Fact label="May grow to">{formatResources(job.envelope.maximum)}</Fact>
          {job.spec ? (
            <Fact label="Attempts allowed">
              {job.spec.max_attempts} ({job.attempts.length} used)
            </Fact>
          ) : null}
          {job.environment_id ? (
            <Fact label="Service">
              <Link to={`/services/${job.environment_id}`}>
                <Ident value={job.environment_id} />
              </Link>
            </Fact>
          ) : null}
        </Facts>
        {job.spec ? (
          <>
            <h3 className="subhead">Where it may run</h3>
            <ul className="plain-list">
              {describeConstraints(job.spec).map((line) => (
                <li key={line}>{line}</li>
              ))}
            </ul>
          </>
        ) : null}
      </Card>

      <Card title="Attempts" description="Each time the job was given to a machine.">
        {attempts.length === 0 ? (
          <Empty title="No attempt yet" icon="machines">
            {job.phase === 'finished'
              ? 'The job ended before it was given to a machine, so nothing ran.'
              : 'The job has not been given to a machine yet.'}
          </Empty>
        ) : (
          <ol className="attempts">
            {attempts.map((attempt) => (
              <AttemptHistory key={attempt.id} attempt={attempt} current={attempt.id === current?.id} now={now} />
            ))}
          </ol>
        )}
      </Card>
    </div>
  );
}

function AttemptHistory({ attempt, current, now }: { attempt: AttemptView; current: boolean; now: number }) {
  const cleanup = describeCleanup(attempt.cleanup, attempt.phase);
  const status = attempt.outcome ? outcomeStatus(attempt.outcome) : phaseStatus(attempt.phase);
  const until = ranUntil(attempt, now);
  return (
    <li>
      <details className="disclosure attempt" open={current}>
        <summary>
          <strong>
            Attempt {attempt.generation} on {attempt.machine_name}
          </strong>
          <StatusBadge status={status} />
          <span className="muted">{current ? 'current' : 'replaced by a later attempt'}</span>
        </summary>
        <div className="attempt-body">
          {current ? null : <p className="muted">A later attempt replaced this one. Its result is not the job's.</p>}
          {!attempt.start_committed ? (
            <p className="muted">
              Not yet allowed to start: the machine holds the resources but runs nothing.
            </p>
          ) : null}
          <Facts columns>
            <Fact label="Result">{status.precise}</Fact>
            <Fact label="Cleanup">
              {cleanup.label} <span className="muted">({cleanup.detail})</span>
            </Fact>
            <Fact label="Offered">{formatTimestamp(attempt.offered_at)}</Fact>
            <Fact label="Started">{attempt.started_at ? formatTimestamp(attempt.started_at) : 'not started'}</Fact>
            <Fact label="Ran for">
              {attempt.started_at
                ? `${formatDuration(Math.max(until - attempt.started_at, 0))}${attempt.exited_at ? '' : ' so far'}`
                : '—'}
            </Fact>
            <Fact label="Finished">{attempt.exited_at ? formatTimestamp(attempt.exited_at) : 'not yet'}</Fact>
            <Fact label="Resources released">
              {attempt.released_at
                ? `${formatTimestamp(attempt.released_at)}${attempt.exited_at ? `, ${formatDuration(Math.max(attempt.released_at - attempt.exited_at, 0))} after it finished` : ''}`
                : 'not yet'}
            </Fact>
            <Fact label="Out of contact">{attempt.uncertain ? 'yes: it may still be running' : 'no'}</Fact>
          </Facts>
          {attempt.steps.length > 0 ? (
            <StepTable steps={attempt.steps} now={now} caption={`Steps of attempt ${attempt.generation}`} />
          ) : (
            <p className="muted">No step was reported for this attempt.</p>
          )}
          <Resources attempt={attempt} until={until} />
        </div>
      </details>
    </li>
  );
}
