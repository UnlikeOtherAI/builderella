import type { JobView } from '../api/schemas';
import { summarizeJob } from '../lib/job';
import { Chip, Help } from './ui';

/**
 * The result of a job and the state of its resources, as separate
 * statements. A job that passed but has not been cleaned up is marked as
 * not settled, so it cannot be read as finished business.
 */
export function JobResult({ job }: { job: JobView }) {
  const summary = summarizeJob(job);
  // Before there is a result or any cleanup, the three parts would only say
  // "not yet" three times; one sentence says it.
  if (!job.outcome && job.cleanup === 'not_started' && job.phase !== 'quarantined') {
    return (
      <section className="card result result-pending" aria-label="Result and cleanup">
        <p className="result-headline result-only">{summary.headline}</p>
      </section>
    );
  }
  return (
    <section
      className={`card result ${summary.settled ? 'result-settled' : 'result-open'}`}
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
          <h3>
            Cleanup{' '}
            <Help label="Why cleanup is shown apart from the result">
              A job's machine keeps its CPU, memory and disk reserved until it has stopped every process and
              removed the workspace. A job can pass while that is still going on.
            </Help>
          </h3>
          <Chip tone={summary.cleanup.tone}>{summary.cleanup.label}</Chip>
          <p className="muted">{summary.cleanup.detail}</p>
        </div>
        <div>
          <h3>Overall</h3>
          <Chip tone={summary.settled ? 'ok' : 'warn'}>{summary.settled ? 'Settled' : 'Not settled'}</Chip>
          <p className="muted">
            {summary.settled
              ? 'Nothing more will change.'
              : 'The job still holds resources or its cleanup is unconfirmed.'}
          </p>
        </div>
      </div>
    </section>
  );
}
