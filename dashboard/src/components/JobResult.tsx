import type { JobView } from '../api/schemas';
import { summarizeJob } from '../lib/job';
import { Chip } from './ui';

/**
 * The result of a job and the state of its resources, as two separate
 * statements. A job that passed but has not been cleaned up is marked as
 * not settled, so it cannot be read as finished business.
 */
export function JobResult({ job }: { job: JobView }) {
  const summary = summarizeJob(job);
  return (
    <section
      className={`result ${summary.settled ? 'result-settled' : 'result-open'}`}
      aria-label="Result and cleanup"
    >
      <p className="result-headline">{summary.headline}</p>
      <div className="result-grid">
        <div>
          <h3>Result</h3>
          <Chip tone={summary.outcome.tone}>{summary.outcome.label}</Chip>
          {summary.outcome.detail ? <p className="muted">{summary.outcome.detail}</p> : null}
        </div>
        <div>
          <h3>Cleanup and resources</h3>
          <Chip tone={summary.cleanup.tone}>{summary.cleanup.label}</Chip>
          <p className="muted">{summary.cleanup.detail}</p>
        </div>
        <div>
          <h3>Overall</h3>
          <Chip tone={summary.settled ? 'ok' : 'warn'}>{summary.settled ? 'Settled' : 'Not settled'}</Chip>
          <p className="muted">
            {summary.settled
              ? 'Nothing more will change.'
              : 'The job still owns resources or its cleanup is unconfirmed.'}
          </p>
        </div>
      </div>
    </section>
  );
}

/** The same distinction in one line, for lists. */
export function JobResultChips({ job }: { job: JobView }) {
  const summary = summarizeJob(job);
  if (!job.outcome) return <span className="muted">no result yet</span>;
  return (
    <span className="chips">
      <Chip tone={summary.outcome.tone} title={summary.outcome.detail}>
        {summary.outcome.label}
      </Chip>
      {summary.settled ? null : (
        <Chip tone={summary.cleanup.tone} title={summary.cleanup.detail}>
          {job.cleanup === 'quarantined' ? 'cleanup quarantined' : 'cleanup pending'}
        </Chip>
      )}
    </span>
  );
}
