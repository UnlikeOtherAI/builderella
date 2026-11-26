import { useState } from 'react';
import { Link, useParams } from 'react-router';
import { useEnvironment, useEnvironmentAction, useEnvironments, useNow, useWhoAmI } from '../api/hooks';
import type { EnvironmentView } from '../api/schemas';
import { Chip, ConfirmDialog, CopyButton, Empty, ErrorPanel, Fact, Facts, Ident, Loadable } from '../components/ui';
import { formatDuration, formatResources, formatTimestamp, shortId } from '../lib/format';
import { environmentStateLabel, environmentStateTone, leaseLimitLabel } from '../lib/labels';
import { deadlineAfterRenewal, formatCountdown, leaseStatus, type LeaseStatus } from '../lib/lease';

/** States in which the lease is still counting down. */
const LIVE_STATES = new Set(['starting', 'ready']);

export function EnvironmentsView() {
  const { id } = useParams();
  return id ? <EnvironmentDetail key={id} id={id} /> : <EnvironmentList />;
}

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

function Countdown({ environment, now }: { environment: EnvironmentView; now: number }) {
  if (!LIVE_STATES.has(environment.state)) {
    return <span className="muted">not counting: the environment is {environment.state}</span>;
  }
  const status = leaseStatus(environment.lease, now, environment.effective_deadline);
  if (status.ended) {
    return (
      <span>
        <strong>Deadline passed</strong> {formatDuration(now - status.deadline)} ago (
        {leaseLimitLabel(status.limit)}). Waiting for the controller to end it.
      </span>
    );
  }
  return (
    <span>
      <strong className="countdown">{formatCountdown(status.remainingMs)}</strong> left, ended by{' '}
      {leaseLimitLabel(status.limit)}
    </span>
  );
}

// -------------------------------------------------------------------- list

