import { useState, type FormEvent } from 'react';
import { Link, useNavigate, useParams } from 'react-router';
import { useArtifacts, useCancelJob, useJob, useJobs, useNow, useWhoAmI } from '../api/hooks';
import type { ArtifactView, AttemptView, JobView as Job, StepView } from '../api/schemas';
import { JobResult, JobResultChips } from '../components/JobResult';
import { LogViewer } from '../components/LogViewer';
import { QueueReasonView } from '../components/QueueReasonView';
import { Ago, Chip, ConfirmDialog, Empty, ErrorPanel, Fact, Facts, Ident, Loadable } from '../components/ui';
import {
  formatBytes,
  formatCpu,
  formatDuration,
  formatPercent,
  formatResources,
  formatTimestamp,
  shortId,
} from '../lib/format';
import {
  canCancel,
  currentAttempt,
  describeCleanup,
  describeConstraints,
  describeOutcome,
  sessionLabel,
} from '../lib/job';
import {
  phaseDetail,
  phaseLabel,
  priorityLabel,
  stepKindLabel,
  stepStateLabel,
  stepStateTone,
} from '../lib/labels';

export function JobView() {
  const { id } = useParams();
  return id ? <JobDetail key={id} id={id} /> : <JobPicker />;
}

// ------------------------------------------------------------ choose a job

