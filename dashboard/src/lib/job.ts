// What a job's state means, in words. The result of a job and the release of
// its resources are separate facts and are described separately: tests can
// have passed while cleanup is still blocked.

import type { AttemptView, JobSpec, JobView, Outcome } from '../api/schemas';
import { HOLDING_PHASES, phaseDetail, selectorLabel, type Tone } from './labels';

export function currentAttempt(job: JobView): AttemptView | undefined {
  let current: AttemptView | undefined;
  for (const attempt of job.attempts) {
    if (!current || attempt.generation > current.generation) current = attempt;
  }
  return current;
}

export interface OutcomeText {
  label: string;
  detail?: string;
  tone: Tone;
}

export function describeOutcome(outcome: Outcome | null | undefined): OutcomeText {
  if (!outcome) return { label: 'No result yet', tone: 'neutral' };
  switch (outcome.type) {
    case 'succeeded':
      return { label: 'Passed', tone: 'ok' };
    case 'failed': {
      const where = outcome.step ? `step "${outcome.step}"` : 'a step';
      const code =
        outcome.exit_code === null || outcome.exit_code === undefined
          ? 'failed'
          : `exited with code ${outcome.exit_code}`;
      return { label: 'Failed', detail: `${where} ${code}`, tone: 'bad' };
    }
    case 'cancelled':
      return { label: 'Cancelled', tone: 'warn' };
    case 'timed_out':
      return {
        label: 'Timed out',
        detail: outcome.step ? `step "${outcome.step}" ran past its time limit` : undefined,
        tone: 'bad',
      };
    case 'resource_exhausted':
      return { label: 'Out of resources', detail: outcome.detail, tone: 'bad' };
    case 'infra_interrupted':
      return {
        label: 'Interrupted by infrastructure',
        detail: `${outcome.detail} (not a failure of the build or its tests)`,
        tone: 'warn',
      };
    case 'rejected':
      return { label: 'Rejected', detail: outcome.detail, tone: 'bad' };
    case 'unknown':
      return {
        label: outcome.raw,
        detail: 'This dashboard does not know this result type.',
        tone: 'neutral',
      };
  }
}

export interface CleanupText {
  label: string;
  detail: string;
  tone: Tone;
  /** Cleanup is confirmed, or there was never anything to clean up. */
  done: boolean;
}

export function describeCleanup(cleanup: string, phase: string): CleanupText {
  switch (cleanup) {
    case 'released':
      return {
        label: 'Released',
        detail: 'Cleanup was confirmed and the resources are free.',
        tone: 'ok',
        done: true,
      };
    case 'quarantined':
      return {
        label: 'Quarantined',
        detail:
          'Cleanup could not be verified. The capacity stays held on the machine until someone resolves it.',
        tone: 'bad',
        done: false,
      };
    case 'not_started':
      if (phase === 'finished') {
        return {
          label: 'Nothing to clean up',
          detail: 'The job ended without holding resources.',
          tone: 'neutral',
          done: true,
        };
      }
      return {
        label: 'Not started',
        detail: HOLDING_PHASES.has(phase)
          ? 'The job still holds its resources.'
          : 'The job holds no resources yet.',
        tone: 'neutral',
        done: false,
      };
    case 'requested':
      return {
        label: 'Cleanup requested',
        detail: 'Waiting for the worker to acknowledge. The resources are still held.',
        tone: 'warn',
        done: false,
      };
    case 'worker_acknowledged':
      return {
        label: 'Stopping processes',
        detail: 'The worker acknowledged and is stopping the processes. The resources are still held.',
        tone: 'warn',
        done: false,
      };
    case 'processes_stopped':
      return {
        label: 'Processes stopped',
        detail: 'The workspace is being removed. The resources are still held.',
        tone: 'warn',
        done: false,
      };
    case 'cleanup_complete':
      return {
        label: 'Cleanup complete',
        detail: 'Waiting for the release to be recorded. The resources are still held.',
        tone: 'warn',
        done: false,
      };
    default:
      return {
        label: cleanup,
        detail: 'This dashboard does not know this cleanup state; assume the resources are still held.',
        tone: 'warn',
        done: false,
      };
  }
}

export interface JobSummary {
  outcome: OutcomeText;
  cleanup: CleanupText;
  /** Finished, with cleanup confirmed: nothing more will change. */
  settled: boolean;
  /** One sentence covering both the result and the state of the resources. */
  headline: string;
}

/**
 * Summarise a job so that a passed job whose cleanup is pending or
 * quarantined never reads the same as one that is settled.
 */
export function summarizeJob(job: JobView): JobSummary {
  const outcome = describeOutcome(job.outcome);
  const cleanup = describeCleanup(job.cleanup, job.phase);
  const settled = job.phase === 'finished' && cleanup.done;

  let resources: string;
  if (settled) {
    resources =
      job.cleanup === 'released'
        ? 'Cleanup is confirmed and its resources are released.'
        : 'It held no resources.';
  } else if (job.cleanup === 'quarantined' || job.phase === 'quarantined') {
    resources = 'Cleanup could not be verified, so its capacity stays held.';
  } else if (job.outcome) {
    resources = 'Cleanup is still in progress, so its resources are not free yet.';
  } else if (job.cancel_requested) {
    resources = 'Cancellation was requested; its resources are freed only after cleanup is confirmed.';
  } else {
    resources = phaseDetail(job.phase);
  }

  const result = job.outcome
    ? `${outcome.label}${outcome.detail ? `: ${outcome.detail}` : ''}.`
    : 'No result yet.';
  return { outcome, cleanup, settled, headline: `${result} ${resources}` };
}

/** May a cancellation still be requested? */
export function canCancel(job: JobView): boolean {
  return job.phase !== 'finished' && !job.cancel_requested && !job.outcome;
}

/** The platform constraints of a job, one short sentence each. */
export function describeConstraints(spec: JobSpec): string[] {
  const lines: string[] = [];
  const { compatibility, placement } = spec;

  if (compatibility.build_hosts.length === 0) {
    lines.push('Builds only on the platform it was submitted from');
  } else {
    lines.push(`Builds on ${compatibility.build_hosts.map(selectorLabel).join(' or ')}`);
  }
  if (compatibility.test_runtime) {
    lines.push(`Tests run on ${selectorLabel(compatibility.test_runtime)}`);
  }
  if (compatibility.artifact_target) lines.push(`Target ${compatibility.artifact_target}`);
  for (const need of compatibility.requires) {
    lines.push(`Needs ${need.name}${need.version ? ` ${need.version}` : ''}`);
  }
  if (compatibility.isolation !== 'native') lines.push(`Isolation: ${compatibility.isolation}`);

  switch (placement.type) {
    case 'any':
      break;
    case 'prefer':
      lines.push(`Prefers ${selectorLabel(placement.platform)}`);
      break;
    case 'require':
      lines.push(`Requires ${selectorLabel(placement.platform)}`);
      break;
    case 'machine':
      lines.push(`Pinned to machine ${placement.machine}`);
      break;
    case 'pool':
      lines.push(`Pool ${placement.pool}`);
      break;
    case 'unknown':
      lines.push(`Placement: ${placement.raw}`);
      break;
  }
  if (spec.source_lan_only) lines.push('Source may not leave the LAN');
  return lines;
}

export function sessionLabel(job: JobView): string {
  const session = job.session;
  if (!session) return 'no session';
  const name = session.label ?? session.id;
  return session.agent ? `${name} (${session.agent})` : name;
}