function EnvironmentList() {
  const environments = useEnvironments();
  const now = useNow(1000);
  return (
    <>
      <h1>Environments</h1>
      <p className="muted">
        An environment keeps a job's services running after its steps finish. It holds its machine
        reservation until it ends and cleanup is confirmed.
      </p>
      <Loadable query={environments} what="environments">
        {(list) =>
          list.length === 0 ? (
            <Empty title="No environments">
              A job whose recipe declares an environment creates one when its services are ready.
            </Empty>
          ) : (
            <div className="table-wrap">
              <table className="stack">
                <thead>
                  <tr>
                    <th scope="col">Environment</th>
                    <th scope="col">State</th>
                    <th scope="col">Machine</th>
                    <th scope="col">Holds</th>
                    <th scope="col">Time left</th>
                  </tr>
                </thead>
                <tbody>
                  {list.map((environment) => (
                    <tr key={environment.id}>
                      <td data-label="Environment">
                        <Link to={`/environments/${environment.id}`}>
                          <Ident value={environment.id} />
                        </Link>
                      </td>
                      <td data-label="State">
                        <Chip tone={environmentStateTone(environment.state)}>
                          {environmentStateLabel(environment.state)}
                        </Chip>
                      </td>
                      <td data-label="Machine">{environment.machine_name}</td>
                      <td data-label="Holds">{formatResources(environment.reserved)}</td>
                      <td data-label="Time left">
                        <Countdown environment={environment} now={now} />
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

// ------------------------------------------------------------------ detail

function EnvironmentDetail({ id }: { id: string }) {
  const environment = useEnvironment(id);
  return (
    <>
      <p className="crumb">
        <Link to="/environments">Environments</Link> / {shortId(id)}
      </p>
      <Loadable query={environment} what="the environment">
        {(data) => <EnvironmentBody environment={data} />}
      </Loadable>
    </>
  );
}

function EnvironmentBody({ environment }: { environment: EnvironmentView }) {
  const now = useNow(1000);
  const who = useWhoAmI();
  const renew = useEnvironmentAction(environment.id, 'renew');
  const finish = useEnvironmentAction(environment.id, 'finish');
  const [askFinish, setAskFinish] = useState(false);

  const live = LIVE_STATES.has(environment.state);
  const status = leaseStatus(environment.lease, now, environment.effective_deadline);
  const renewal = live ? deadlineAfterRenewal(environment.lease, now) : null;
  const gain = renewal ? renewal.deadline - status.deadline : 0;
  const viewer = who.data?.role === 'viewer';

  const renewedTo = renew.data
    ? leaseStatus(renew.data.lease, now, renew.data.effective_deadline)
    : null;

  return (
    <>
      <h1>
        Environment <Ident value={environment.id} />
      </h1>
      <p>
        <CopyButton value={environment.id} label="Copy environment id" />
      </p>
      <p className="chips">
        <Chip tone={environmentStateTone(environment.state)}>
          {environmentStateLabel(environment.state)}
        </Chip>
      </p>
      {environment.ended_reason ? <p className="note">{environment.ended_reason}</p> : null}
      {environment.state === 'quarantined' ? (
        <p className="note note-bad">
          Cleanup could not be verified, so this environment's reservation is still counted against{' '}
          {environment.machine_name}.
        </p>
      ) : null}

      <section aria-labelledby="lease-title" className="card">
        <h2 id="lease-title">Lease</h2>
        <p className="lease-countdown" role="timer" aria-live="off">
          <Countdown environment={environment} now={now} />
        </p>
        {live ? (
          <p>
            It ends at {formatTimestamp(status.deadline)} because of {limitSentence(status)}.
          </p>
        ) : null}
        <Facts>
          <Fact label="Idle timeout">
            {formatDuration(environment.lease.spec.idle_timeout_secs * 1000)} without use; would end it at{' '}
            {formatTimestamp(status.idleDeadline)}
          </Fact>
          <Fact label="Lease">
            {formatDuration(environment.lease.spec.lease_secs * 1000)} per renewal; runs until{' '}
            {formatTimestamp(status.leaseDeadline)}
          </Fact>
          <Fact label="Maximum lifetime">
            {formatDuration(environment.lease.spec.max_lifetime_secs * 1000)} in total; ends at{' '}
            {formatTimestamp(status.hardDeadline)} whatever happens
          </Fact>
          <Fact label="Created">{formatTimestamp(environment.lease.created_at)}</Fact>
          <Fact label="Last used or renewed">{formatTimestamp(environment.lease.last_activity_at)}</Fact>
        </Facts>

        {live ? (
          <>
            <p className="muted">
              {renewal === null
                ? 'The deadline has passed, so the controller will refuse a renewal.'
                : gain >= 60_000
                  ? `Renewing now would move the end to ${formatTimestamp(renewal.deadline)} (${formatDuration(gain)} later), limited by the ${leaseLimitLabel(renewal.limit)}.`
                  : renewal.limit === 'maximum'
                    ? 'Renewing now would not move the end: it is already set by the maximum lifetime.'
                    : `Renewing now would move the end by less than a minute: it is set by the ${leaseLimitLabel(renewal.limit)}.`}
            </p>
            {viewer ? (
              <p className="muted">You are signed in as a viewer. Renewing or finishing needs the developer role.</p>
            ) : (
              <div className="actions actions-start">
                <button
                  type="button"
                  className="btn btn-primary"
                  onClick={() => renew.mutate()}
                  disabled={renew.isPending}
                >
                  {renew.isPending ? 'Renewing…' : 'Renew lease'}
                </button>
                <button
                  type="button"
                  className="btn btn-danger"
                  onClick={() => setAskFinish(true)}
                  disabled={finish.isPending}
                >
                  Finish environment…
                </button>
              </div>
            )}
          </>
        ) : (
          <p className="muted">
            This environment is {environment.state}; it can no longer be renewed or finished.
          </p>
        )}

        <div aria-live="polite">
          {live && renew.isSuccess && renewedTo ? (
            <p className="note note-ok">
              Renewed. The controller now ends it at {formatTimestamp(renewedTo.deadline)}, by the{' '}
              {leaseLimitLabel(renewedTo.limit)}.
            </p>
          ) : null}
          {finish.isSuccess ? (
            <p className="note note-ok">
              Finish accepted. The controller reports the environment as{' '}
              {environmentStateLabel(finish.data.state).toLowerCase()}. Its resources are freed when
              cleanup is confirmed.
            </p>
          ) : null}
        </div>
        {renew.error ? <ErrorPanel error={renew.error} /> : null}
        {finish.error ? <ErrorPanel error={finish.error} /> : null}
      </section>

      <section aria-labelledby="where-title">
        <h2 id="where-title">Where and what</h2>
        <Facts>
          <Fact label="Machine">{environment.machine_name}</Fact>
          <Fact label="Job">
            <Link to={`/jobs/${environment.job_id}`}>
              <Ident value={environment.job_id} />
            </Link>
          </Fact>
          <Fact label="Snapshot">
            <Ident value={environment.snapshot_id} copy />
            <span className="muted"> Later edits to the working tree never change what it serves.</span>
          </Fact>
          <Fact label="Holds">{formatResources(environment.reserved)}</Fact>
        </Facts>
      </section>

      <section aria-labelledby="endpoints-title">
        <h2 id="endpoints-title">Endpoints</h2>
        {environment.endpoints.length === 0 ? (
          <Empty title="No endpoints">
            {environment.state === 'starting'
              ? 'Services are still starting; endpoints appear when they are ready.'
              : 'No service of this environment publishes a port.'}
          </Empty>
        ) : (
          <ul className="endpoints">
            {environment.endpoints.map((endpoint) => {
              const address = `${endpoint.host}:${endpoint.port}`;
              return (
                <li key={`${endpoint.step}/${address}`}>
                  <span className="tag">{endpoint.step}</span> <code className="wrap">{address}</code>{' '}
                  <CopyButton value={address} label="Copy address" />
                </li>
              );
            })}
          </ul>
        )}
      </section>

      <ConfirmDialog
        open={askFinish}
        title="Finish this environment?"
        confirmLabel="Finish environment"
        cancelLabel="Keep it"
        danger
        busy={finish.isPending}
        onCancel={() => setAskFinish(false)}
        onConfirm={() => finish.mutate(undefined, { onSettled: () => setAskFinish(false) })}
      >
        <p>Finishing stops the environment's services and runs cleanup. It cannot be restarted.</p>
        <p>The machine's reservation is released only after that cleanup is confirmed.</p>
      </ConfirmDialog>
    </>
  );
}
