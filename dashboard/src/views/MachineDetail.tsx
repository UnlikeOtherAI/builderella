// One machine in full: whether it takes jobs and why not, its load split into
// its parts, what runs on it, its health, its policy and its hardware.

import { Link, useParams } from 'react-router';
import { useFleet, useNow, useWhoAmI } from '../api/hooks';
import type { MachineView } from '../api/schemas';
import { JobLink, Platform } from '../components/bits';
import { Icon } from '../components/icons';
import { LoadBreakdown } from '../components/LoadBar';
import {
  Callout,
  Card,
  Chip,
  Empty,
  Fact,
  Facts,
  FlagSummary,
  Help,
  Ident,
  Loadable,
  PageHeader,
  StatusBadge,
} from '../components/ui';
import { machineCapacity } from '../lib/capacity';
import { formatBytes, formatCpu, formatDuration, formatResources, isUnlimited } from '../lib/format';
import {
  lastSeenText,
  lifecycleLabel,
  notAdmittingReasons,
  osLabel,
  powerText,
  profileDetail,
  profileLabel,
} from '../lib/labels';
import { diskFloorBytes } from '../lib/policy';
import { machineFlags, machineStatus, phaseStatus } from '../lib/status';

/** Telemetry older than this is no longer fresh (`TELEMETRY_STALE_AFTER` in bella-core). */
const STALE_AFTER_MS = 15_000;

export function MachineDetailView() {
  const { id = '' } = useParams();
  const fleet = useFleet();
  return (
    <>
      <nav className="crumb" aria-label="Breadcrumb">
        <Link to="/machines">Machines</Link>
        <Icon name="chevronRight" size={14} />
      </nav>
      <Loadable query={fleet} what="the machine" lines={8}>
        {(data) => {
          const machine = data.machines.find((m) => m.id === id || m.name === id);
          if (!machine) {
            return (
              <Card>
                <Empty title="No such machine" icon="machines" action={<Link to="/machines">All machines</Link>}>
                  The fleet has no machine <code>{id}</code>. It may have been removed.
                </Empty>
              </Card>
            );
          }
          return <MachineBody machine={machine} />;
        }}
      </Loadable>
    </>
  );
}

function MachineBody({ machine }: { machine: MachineView }) {
  const now = useNow(5000);
  const who = useWhoAmI();
  const status = machineStatus(machine);
  const reasons = notAdmittingReasons(machine);
  return (
    <>
      <PageHeader
        title={machine.name}
        actions={
          who.data?.role === 'admin' ? (
            <Link to={`/settings?machine=${machine.id}`} className="btn">
              <Icon name="settings" size={16} /> Edit policy
            </Link>
          ) : null
        }
      >
        <StatusBadge status={status} />
        {status.precise !== status.label ? <span className="muted">{status.precise}</span> : null}
        <span className="meta-sep" aria-hidden="true" />
        <Platform machine={machine} />
        <span className="meta-sep" aria-hidden="true" />
        <span>{profileLabel(machine.policy.profile)}</span>
        <span className="meta-sep" aria-hidden="true" />
        <span className="muted">{lastSeenText(machine, now)}</span>
        <FlagSummary flags={machineFlags(machine, status)} max={4} />
      </PageHeader>

      {reasons.length === 0 ? (
        <Callout tone="ok" title="Taking new jobs" />
      ) : (
        <Callout tone={status.tone === 'bad' ? 'bad' : 'warn'} title="Not taking new jobs">
          <ul className="plain-list">
            {reasons.map((reason) => (
              <li key={reason}>{reason}</li>
            ))}
          </ul>
        </Callout>
      )}

      <LoadCard machine={machine} now={now} />
      <RunningCard machine={machine} />
      <div className="grid-2">
        <HealthCard machine={machine} now={now} />
        <PolicyCard machine={machine} />
      </div>
      <HardwareCard machine={machine} />
    </>
  );
}

