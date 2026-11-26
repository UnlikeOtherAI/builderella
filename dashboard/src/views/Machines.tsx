// Every machine in one table, so they can be compared at a glance: status,
// platform, CPU and memory as simple bars, jobs and conditions. The full
// detail of a machine is one click away.

import { Link } from 'react-router';
import { useFleet } from '../api/hooks';
import type { FleetOverview } from '../api/schemas';
import { MachineLink, Platform } from '../components/bits';
import { Icon } from '../components/icons';
import { LoadBar, LoadLegend } from '../components/LoadBar';
import { Callout, Card, Empty, FlagSummary, Help, Loadable, PageHeader, StatusBadge } from '../components/ui';
import { machineCapacity } from '../lib/capacity';
import { formatBytes, formatCpu } from '../lib/format';
import { profileLabel } from '../lib/labels';
import { MACHINE_WORDS, isOnline, machineFlags, machineStatus } from '../lib/status';

export function MachinesView() {
  const fleet = useFleet();
  return (
    <>
      <PageHeader
        title="Machines"
        description="The computers that run jobs. Each one keeps a share for its owner and offers the rest to the fleet."
      />
      <Loadable query={fleet} what="the machines" lines={6}>
        {(data) => <MachinesBody fleet={data} />}
      </Loadable>
    </>
  );
}

function StatusHelp() {
  return (
    <Help label="What the machine statuses mean">
      <span className="help-list">
        {(['busy', 'idle', 'stale', 'offline', 'draining', 'paused', 'retired'] as const).map((key) => (
          <span key={key}>
            <strong>{MACHINE_WORDS[key].label}</strong>: {MACHINE_WORDS[key].meaning}
          </span>
        ))}
      </span>
    </Help>
  );
}

function MachinesBody({ fleet }: { fleet: FleetOverview }) {
  if (fleet.machines.length === 0) {
    return (
      <Card>
        <Empty
          title="No machines yet"
          icon="machines"
          action={
            <Link to="/settings#add-machine" className="btn btn-primary">
              <Icon name="plus" size={16} /> Add your first machine
            </Link>
          }
        >
          Create a pairing code in Settings, then run <code>bella worker enroll</code> on the machine you want to
          add.
        </Empty>
      </Card>
    );
  }
  const online = fleet.machines.filter((m) => isOnline(machineStatus(m))).length;
  return (
    <>
      {fleet.reconciling ? (
        <Callout tone="warn" role="status" title="The controller is reconciling">
          It restarted and is checking in with its machines. No new jobs start until that is done.
        </Callout>
      ) : null}
      <Card
        title={`${online} of ${fleet.machines.length} online`}
        actions={<LoadLegend />}
        flush
      >
        <div className="table-wrap">
          <table className="table table-stack">
            <thead>
              <tr>
                <th scope="col">Machine</th>
                <th scope="col">
                  <span className="th-help">
                    Status <StatusHelp />
                  </span>
                </th>
                <th scope="col">CPU</th>
                <th scope="col">Memory</th>
                <th scope="col">Jobs</th>
                <th scope="col">Conditions</th>
              </tr>
            </thead>
            <tbody>
              {fleet.machines.map((machine) => {
                const status = machineStatus(machine);
                const capacity = machineCapacity(machine);
                const count = machine.attempts.length;
                return (
                  <tr key={machine.id}>
                    <th scope="row" data-label="Machine">
                      <span className="cell-main">
                        <MachineLink id={machine.id} name={machine.name} />
                        <span className="cell-sub">
                          <Platform machine={machine} /> · {profileLabel(machine.policy.profile)}
                        </span>
                      </span>
                    </th>
                    <td data-label="Status">
                      <StatusBadge status={status} />
                    </td>
                    <td data-label="CPU" className="cell-bar">
                      <LoadBar label="CPU" capacity={capacity.cpu} format={formatCpu} />
                    </td>
                    <td data-label="Memory" className="cell-bar">
                      <LoadBar label="Memory" capacity={capacity.memory} format={formatBytes} />
                    </td>
                    <td data-label="Jobs" className="num">
                      {count > 0 ? count : <span className="muted">0</span>}
                    </td>
                    <td data-label="Conditions">
                      <FlagSummary flags={machineFlags(machine, status)} max={2} />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </Card>
    </>
  );
}