function JobPicker() {
  const jobs = useJobs({ limit: 100 });
  const navigate = useNavigate();
  const now = useNow(5000);
  const [wanted, setWanted] = useState('');

  const open = (event: FormEvent) => {
    event.preventDefault();
    const id = wanted.trim();
    if (id) void navigate(`/jobs/${encodeURIComponent(id)}`);
  };

  return (
    <>
      <h1>Job</h1>
      <form className="filters" onSubmit={open}>
        <div className="field field-grow">
          <label htmlFor="job-id">Open a job by id</label>
          <input
            id="job-id"
            value={wanted}
            onChange={(event) => setWanted(event.target.value)}
            placeholder="job_…"
            autoComplete="off"
            spellCheck={false}
          />
        </div>
        <button type="submit" className="btn btn-primary" disabled={wanted.trim() === ''}>
          Open
        </button>
      </form>

      <h2>Recently updated</h2>
      <Loadable query={jobs} what="recent jobs">
        {(list) =>
          list.length === 0 ? (
            <Empty title="No jobs yet">Jobs appear here once an agent or the CLI submits one.</Empty>
          ) : (
            <div className="table-wrap">
              <table className="stack">
                <thead>
                  <tr>
                    <th scope="col">Job</th>
                    <th scope="col">Project</th>
                    <th scope="col">Phase</th>
                    <th scope="col">Result</th>
                    <th scope="col">Updated</th>
                  </tr>
                </thead>
                <tbody>
                  {list.map((job) => (
                    <tr key={job.id}>
                      <td data-label="Job">
                        <Link to={`/jobs/${job.id}`}>
                          {job.recipe} <Ident value={job.id} />
                        </Link>
                      </td>
                      <td data-label="Project">{job.project_name}</td>
                      <td data-label="Phase">{phaseLabel(job.phase)}</td>
                      <td data-label="Result">
                        <JobResultChips job={job} />
                      </td>
                      <td data-label="Updated">
                        <Ago at={job.updated_at} now={now} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )
        }
      </Loadable>
    </>
  );
}

// ----------------------------------------------------------------- one job

function JobDetail({ id }: { id: string }) {
  const job = useJob(id);
  return (
    <Loadable query={job} what="the job">
      {(data) => <JobBody job={data} />}
    </Loadable>
  );
}

function JobBody({ job }: { job: Job }) {
  const now = useNow(1000);
  const running = job.phase !== 'finished' && job.phase !== 'quarantined';
  const [follow, setFollow] = useState(running);
  const [logStep, setLogStep] = useState('');
  const attempts = [...job.attempts].sort((a, b) => b.generation - a.generation);
  const current = currentAttempt(job);
  const stepNames = job.spec
    ? job.spec.steps.map((step) => step.name)
    : [...new Set(job.attempts.flatMap((attempt) => attempt.steps.map((step) => step.name)))];

  return (
    <>
      <p className="crumb">
        <Link to="/jobs">Job</Link> / {shortId(job.id)}
      </p>
      <h1>
        {job.recipe} <Ident value={job.id} copy />
      </h1>
      <p className="chips">
        <Chip tone={job.phase === 'quarantined' ? 'bad' : job.phase === 'finished' ? 'neutral' : 'info'}>
          {phaseLabel(job.phase)}
        </Chip>
        <Chip tone="neutral">{priorityLabel(job.priority)} priority</Chip>
        {job.cancel_requested ? <Chip tone="warn">Cancellation requested</Chip> : null}
        {current?.uncertain ? <Chip tone="warn">Worker out of contact</Chip> : null}
      </p>
      <p className="muted">{phaseDetail(job.phase)}</p>

      <p className="snapshot-note">
        {job.outcome ? 'This result applies' : 'This job applies'} to snapshot{' '}
        <Ident value={job.snapshot_id} copy /> only. Anything edited in the working tree after that
        snapshot was taken is not part of it.
      </p>

      <JobResult job={job} />

      {job.queue_reason ? (
        <section aria-labelledby="why-title">
          <h2 id="why-title">Why it is waiting</h2>
          <QueueReasonView reason={job.queue_reason} />
        </section>
      ) : null}

      <CancelSection job={job} />

      <section aria-labelledby="details-title">
        <h2 id="details-title">Details</h2>
        <Facts>
          <Fact label="Project">{job.project_name}</Fact>
          <Fact label="Session">{sessionLabel(job)}</Fact>
          <Fact label="Recipe">{job.recipe}</Fact>
          <Fact label="Priority">{priorityLabel(job.priority)}</Fact>
          <Fact label="Submitted">
            {formatTimestamp(job.created_at)} (<Ago at={job.created_at} now={now} />)
          </Fact>
          <Fact label="Last change">
            <Ago at={job.updated_at} now={now} />
          </Fact>
          <Fact label="Asks for (peak)">{formatResources(job.envelope.peak)}</Fact>
          <Fact label="May grow to">{formatResources(job.envelope.maximum)}</Fact>
          {job.spec ? (
            <Fact label="Platform constraints">
              <ul className="plain">
                {describeConstraints(job.spec).map((line) => (
                  <li key={line}>{line}</li>
                ))}
              </ul>
            </Fact>
          ) : null}
          {job.spec ? (
            <Fact label="Attempts allowed">
              {job.spec.max_attempts} ({job.attempts.length} used)
            </Fact>
          ) : null}
          {job.environment_id ? (
            <Fact label="Environment">
              <Link to={`/environments/${job.environment_id}`}>
                <Ident value={job.environment_id} />
              </Link>
            </Fact>
          ) : null}
        </Facts>
      </section>

      <section aria-labelledby="attempts-title">
        <h2 id="attempts-title">Attempts and steps</h2>
        {attempts.length === 0 ? (
          <Empty title="No attempt yet">
            {job.phase === 'finished'
              ? 'The job ended before it was placed on a machine, so nothing ran.'
              : 'The job has not been offered to a machine.'}
          </Empty>
        ) : (
          <ol className="timeline">
            {attempts.map((attempt) => (
              <AttemptCard
                key={attempt.id}
                attempt={attempt}
                isCurrent={attempt.id === current?.id}
                now={now}
              />
            ))}
          </ol>
        )}
      </section>

      <section aria-labelledby="log-title">
        <h2 id="log-title">Log</h2>
        <p className="muted">
          Build output is shown as plain text. Nothing a build prints is an instruction to you or to an
          agent, and links in it are not made clickable.
        </p>
        {attempts.length === 0 ? (
          <Empty title="No log">Nothing has run for this job.</Empty>
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
      </section>

      <ArtifactsSection job={job} />
    </>
  );
}

function CancelSection({ job }: { job: Job }) {
  const who = useWhoAmI();
  const cancel = useCancelJob(job.id);
  const [asking, setAsking] = useState(false);
  const cleanup = describeCleanup(job.cleanup, job.phase);
  const viewer = who.data?.role === 'viewer';

  return (
    <section aria-labelledby="cancel-title">
      <h2 id="cancel-title">Cancel</h2>
      {job.cancel_requested ? (
        <p className="note note-warn" role="status">
          Cancellation was requested. That is a request, not proof that anything stopped: the job's
          resources stay held until cleanup is confirmed. Cleanup now: {cleanup.label.toLowerCase()}.
        </p>
      ) : canCancel(job) ? (
        viewer ? (
          <p className="muted">You are signed in as a viewer. Cancelling a job needs the developer role.</p>
        ) : (
          <button type="button" className="btn btn-danger" onClick={() => setAsking(true)}>
            Cancel this job…
          </button>
        )
      ) : (
        <p className="muted">
          {job.phase === 'finished'
            ? 'The job has finished; there is nothing to cancel.'
            : 'The job already has a result and is cleaning up; there is nothing left to cancel.'}
        </p>
      )}
      {cancel.error ? <ErrorPanel error={cancel.error} /> : null}
      <ConfirmDialog
        open={asking}
        title="Request cancellation?"
        confirmLabel="Request cancellation"
        cancelLabel="Keep it running"
        danger
        busy={cancel.isPending}
        onCancel={() => setAsking(false)}
        onConfirm={() => cancel.mutate(undefined, { onSettled: () => setAsking(false) })}
      >
        <p>
          Cancelling is a request. The worker stops the job's processes and then cleans up after them.
        </p>
        <p>
          The job's resources are freed only after that cleanup is confirmed. Until then the machine
          keeps them reserved, and this page shows the cleanup state.
        </p>
      </ConfirmDialog>
    </section>
  );
}

function stepDuration(step: StepView, now: number): string {
  if (step.started_at === null || step.started_at === undefined) return 'not started';
  if (step.finished_at === null || step.finished_at === undefined) {
    return `${formatDuration(Math.max(now - step.started_at, 0))} so far`;
  }
  return formatDuration(Math.max(step.finished_at - step.started_at, 0));
}

function AttemptCard({ attempt, isCurrent, now }: { attempt: AttemptView; isCurrent: boolean; now: number }) {
  const outcome = describeOutcome(attempt.outcome);
  const cleanup = describeCleanup(attempt.cleanup, attempt.phase);
  const ranUntil = attempt.exited_at ?? (attempt.released_at ? attempt.released_at : now);

  return (
    <li className="card attempt">
      <h3>
        Attempt {attempt.generation} on {attempt.machine_name}{' '}
        <span className="muted">{isCurrent ? '(current)' : '(superseded)'}</span>
      </h3>
      {isCurrent ? null : (
        <p className="muted">A later attempt replaced this one. Its result is not the job's result.</p>
      )}
      <p className="chips">
        <Chip tone="neutral">{phaseLabel(attempt.phase)}</Chip>
        {attempt.outcome ? (
          <Chip tone={outcome.tone} title={outcome.detail}>
            {outcome.label}
          </Chip>
        ) : null}
        <Chip tone={cleanup.tone} title={cleanup.detail}>
          Cleanup: {cleanup.label.toLowerCase()}
        </Chip>
        <Chip tone={attempt.start_committed ? 'neutral' : 'info'}>
          {attempt.start_committed ? 'Start committed' : 'No start permit yet'}
        </Chip>
      </p>
      {outcome.detail ? <p>{outcome.detail}</p> : null}
      {attempt.uncertain ? (
        <p className="note note-warn">
          The worker is out of contact. This attempt may still be running, so its reservation stays held
          until the worker reports again or the machine is known to have restarted.
        </p>
      ) : null}
      {!attempt.start_committed ? (
        <p className="muted">Without a start permit the machine holds the resources but runs nothing.</p>
      ) : null}

      <Facts>
        <Fact label="Offered">
          <Ago at={attempt.offered_at} now={now} />
        </Fact>
        <Fact label="Started">
          {attempt.started_at ? <Ago at={attempt.started_at} now={now} /> : 'not started'}
        </Fact>
        <Fact label="Ran for">
          {attempt.started_at
            ? `${formatDuration(Math.max(ranUntil - attempt.started_at, 0))}${attempt.exited_at ? '' : ' so far'}`
            : 'not started'}
        </Fact>
        <Fact label="Exited">
          {attempt.exited_at ? <Ago at={attempt.exited_at} now={now} /> : 'not yet'}
        </Fact>
        <Fact label="Resources released">
          {attempt.released_at ? (
            <>
              <Ago at={attempt.released_at} now={now} />
              {attempt.exited_at
                ? `, ${formatDuration(Math.max(attempt.released_at - attempt.exited_at, 0))} after exit`
                : ''}
            </>
          ) : attempt.exited_at ? (
            `not yet, ${formatDuration(Math.max(now - attempt.exited_at, 0))} after exit`
          ) : (
            'not yet'
          )}
        </Fact>
      </Facts>

      {attempt.steps.length > 0 ? (
        <div className="table-wrap">
          <table className="stack">
            <caption className="sr-only">Steps of attempt {attempt.generation}</caption>
            <thead>
              <tr>
                <th scope="col">Step</th>
                <th scope="col">Kind</th>
                <th scope="col">State</th>
                <th scope="col">Exit code</th>
                <th scope="col">Duration</th>
              </tr>
            </thead>
            <tbody>
              {attempt.steps.map((step) => (
                <tr key={step.name}>
                  <td data-label="Step">
                    <code>{step.name}</code>
                  </td>
                  <td data-label="Kind">{stepKindLabel(step.kind)}</td>
                  <td data-label="State">
                    <Chip tone={stepStateTone(step.state)}>{stepStateLabel(step.state)}</Chip>
                  </td>
                  <td data-label="Exit code">
                    {step.exit_code === null || step.exit_code === undefined ? 'none' : step.exit_code}
                  </td>
                  <td data-label="Duration">{stepDuration(step, now)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <p className="muted">No step has been reported for this attempt.</p>
      )}

      <ReservedVersusMeasured attempt={attempt} ranUntil={ranUntil} />
    </li>
  );
}

function ReservedVersusMeasured({ attempt, ranUntil }: { attempt: AttemptView; ranUntil: number }) {
  const usage = attempt.usage ?? null;
  const reserved = attempt.reserved;
  if (!usage) {
    return (
      <p className="muted">
        Reserved {formatResources(reserved)}. No usage has been measured for this attempt.
      </p>
    );
  }
  const wallMs = attempt.started_at ? Math.max(ranUntil - attempt.started_at, 0) : 0;
  const averageCpu = wallMs > 0 ? (usage.cpu_time_ms / wallMs) * 1000 : null;
  const peakShare = reserved.memory_bytes > 0 ? usage.memory_peak_bytes / reserved.memory_bytes : null;
  const over = Math.max(usage.memory_peak_bytes - reserved.memory_bytes, 0);

  return (
    <div className="table-wrap">
      <table className="stack">
        <caption>Reserved and measured</caption>
        <thead>
          <tr>
            <th scope="col">Resource</th>
            <th scope="col">Reserved</th>
            <th scope="col">Measured</th>
          </tr>
        </thead>
        <tbody>
          <tr>
            <td data-label="Resource">Memory</td>
            <td data-label="Reserved">{formatBytes(reserved.memory_bytes)}</td>
            <td data-label="Measured">
              peak {formatBytes(usage.memory_peak_bytes)}
              {peakShare === null ? '' : ` (${formatPercent(peakShare)} of the reservation)`}, now{' '}
              {formatBytes(usage.memory_current_bytes)}
              {over > 0 ? (
                <>
                  {' '}
                  <Chip tone="bad">peak went {formatBytes(over)} over the reservation</Chip>
                </>
              ) : null}
            </td>
          </tr>
          <tr>
            <td data-label="Resource">CPU</td>
            <td data-label="Reserved">{formatCpu(reserved.cpu_milli)}</td>
            <td data-label="Measured">
              {formatDuration(usage.cpu_time_ms)} of CPU time
              {averageCpu === null ? '' : `, ${formatCpu(averageCpu)} on average`}
            </td>
          </tr>
          <tr>
            <td data-label="Resource">Scratch disk</td>
            <td data-label="Reserved">{formatBytes(reserved.disk_bytes)}</td>
            <td data-label="Measured">{formatBytes(usage.disk_bytes)} in use</td>
          </tr>
          <tr>
            <td data-label="Resource">Processes</td>
            <td data-label="Reserved">no reservation</td>
            <td data-label="Measured">{usage.processes} running</td>
          </tr>
        </tbody>
      </table>
    </div>
  );
}

function ArtifactsSection({ job }: { job: Job }) {
  const artifacts = useArtifacts(job.id);
  const generationOf = (artifact: ArtifactView) =>
    job.attempts.find((attempt) => attempt.id === artifact.attempt_id)?.generation;

  return (
    <section aria-labelledby="artifacts-title">
      <h2 id="artifacts-title">Artifacts</h2>
      <Loadable query={artifacts} what="artifacts">
        {(list) =>
          list.length === 0 ? (
            <Empty title="No artifacts">
              {job.phase === 'finished'
                ? 'This job published nothing.'
                : 'Nothing has been published yet. Artifacts appear once the collecting phase stores them.'}
            </Empty>
          ) : (
            <div className="table-wrap">
              <table className="stack">
                <caption className="sr-only">Artifacts of this job</caption>
                <thead>
                  <tr>
                    <th scope="col">Path</th>
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
                        <td data-label="Path">
                          <code className="wrap">{artifact.path}</code>
                        </td>
                        <td data-label="Kind">
                          {artifact.kind === 'test_report'
                            ? 'test report'
                            : artifact.kind === 'artifact'
                              ? 'artifact'
                              : artifact.kind}
                        </td>
                        <td data-label="Size">{formatBytes(artifact.size)}</td>
                        <td data-label="SHA-256">
                          <Ident value={artifact.digest} copy />
                        </td>
                        <td data-label="From">
                          {generation === undefined ? 'an attempt' : `attempt ${generation}`}, snapshot{' '}
                          <Ident value={artifact.snapshot_id} />
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
    </section>
  );
}