function LoadCard({ machine, now }: { machine: MachineView; now: number }) {
  const capacity = machineCapacity(machine);
  const telemetry = machine.telemetry ?? null;
  const levels = [machine.enforcement.cpu, machine.enforcement.memory];
  // Anything that is neither a kernel limit nor a bounded guest is treated
  // as policy only, including a level this dashboard does not know.
  const policyOnly = levels.some((level) => level !== 'hard' && level !== 'bounded_guest');
  const guest = !policyOnly && levels.includes('bounded_guest');
  const budget = machine.effective_budget ?? null;
  const sampleAge = telemetry ? Math.max(now - telemetry.sampled_at, 0) : null;
  return (
    <Card
      title="Load"
      description="What the machine's CPU and memory are doing. Jobs only ever get the part not protected for the owner."
    >
      {policyOnly ? (
        <p className="note note-warn">
          Limits here are advisory{' '}
          <Help label="What advisory limits mean">
            The worker keeps jobs within their share by scheduling priority and by reacting to pressure. The
            operating system does not enforce a hard cap, so a job can briefly use more before the worker reacts.
          </Help>
          : jobs are kept within their share by policy, not by the operating system.
        </p>
      ) : null}
      {guest ? (
        <p className="note note-warn">
          The worker runs inside a guest. Its limits hold inside the guest; what the host itself uses is outside
          them.
        </p>
      ) : null}
      {sampleAge !== null && sampleAge > STALE_AFTER_MS ? (
        <p className="note note-warn">
          These are the last measurements received, {formatDuration(sampleAge)} ago. Use may have changed since.
        </p>
      ) : null}
      <div className="grid-2 grid-tight">
        <LoadBreakdown
          label="CPU"
          capacity={capacity.cpu}
          format={formatCpu}
          enforcement={machine.enforcement.cpu}
          verified={machine.enforcement.verified}
        />
        <LoadBreakdown
          label="Memory"
          capacity={capacity.memory}
          format={formatBytes}
          enforcement={machine.enforcement.memory}
          verified={machine.enforcement.verified}
        />
      </div>
      <Facts columns>
        <Fact
          label={
            <>
              Jobs may use at most{' '}
              <Help label="What the managed ceiling is">
                The managed ceiling: everything the machine has, minus the share protected for its owner, and
                capped by its policy.
              </Help>
            </>
          }
        >
          {formatCpu(machine.managed_ceiling.cpu_milli)}, {formatBytes(machine.managed_ceiling.memory_bytes)}
          {isUnlimited(machine.managed_ceiling.disk_bytes) ? '' : `, ${formatBytes(machine.managed_ceiling.disk_bytes)} disk`}
        </Fact>
        <Fact label="Granted right now">
          {budget ? `${formatCpu(budget.cpu_milli)}, ${formatBytes(budget.memory_bytes)}` : 'not reported'}
        </Fact>
        <Fact label="Reserved by jobs">{formatResources(machine.reserved)}</Fact>
        <Fact label="Measured">
          {telemetry && sampleAge !== null
            ? `${formatDuration(sampleAge)} ago, quality ${telemetry.quality}`
            : 'no measurements received'}
        </Fact>
      </Facts>
      <details className="disclosure">
        <summary>Enforcement details from the worker's self-test</summary>
        <Facts columns>
          <Fact label="CPU">{machine.enforcement.cpu}</Fact>
          <Fact label="Memory">{machine.enforcement.memory}</Fact>
          <Fact label="Process tree">{machine.enforcement.process_tree}</Fact>
          <Fact label="Process count">{machine.enforcement.process_count}</Fact>
          <Fact label="Disk">{machine.enforcement.disk}</Fact>
          <Fact label="Self-tested">{machine.enforcement.verified ? 'yes' : 'no'}</Fact>
        </Facts>
        {machine.enforcement.notes.length > 0 ? (
          <ul className="plain-list">
            {machine.enforcement.notes.map((note) => (
              <li key={note}>{note}</li>
            ))}
          </ul>
        ) : null}
      </details>
    </Card>
  );
}

