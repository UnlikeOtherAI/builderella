import { useEffect } from 'react';
import { Link, useParams } from 'react-router';
import { useFleet, useNow } from '../api/hooks';
import type { FleetOverview, MachineView } from '../api/schemas';
import { CapacityBar } from '../components/CapacityBar';
import { Chip, Empty, Ident, Loadable } from '../components/ui';
import { machineCapacity } from '../lib/capacity';
import { formatBytes, formatCpu, formatDuration, formatResources, isUnlimited } from '../lib/format';
import {
  flagLabel,
  lastSeenText,
  lifecycleLabel,
  notAdmittingReasons,
  osLabel,
  phaseLabel,
  powerText,
  profileLabel,
} from '../lib/labels';
import { diskFloorBytes } from '../lib/policy';

/** Telemetry older than this is no longer fresh (`TELEMETRY_STALE_AFTER` in bella-core). */
const STALE_AFTER_MS = 15_000;

export function FleetView() {
  const fleet = useFleet();
  return (
    <>
      <h1>Fleet</h1>
      <Loadable query={fleet} what="the fleet">
        {(data) => <FleetBody fleet={data} />}
      </Loadable>
    </>
  );
}

function FleetBody({ fleet }: { fleet: FleetOverview }) {
  const now = useNow(5000);
  const { machine: selected } = useParams();
  // A link to one machine brings its card into view and gives it focus.
  useEffect(() => {
    if (!selected) return;
    const card = document.getElementById(selected);
    card?.scrollIntoView?.({ block: 'start' });
    card?.focus({ preventScroll: true });
  }, [selected]);
  return (
    <>
      <p className="summary">
        {fleet.machines.length} {fleet.machines.length === 1 ? 'machine' : 'machines'}, {fleet.queued}{' '}
        queued, {fleet.running} running, {fleet.environments}{' '}
        {fleet.environments === 1 ? 'environment' : 'environments'}. Controller {fleet.controller_version},
        epoch {fleet.controller_epoch}.
      </p>
      {fleet.reconciling ? (
        <p className="note note-warn" role="status">
          The controller is reconciling with its workers after a restart and is not starting new work
          until that is done.
        </p>
      ) : null}
      {fleet.machines.length === 0 ? (
        <Empty title="No machines are enrolled">
          An administrator can add one under Administration, then run the enroll command on the machine.
        </Empty>
      ) : (
        <div className="machines">
          {fleet.machines.map((machine) => (
            <MachineCard
              key={machine.id}
              machine={machine}
              now={now}
              selected={machine.id === selected}
            />
          ))}
        </div>
      )}
    </>
  );
}

