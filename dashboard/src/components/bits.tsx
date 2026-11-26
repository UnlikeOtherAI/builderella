// Small pieces several pages share: links to jobs and machines, a job's
// status with its flags, and a machine's platform.

import { Link } from 'react-router';
import type { JobView, MachineView } from '../api/schemas';
import { osLabel } from '../lib/labels';
import { jobFlags, jobStatus, type Status } from '../lib/status';
import { FlagSummary, Ident, StatusBadge } from './ui';

/** The recipe in words and the job's short id, as one link. */
export function JobLink({ id, recipe }: { id: string; recipe?: string }) {
  return (
    <Link to={`/jobs/${id}`} className="job-link">
      {recipe ? <span className="job-recipe">{recipe}</span> : null}
      <Ident value={id} />
    </Link>
  );
}

export function MachineLink({ id, name }: { id: string; name: string }) {
  return <Link to={`/machines/${id}`}>{name}</Link>;
}

const LIVE_KEYS = new Set(['launching', 'running', 'cleaning']);

/** A job's word, with a pulse while it is active, plus its most important flag. */
export function JobStatus({ job, status = jobStatus(job) }: { job: JobView; status?: Status }) {
  return (
    <span className="status-cell">
      <StatusBadge status={status} live={LIVE_KEYS.has(status.key)} />
      <FlagSummary flags={jobFlags(job)} />
    </span>
  );
}

export function Platform({ machine }: { machine: MachineView }) {
  const { platform, os_version } = machine.inventory.os;
  return (
    <span className="platform" title={`${osLabel(platform.os)} ${os_version}, ${platform.arch}`}>
      {osLabel(platform.os)} <span className="muted">{platform.arch}</span>
    </span>
  );
}