function RunningCard({ machine }: { machine: MachineView }) {
  return (
    <Card title="Running here" description="Jobs holding a reservation on this machine." flush>
      {machine.attempts.length === 0 ? (
        <Empty title="Nothing running" icon="activity">
          No job holds a reservation on this machine.
        </Empty>
      ) : (
        <div className="table-wrap">
          <table className="table table-stack">
            <thead>
              <tr>
                <th scope="col">Status</th>
                <th scope="col">Job</th>
                <th scope="col">Project</th>
                <th scope="col">Holds</th>
              </tr>
            </thead>
            <tbody>
              {machine.attempts.map((attempt) => (
                <tr key={attempt.attempt_id}>
                  <td data-label="Status">
                    <span className="status-cell">
                      <StatusBadge status={phaseStatus(attempt.phase)} live />
                      {attempt.phase === 'quarantined' ? (
                        <Chip tone="bad" title="Cleanup could not be verified; the reservation stays held.">
                          Stuck cleanup
                        </Chip>
                      ) : null}
                      {attempt.uncertain ? (
                        <Chip
                          tone="warn"
                          title="The worker is out of contact. The job may still be running, so its reservation stays held."
                        >
                          Out of contact
                        </Chip>
                      ) : null}
                    </span>
                  </td>
                  <td data-label="Job">
                    <JobLink id={attempt.job_id} recipe={attempt.recipe} />
                  </td>
                  <td data-label="Project">{attempt.project_name}</td>
                  <td data-label="Holds">{formatResources(attempt.reserved)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}

function HealthCard({ machine, now }: { machine: MachineView; now: number }) {
  const telemetry = machine.telemetry ?? null;
  const none = 'unknown (no measurements)';
  return (
    <Card title="Health">
      <Facts>
        <Fact label="Connection">
          {machine.connected ? 'Connected' : 'Not connected'}, {lastSeenText(machine, now)}
        </Fact>
        <Fact label="Power">{powerText(machine)}</Fact>
        <Fact label="Temperature">{telemetry ? telemetry.thermal : none}</Fact>
        <Fact label="Memory pressure">{telemetry ? telemetry.memory_pressure : none}</Fact>
        <Fact label="Swap in use">{telemetry ? formatBytes(telemetry.swap_used_bytes) : none}</Fact>
        <Fact label="Work disk">
          {telemetry
            ? `${formatBytes(telemetry.disk_free_bytes)} free of ${formatBytes(telemetry.disk_total_bytes)}; keeps at least ${formatBytes(diskFloorBytes(machine.policy, telemetry.disk_total_bytes))} free`
            : none}
        </Fact>
        <Fact label="Owner controls">
          {machine.local_control.paused ? 'paused' : 'not paused'},{' '}
          {machine.local_control.draining ? 'draining' : 'not draining'}
          {machine.local_control.boost_until ? `, boosted for ${formatDuration(machine.local_control.boost_until - now)}` : ''}
        </Fact>
        <Fact label="Lifecycle">{lifecycleLabel(machine.lifecycle)}</Fact>
      </Facts>
    </Card>
  );
}

function PolicyCard({ machine }: { machine: MachineView }) {
  const p = machine.policy;
  return (
    <Card title="Policy" description={profileDetail(p.profile)}>
      <Facts>
        <Fact label="Profile">{profileLabel(p.profile)}</Fact>
        <Fact label="Takes builds">{p.accept_builds ? 'yes' : 'no'}</Fact>
        <Fact
          label={
            <>
              Protected for the owner{' '}
              <Help label="What the protected share is">
                A share of the machine that jobs never get, so whoever works on it keeps a responsive computer.
              </Help>
            </>
          }
        >
          {p.foreground_cpu_percent}% of CPU, {p.foreground_memory_percent}% of memory (at least{' '}
          {formatBytes(p.foreground_memory_min_bytes)})
        </Fact>
        <Fact label="Free memory floor">{formatBytes(p.available_memory_floor_bytes)}</Fact>
        <Fact label="Free disk floor">
          {p.disk_free_floor_percent}% (at least {formatBytes(p.disk_free_floor_min_bytes)})
        </Fact>
        <Fact label="Jobs at once">{p.max_concurrent_attempts ?? 'no limit'}</Fact>
        <Fact label="Power">{p.require_ac_power ? 'new jobs only on external power' : 'any'}</Fact>
        {p.min_battery_percent !== null && p.min_battery_percent !== undefined ? (
          <Fact label="Minimum battery">{p.min_battery_percent}%</Fact>
        ) : null}
        <Fact label="Pools">{p.pools.length > 0 ? p.pools.join(', ') : 'none'}</Fact>
        <Fact label="Projects">
          {p.allowed_projects && p.allowed_projects.length > 0 ? `${p.allowed_projects.length} allowed` : 'all'}
        </Fact>
        <Fact label="Revision">{p.revision}</Fact>
      </Facts>
    </Card>
  );
}

function HardwareCard({ machine }: { machine: MachineView }) {
  const { hardware, os, capabilities, executors, worker_version } = machine.inventory;
  return (
    <Card title="Hardware and software">
      <Facts columns>
        <Fact label="Processor">
          {hardware.cpu_model || 'unknown model'}, {hardware.logical_cpus} logical
          {hardware.physical_cpus ? ` / ${hardware.physical_cpus} physical` : ''} CPUs
        </Fact>
        <Fact label="Memory">{formatBytes(hardware.memory_bytes)}</Fact>
        <Fact label="System">
          {osLabel(os.platform.os)} {os.os_version}, {os.platform.arch}
          {os.kernel ? <span className="muted"> (kernel {os.kernel})</span> : null}
        </Fact>
        <Fact label="Host name">{os.hostname || 'not reported'}</Fact>
        <Fact label="Worker">{worker_version}</Fact>
        <Fact label="Machine id">
          <Ident value={machine.id} copy />
        </Fact>
        <Fact label="Disks">
          {hardware.disks.length === 0
            ? 'none reported'
            : hardware.disks.map((d) => `${d.mount} ${formatBytes(d.total_bytes)}${d.filesystem ? ` (${d.filesystem})` : ''}`).join('; ')}
        </Fact>
        <Fact label="GPUs">{hardware.gpus.length > 0 ? hardware.gpus.join(', ') : 'none'}</Fact>
        <Fact label="Isolation">
          {executors.length > 0 ? executors.map((e) => `${e.isolation} (${e.mechanism})`).join(', ') : 'none reported'}
        </Fact>
      </Facts>
      {capabilities.length > 0 ? (
        <>
          <h3 className="subhead">Tools and SDKs</h3>
          <ul className="chips chips-wrap" aria-label="Tools and SDKs">
            {capabilities.map((capability) => (
              <li key={`${capability.name}/${capability.version ?? ''}`}>
                <Chip tone={capability.state === 'installed' ? 'neutral' : 'warn'} title={capability.state}>
                  {capability.name}
                  {capability.version ? ` ${capability.version}` : ''}
                  {capability.state === 'installed' ? '' : ` (${capability.state})`}
                </Chip>
              </li>
            ))}
          </ul>
        </>
      ) : null}
    </Card>
  );
}