function MachineCard({
  machine,
  now,
  selected,
}: {
  machine: MachineView;
  now: number;
  selected: boolean;
}) {
  const { hardware, os } = machine.inventory;
  const telemetry = machine.telemetry ?? null;
  const capacity = machineCapacity(machine);
  const reasons = notAdmittingReasons(machine);
  const levels = [machine.enforcement.cpu, machine.enforcement.memory];
  // Anything that is neither a kernel limit nor a bounded guest is treated
  // as policy only, including a level this dashboard does not know.
  const policyOnly = levels.some((level) => level !== 'hard' && level !== 'bounded_guest');
  const guest = !policyOnly && levels.includes('bounded_guest');
  const budget = machine.effective_budget ?? null;
  const sampleAge = telemetry ? Math.max(now - telemetry.sampled_at, 0) : null;

  return (
    <article
      className={selected ? 'card machine machine-selected' : 'card machine'}
      id={machine.id}
      tabIndex={-1}
      aria-labelledby={`${machine.id}-name`}
    >
      <header className="machine-head">
        <h2 id={`${machine.id}-name`}>{machine.name}</h2>
        <p className="muted">
          {osLabel(os.platform.os)} {os.os_version}, {os.platform.arch}. {hardware.logical_cpus} logical
          CPUs, {formatBytes(hardware.memory_bytes)} memory.
        </p>
        <p className="chips">
          <Chip tone="neutral">{profileLabel(machine.policy.profile)}</Chip>
          <Chip tone={machine.connected ? 'ok' : 'bad'}>
            {machine.connected ? 'Connected' : 'Disconnected'}
          </Chip>
          {machine.lifecycle !== 'ready' ? (
            <Chip tone="warn">{lifecycleLabel(machine.lifecycle)}</Chip>
          ) : null}
          {machine.flags.map((flag) => (
            <Chip key={flag} tone="warn">
              {flagLabel(flag)}
            </Chip>
          ))}
        </p>
        <p className="muted">
          {machine.connected ? 'Connected to the controller' : 'Not connected to the controller'};{' '}
          {lastSeenText(machine, now)}.
        </p>
      </header>

      <section className="admission" aria-label="Admission">
        {reasons.length === 0 ? (
          <p>
            <strong>Accepting new work.</strong>
          </p>
        ) : (
          <>
            <p>
              <strong>
                {machine.connected ? 'Reachable, but not accepting new work' : 'Not accepting new work'}
              </strong>{' '}
              because:
            </p>
            <ul>
              {reasons.map((reason) => (
                <li key={reason}>{reason}</li>
              ))}
            </ul>
          </>
        )}
      </section>

      {policyOnly ? (
        <p className="note note-warn">
          Limits on this machine are not kernel guarantees. Managed work is kept within its share by
          policy and can exceed it before the worker reacts.
        </p>
      ) : null}
      {guest ? (
        <p className="note note-warn">
          The worker runs inside a guest. Its limits hold inside that guest; what the host itself
          uses is outside them.
        </p>
      ) : null}
      {sampleAge !== null && sampleAge > STALE_AFTER_MS ? (
        <p className="note note-warn">
          The bars show the last measurements received, taken {formatDuration(sampleAge)} ago. Use on
          the machine may have changed since.
        </p>
      ) : null}

      <CapacityBar
        label="CPU"
        capacity={capacity.cpu}
        format={formatCpu}
        enforcement={machine.enforcement.cpu}
        verified={machine.enforcement.verified}
      />
      <CapacityBar
        label="Memory"
        capacity={capacity.memory}
        format={formatBytes}
        enforcement={machine.enforcement.memory}
        verified={machine.enforcement.verified}
      />

      <dl className="facts">
        <div className="fact">
          <dt>Power</dt>
          <dd>{powerText(machine)}</dd>
        </div>
        <div className="fact">
          <dt>Thermal state</dt>
          <dd>{telemetry ? telemetry.thermal : 'unknown (no telemetry)'}</dd>
        </div>
        <div className="fact">
          <dt>Memory pressure</dt>
          <dd>{telemetry ? telemetry.memory_pressure : 'unknown (no telemetry)'}</dd>
        </div>
        <div className="fact">
          <dt>Work volume</dt>
          <dd>
            {telemetry
              ? `${formatBytes(telemetry.disk_free_bytes)} free of ${formatBytes(telemetry.disk_total_bytes)}; floor ${formatBytes(diskFloorBytes(machine.policy, telemetry.disk_total_bytes))}`
              : 'unknown (no telemetry)'}
          </dd>
        </div>
        <div className="fact">
          <dt>Managed ceiling</dt>
          <dd>
            {formatCpu(machine.managed_ceiling.cpu_milli)}, {formatBytes(machine.managed_ceiling.memory_bytes)}{' '}
            memory
            {isUnlimited(machine.managed_ceiling.disk_bytes) ? '' : `, ${formatBytes(machine.managed_ceiling.disk_bytes)} disk`}
          </dd>
        </div>
        <div className="fact">
          <dt>Granted right now</dt>
          <dd>
            {budget
              ? `${formatCpu(budget.cpu_milli)}, ${formatBytes(budget.memory_bytes)} memory`
              : 'not reported'}
          </dd>
        </div>
        <div className="fact">
          <dt>Reserved</dt>
          <dd>{formatResources(machine.reserved)}</dd>
        </div>
        <div className="fact">
          <dt>Measurements</dt>
          <dd>
            {telemetry && sampleAge !== null
              ? `taken ${formatDuration(sampleAge)} ago, quality ${telemetry.quality}`
              : 'none received'}
          </dd>
        </div>
      </dl>

      <section aria-label="Active attempts">
        <h3>Active attempts</h3>
        {machine.attempts.length === 0 ? (
          <p className="muted">None. Nothing managed holds a reservation here.</p>
        ) : (
          <ul className="attempts">
            {machine.attempts.map((attempt) => (
              <li key={attempt.attempt_id}>
                <Link to={`/jobs/${attempt.job_id}`}>
                  {attempt.recipe} <Ident value={attempt.job_id} />
                </Link>{' '}
                <span className="muted">
                  {attempt.project_name}, {phaseLabel(attempt.phase).toLowerCase()}, holds{' '}
                  {formatResources(attempt.reserved)}
                </span>
                {attempt.uncertain ? (
                  <>
                    {' '}
                    <Chip tone="warn" title="The worker is out of contact. The attempt may still be running, so its reservation stays held.">
                      uncertain
                    </Chip>
                  </>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      {machine.enforcement.notes.length > 0 ? (
        <details>
          <summary>Enforcement notes from the worker's self-test</summary>
          <ul>
            {machine.enforcement.notes.map((note) => (
              <li key={note}>{note}</li>
            ))}
          </ul>
          <p className="muted">
            Process tree: {machine.enforcement.process_tree}. Process count:{' '}
            {machine.enforcement.process_count}. Disk: {machine.enforcement.disk}.
          </p>
        </details>
      ) : null}
    </article>
  );
}
