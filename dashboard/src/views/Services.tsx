// Services: a job's processes kept running after its steps finish (an
// "environment" in the API), reachable at their endpoints until their lease
// runs out or someone finishes them.

import { useState } from 'react';
import { Link, useParams } from 'react-router';
import { useEnvironment, useEnvironmentAction, useEnvironments, useJobs, useNow, useWhoAmI } from '../api/hooks';
import type { EnvironmentView } from '../api/schemas';
import { JobLink, MachineLink } from '../components/bits';
import { Icon } from '../components/icons';
import {
  Callout,
  Card,
  ConfirmDialog,
  CopyButton,
  Empty,
  ErrorPanel,
  Fact,
  Facts,
  Help,
  Ident,
  Loadable,
  PageHeader,
  StatusBadge,
  Tabs,
} from '../components/ui';
import { formatDuration, formatResources, formatTimestamp, shortId } from '../lib/format';
import { environmentStateLabel, environmentStateTone, leaseLimitLabel } from '../lib/labels';
import { deadlineAfterRenewal, formatCountdown, leaseStatus, type LeaseStatus } from '../lib/lease';
import type { Status } from '../lib/status';

/** States in which the lease is still counting down. */
const LIVE_STATES = new Set(['starting', 'ready']);
/** Less than this left is worth a warning colour. */
const SOON_MS = 10 * 60_000;

export function serviceStatus(environment: EnvironmentView): Status {
  return {
    key: environment.state,
    label: environmentStateLabel(environment.state),
    tone: environmentStateTone(environment.state),
    precise: environment.ended_reason ?? environmentStateLabel(environment.state),
  };
}

const canAct = (role: string | undefined) => role !== undefined && role !== 'viewer';

function limitSentence(status: LeaseStatus): string {
  switch (status.limit) {
    case 'idle':
      return 'the idle timeout: nobody has used or renewed it recently';
    case 'lease':
      return 'the lease, which a renewal extends';
    case 'maximum':
      return 'the maximum lifetime, which no renewal can extend';
  }
}

/** How long a live service has left, as a countdown and a draining bar. */
export function TimeLeft({ environment, now }: { environment: EnvironmentView; now: number }) {
  if (!LIVE_STATES.has(environment.state)) {
    return <span className="muted">not counting ({environmentStateLabel(environment.state).toLowerCase()})</span>;
  }
  const status = leaseStatus(environment.lease, now, environment.effective_deadline);
  if (status.ended) {
    return (
      <span className="text-warn">
        Deadline passed {formatDuration(now - status.deadline)} ago; ending
      </span>
    );
  }
  const span = Math.max(status.deadline - environment.lease.last_activity_at, 1);
  const fraction = Math.min(Math.max(status.remainingMs / span, 0), 1);
  const soon = status.remainingMs < SOON_MS;
  return (
    <span className="time-left" title={`Ends at ${formatTimestamp(status.deadline)}, by the ${leaseLimitLabel(status.limit)}`}>
      <span className={soon ? 'bar bar-thin time-bar time-soon' : 'bar bar-thin time-bar'} aria-hidden="true">
        <span className="bar-part bar-use" style={{ flexGrow: fraction }} />
        <span className="bar-part bar-free" style={{ flexGrow: 1 - fraction }} />
      </span>
      <span className="nowrap">
        <strong className="countdown">{formatCountdown(status.remainingMs)}</strong> <span className="muted">left</span>
      </span>
      <span className="cell-sub">ends by its {leaseLimitLabel(status.limit)}</span>
    </span>
  );
}

// ------------------------------------------------------------------ list

export function ServicesView() {
  const environments = useEnvironments();
  const active = useJobs({ active: true, limit: 1000 });
  const now = useNow(1000);
  const who = useWhoAmI();
  const [tab, setTab] = useState('live');
  const recipes = new Map((active.data ?? []).map((job) => [job.id, job.recipe]));
  return (
    <>
      <PageHeader
        title="Services"
        description={
          <>
            Jobs can keep their services running after their steps finish, for example a preview server. Each one
            holds its machine's resources until its time runs out or someone finishes it{' '}
            <Help label="Where services come from">
              The API calls these environments. A recipe that declares an environment keeps its service steps
              running once they are ready.
            </Help>
          </>
        }
      />
      <Loadable query={environments} what="services" lines={5}>
        {(list) => {
          const live = list.filter((e) => e.state !== 'ended');
          const ended = list.filter((e) => e.state === 'ended');
          const rows = tab === 'live' ? live : ended;
          return (
            <Card flush>
              <div className="toolbar">
                <Tabs
                  label="Show services"
                  idBase="services"
                  value={tab}
                  onChange={setTab}
                  tabs={[
                    { id: 'live', label: 'Running', count: live.length },
                    { id: 'ended', label: 'Ended', count: ended.length },
                  ]}
                />
              </div>
              <div role="tabpanel" id={`services-panel-${tab}`} aria-labelledby={`services-tab-${tab}`}>
                {rows.length === 0 ? (
                  <Empty title={tab === 'live' ? 'No services are running' : 'No ended services'} icon="services">
                    {tab === 'live'
                      ? 'A job whose recipe declares an environment starts one when its services are ready.'
                      : 'Services that have ended are listed here until the controller forgets them.'}
                  </Empty>
                ) : (
                  <div className="table-wrap">
                    <table className="table table-stack">
                      <thead>
                        <tr>
                          <th scope="col">Status</th>
                          <th scope="col">Service</th>
                          <th scope="col">Machine</th>
                          <th scope="col">Endpoints</th>
                          <th scope="col">Time left</th>
                          <th scope="col">
                            <span className="sr-only">Actions</span>
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {rows.map((environment) => (
                          <tr key={environment.id}>
                            <td data-label="Status">
                              <StatusBadge status={serviceStatus(environment)} live={environment.state === 'ready'} />
                            </td>
                            <td data-label="Service">
                              <span className="cell-main">
                                <Link to={`/services/${environment.id}`} className="nowrap">
                                  {recipes.get(environment.job_id) ?? 'Service'} <Ident value={environment.id} />
                                </Link>
                                <span className="cell-sub">
                                  from job <Link to={`/jobs/${environment.job_id}`}>{shortId(environment.job_id)}</Link>
                                </span>
                              </span>
                            </td>
                            <td data-label="Machine" className="nowrap">
                              <MachineLink id={environment.machine_id} name={environment.machine_name} />
                            </td>
                            <td data-label="Endpoints">
                              <Endpoints environment={environment} />
                            </td>
                            <td data-label="Time left">
                              <TimeLeft environment={environment} now={now} />
                            </td>
                            <td data-label="Actions" className="cell-actions">
                              {canAct(who.data?.role) && LIVE_STATES.has(environment.state) ? (
                                <RowActions environment={environment} />
                              ) : null}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </div>
            </Card>
          );
        }}
      </Loadable>
    </>
  );
}

function Endpoints({ environment }: { environment: EnvironmentView }) {
  if (environment.endpoints.length === 0) {
    return <span className="muted">{environment.state === 'starting' ? 'not ready yet' : 'none'}</span>;
  }
  return (
    <ul className="endpoints">
      {environment.endpoints.map((endpoint) => {
        const address = `${endpoint.host}:${endpoint.port}`;
        return (
          <li key={`${endpoint.step}/${address}`}>
            <span className="tag">{endpoint.step}</span>
            <code title={address}>{address}</code>
            <CopyButton value={address} label={`Copy ${address}`} />
          </li>
        );
      })}
    </ul>
  );
}

function RowActions({ environment }: { environment: EnvironmentView }) {
  const renew = useEnvironmentAction(environment.id, 'renew');
  const finish = useEnvironmentAction(environment.id, 'finish');
  const [ask, setAsk] = useState(false);
  return (
    <span className="row-actions">
      <button type="button" className="btn btn-small" disabled={renew.isPending} onClick={() => renew.mutate()}>
        {renew.isPending ? 'Renewing…' : renew.isSuccess ? 'Renewed' : 'Renew'}
      </button>
      <button type="button" className="btn btn-small btn-danger-quiet" onClick={() => setAsk(true)}>
        Finish
      </button>
      {renew.error ? <ErrorPanel error={renew.error} /> : null}
      {finish.error ? <ErrorPanel error={finish.error} /> : null}
      <FinishDialog open={ask} busy={finish.isPending} onCancel={() => setAsk(false)} onConfirm={() => finish.mutate(undefined, { onSettled: () => setAsk(false) })} />
    </span>
  );
}

function FinishDialog({
  open,
  busy,
  onCancel,
  onConfirm,
}: {
  open: boolean;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <ConfirmDialog
      open={open}
      title="Finish this service?"
      confirmLabel="Finish service"
      cancelLabel="Keep it"
      danger
      busy={busy}
      onCancel={onCancel}
      onConfirm={onConfirm}
    >
      <p>Finishing stops the service's processes and runs cleanup. It cannot be restarted.</p>
      <p>The machine's reservation is released only after that cleanup is confirmed.</p>
    </ConfirmDialog>
  );
}

// ---------------------------------------------------------------- detail

export function ServiceDetailView() {
  const { id = '' } = useParams();
  const environment = useEnvironment(id);
  return (
    <>
      <nav className="crumb" aria-label="Breadcrumb">
        <Link to="/services">Services</Link>
        <Icon name="chevronRight" size={14} />
      </nav>
      <Loadable query={environment} what="the service" lines={6}>
        {(data) => <ServiceBody key={data.id} environment={data} />}
      </Loadable>
    </>
  );
}

function ServiceBody({ environment }: { environment: EnvironmentView }) {
  const now = useNow(1000);
  const who = useWhoAmI();
  const renew = useEnvironmentAction(environment.id, 'renew');
  const finish = useEnvironmentAction(environment.id, 'finish');
  const [askFinish, setAskFinish] = useState(false);

  const live = LIVE_STATES.has(environment.state);
  const status = leaseStatus(environment.lease, now, environment.effective_deadline);
  const renewal = live ? deadlineAfterRenewal(environment.lease, now) : null;
  const gain = renewal ? renewal.deadline - status.deadline : 0;
  const allowed = canAct(who.data?.role);
  const renewedTo = renew.data ? leaseStatus(renew.data.lease, now, renew.data.effective_deadline) : null;

  return (
    <>
      <PageHeader
        title={
          <>
            Service <Ident value={environment.id} copy />
          </>
        }
        actions={
          live && allowed ? (
            <>
              <button type="button" className="btn btn-primary" onClick={() => renew.mutate()} disabled={renew.isPending}>
                <Icon name="refresh" size={16} /> {renew.isPending ? 'Renewing…' : 'Renew'}
              </button>
              <button type="button" className="btn btn-danger" onClick={() => setAskFinish(true)} disabled={finish.isPending}>
                <Icon name="stop" size={16} /> Finish
              </button>
            </>
          ) : null
        }
      >
        <StatusBadge status={serviceStatus(environment)} live={environment.state === 'ready'} />
        <span className="meta-sep" aria-hidden="true" />
        <span>
          on <MachineLink id={environment.machine_id} name={environment.machine_name} />
        </span>
        <span className="meta-sep" aria-hidden="true" />
        <span>
          from <JobLink id={environment.job_id} />
        </span>
      </PageHeader>

      {environment.ended_reason ? <Callout tone="info">{environment.ended_reason}</Callout> : null}
      {environment.state === 'quarantined' ? (
        <Callout tone="bad" title="Cleanup could not be verified">
          This service's reservation is still counted against {environment.machine_name} until someone resolves it.
        </Callout>
      ) : null}
      <div aria-live="polite">
        {live && renew.isSuccess && renewedTo ? (
          <Callout tone="ok">
            Renewed. It now ends at {formatTimestamp(renewedTo.deadline)}, by the {leaseLimitLabel(renewedTo.limit)}.
          </Callout>
        ) : null}
        {finish.isSuccess ? (
          <Callout tone="ok">
            Finish accepted; the service is {environmentStateLabel(finish.data.state).toLowerCase()}. Its resources
            are freed when cleanup is confirmed.
          </Callout>
        ) : null}
      </div>
      {renew.error ? <ErrorPanel error={renew.error} /> : null}
      {finish.error ? <ErrorPanel error={finish.error} /> : null}
      {live && !allowed && who.data ? (
        <p className="muted">You are signed in as a viewer. Renewing or finishing a service needs the developer role.</p>
      ) : null}

      <div className="grid-2">
        <Card title="Time left">
          <p className="lease-countdown" role="timer" aria-live="off">
            <TimeLeft environment={environment} now={now} />
          </p>
          {live ? (
            <p>
              It ends at {formatTimestamp(status.deadline)} because of {limitSentence(status)}.{' '}
              {renewal === null
                ? 'The deadline has passed, so a renewal will be refused.'
                : gain >= 60_000
                  ? `Renewing now moves the end ${formatDuration(gain)} later, to ${formatTimestamp(renewal.deadline)}.`
                  : renewal.limit === 'maximum'
                    ? 'Renewing now would not move the end: it is set by the maximum lifetime.'
                    : `Renewing now would move the end by less than a minute (${leaseLimitLabel(renewal.limit)}).`}
            </p>
          ) : (
            <p className="muted">This service is {environment.state}; it can no longer be renewed or finished.</p>
          )}
          <Facts>
            <Fact label="Ends when idle for">
              {formatDuration(environment.lease.spec.idle_timeout_secs * 1000)}{' '}
              <span className="muted">(would end {formatTimestamp(status.idleDeadline)})</span>
            </Fact>
            <Fact label="Each renewal gives">
              {formatDuration(environment.lease.spec.lease_secs * 1000)}{' '}
              <span className="muted">(runs until {formatTimestamp(status.leaseDeadline)})</span>
            </Fact>
            <Fact label="Never longer than">
              {formatDuration(environment.lease.spec.max_lifetime_secs * 1000)}{' '}
              <span className="muted">(ends {formatTimestamp(status.hardDeadline)} whatever happens)</span>
            </Fact>
            <Fact label="Started">{formatTimestamp(environment.lease.created_at)}</Fact>
            <Fact label="Last used or renewed">{formatTimestamp(environment.lease.last_activity_at)}</Fact>
          </Facts>
        </Card>
        <div className="stack-gap">
          <Card title="Endpoints">
            {environment.endpoints.length === 0 ? (
              <Empty title="No endpoints" icon="link">
                {environment.state === 'starting'
                  ? 'Its services are still starting; endpoints appear when they are ready.'
                  : 'None of its services publishes a port.'}
              </Empty>
            ) : (
              <Endpoints environment={environment} />
            )}
          </Card>
          <Card title="Where and what">
            <Facts>
              <Fact label="Machine">
                <MachineLink id={environment.machine_id} name={environment.machine_name} />
              </Fact>
              <Fact label="Job">
                <JobLink id={environment.job_id} />
              </Fact>
              <Fact label="Snapshot">
                <Ident value={environment.snapshot_id} copy />
                <span className="muted"> Later edits never change what it serves.</span>
              </Fact>
              <Fact label="Holds">{formatResources(environment.reserved)}</Fact>
            </Facts>
          </Card>
        </div>
      </div>

      <FinishDialog
        open={askFinish}
        busy={finish.isPending}
        onCancel={() => setAskFinish(false)}
        onConfirm={() => finish.mutate(undefined, { onSettled: () => setAskFinish(false) })}
      />
    </>
  );
}
