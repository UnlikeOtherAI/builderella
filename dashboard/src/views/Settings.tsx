// Settings: machines and their policies, adding a machine, projects and
// access tokens. Everything here needs the admin role to change; others see
// it read-only.

import { useEffect, useState, type FormEvent, type ReactNode } from 'react';
import { useLocation, useSearchParams } from 'react-router';
import {
  useCreatePairingCode,
  useCreateToken,
  useFleet,
  useNow,
  useProjects,
  usePutProject,
  useUpdateMachine,
  useWhoAmI,
} from '../api/hooks';
import {
  CloudMode,
  MachineProfile,
  Role,
  type MachinePolicy,
  type MachineView,
  type ProjectPolicy,
  type ProjectView,
} from '../api/schemas';
import { Icon } from '../components/icons';
import {
  Callout,
  Card,
  CheckField,
  ConfirmDialog,
  CopyButton,
  Drawer,
  Empty,
  ErrorPanel,
  Field,
  Loadable,
  NumberField,
  PageHeader,
  StatusBadge,
} from '../components/ui';
import { formatBytes, formatCpu, formatResources, formatTimestamp, shellQuote } from '../lib/format';
import { cloudModeLabel, lifecycleLabel, profileDetail, profileLabel, roleLabel } from '../lib/labels';
import { formatCountdown } from '../lib/lease';
import { managedCeiling } from '../lib/policy';
import { sameJson } from '../lib/same';
import { machineStatus } from '../lib/status';

const GIB = 1024 ** 3;
const toGib = (bytes: number) => Math.round((bytes / GIB) * 100) / 100;
const fromGib = (gib: number) => Math.round(gib * GIB);

export function SettingsView() {
  const who = useWhoAmI();
  const admin = who.data?.role === 'admin';
  const { hash } = useLocation();
  // `#/settings#add-machine` brings a section into view.
  useEffect(() => {
    if (!hash) return;
    document.getElementById(hash.slice(1))?.scrollIntoView?.({ block: 'start' });
  }, [hash]);
  return (
    <>
      <PageHeader title="Settings" description="Machines, projects and access to this fleet." />
      {who.data && !admin ? (
        <Callout tone="info" role="status" title="Read-only">
          You are signed in as {roleLabel(who.data.role)}. Changing machines, projects, pairing codes and tokens needs
          the admin role.
        </Callout>
      ) : null}
      {who.error && !who.data ? <ErrorPanel error={who.error} onRetry={() => void who.refetch()} /> : null}
      <MachinesSection admin={admin} />
      <AddMachineSection admin={admin} />
      <ProjectsSection admin={admin} />
      <TokensSection admin={admin} />
    </>
  );
}

function Section({ id, title, intro, children }: { id: string; title: string; intro: ReactNode; children: ReactNode }) {
  return (
    <section className="settings-section" id={id} aria-labelledby={`${id}-title`}>
      <div className="settings-intro">
        <h2 id={`${id}-title`}>{title}</h2>
        <p className="muted">{intro}</p>
      </div>
      <div className="settings-body">{children}</div>
    </section>
  );
}

// ---------------------------------------------------------------- machines

function MachinesSection({ admin }: { admin: boolean }) {
  const fleet = useFleet();
  const [params, setParams] = useSearchParams();
  const editing = params.get('machine');
  const open = (id: string | null) => {
    const next = new URLSearchParams(params);
    if (id) next.set('machine', id);
    else next.delete('machine');
    setParams(next, { replace: true });
  };
  const machine = fleet.data?.machines.find((m) => m.id === editing);
  return (
    <Section
      id="machines"
      title="Machines"
      intro="Each machine's policy decides how much of it jobs may use. A recipe can ask for less, never more."
    >
      <Card flush>
        <Loadable query={fleet} what="machines" lines={4}>
          {(data) =>
            data.machines.length === 0 ? (
              <Empty title="No machines yet" icon="machines">
                Add one below.
              </Empty>
            ) : (
              <div className="table-wrap">
                <table className="table table-stack">
                  <thead>
                    <tr>
                      <th scope="col">Machine</th>
                      <th scope="col">Status</th>
                      <th scope="col">Profile</th>
                      <th scope="col">Protected for owner</th>
                      <th scope="col">Jobs at once</th>
                      <th scope="col">
                        <span className="sr-only">Actions</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.machines.map((m) => (
                      <tr key={m.id}>
                        <th scope="row" data-label="Machine">
                          {m.name}
                        </th>
                        <td data-label="Status">
                          <StatusBadge status={machineStatus(m)} />
                        </td>
                        <td data-label="Profile">{profileLabel(m.policy.profile)}</td>
                        <td data-label="Protected for owner">
                          {m.policy.foreground_cpu_percent}% CPU, {m.policy.foreground_memory_percent}% memory
                        </td>
                        <td data-label="Jobs at once">{m.policy.max_concurrent_attempts ?? 'no limit'}</td>
                        <td data-label="Actions" className="cell-actions">
                          <button type="button" className="btn btn-small" onClick={() => open(m.id)}>
                            {admin ? 'Edit' : 'View'}
                            <span className="sr-only"> {m.name}</span>
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )
          }
        </Loadable>
      </Card>
      <Drawer open={machine !== undefined} title={machine ? `${machine.name}: policy` : 'Machine'} onClose={() => open(null)}>
        {machine ? <MachineEditor machine={machine} admin={admin} /> : null}
      </Drawer>
    </Section>
  );
}

function MachineEditor({ machine, admin }: { machine: MachineView; admin: boolean }) {
  const update = useUpdateMachine(machine.id);
  const [askRetire, setAskRetire] = useState(false);
  const draining = machine.flags.includes('draining') || machine.local_control.draining;
  const gone = machine.lifecycle === 'retired' || machine.lifecycle === 'revoked';
  return (
    <>
      <p className="muted">
        {profileLabel(machine.policy.profile)}: {profileDetail(machine.policy.profile)} Policy revision{' '}
        {machine.policy.revision}.
      </p>
      <MachineForm
        // A saved change comes back with a new revision; start again from it.
        key={`${machine.policy.revision}/${machine.name}`}
        machine={machine}
        disabled={!admin || gone || update.isPending}
        saving={update.isPending}
        onSave={(body) => update.mutate(body)}
      />
      <div aria-live="polite">
        {update.isSuccess ? (
          <Callout tone="ok">
            Saved. Policy revision {update.data.policy.revision}; jobs may now use up to{' '}
            {formatCpu(update.data.managed_ceiling.cpu_milli)} and {formatBytes(update.data.managed_ceiling.memory_bytes)};
            lifecycle "{lifecycleLabel(update.data.lifecycle)}".
          </Callout>
        ) : null}
      </div>
      {update.error ? <ErrorPanel error={update.error} /> : null}

      <h3 className="subhead">Take it out of service</h3>
      <p className="hint">
        Draining lets running jobs finish and takes nothing new. Retiring stops all offers and revokes the machine's
        credentials.
        {machine.local_control.paused ? ' The owner has also paused this machine locally; only they can resume it.' : ''}
      </p>
      <div className="actions actions-start">
        <button
          type="button"
          className="btn"
          disabled={!admin || gone || update.isPending}
          onClick={() => update.mutate({ drain: !draining })}
        >
          {draining ? 'Stop draining' : 'Drain'}
        </button>
        <button
          type="button"
          className="btn btn-danger"
          disabled={!admin || gone || update.isPending}
          onClick={() => setAskRetire(true)}
        >
          Retire…
        </button>
      </div>
      <ConfirmDialog
        open={askRetire}
        title={`Retire ${machine.name}?`}
        confirmLabel="Retire machine"
        cancelLabel="Keep it"
        danger
        busy={update.isPending}
        onCancel={() => setAskRetire(false)}
        onConfirm={() => update.mutate({ retire: true }, { onSettled: () => setAskRetire(false) })}
      >
        <p>
          Retiring stops all offers to this machine and revokes its credentials. To use it again it has to be added
          again with a new pairing code.
        </p>
        <p>
          {machine.attempts.length > 0
            ? `It currently holds ${machine.attempts.length} ${machine.attempts.length === 1 ? 'job' : 'jobs'} (${formatResources(machine.reserved)}). Their reservations stay held until cleanup is confirmed.`
            : 'It runs nothing right now.'}
        </p>
      </ConfirmDialog>
    </>
  );
}

interface MachineDraft {
  name: string;
  acceptBuilds: boolean;
  cpuPercent: number | null;
  memoryPercent: number | null;
  memoryMinGib: number | null;
  availableFloorGib: number | null;
  diskPercent: number | null;
  diskMinGib: number | null;
  maxAttempts: number | null;
  requireAc: boolean;
}

function draftOf(machine: MachineView): MachineDraft {
  const p = machine.policy;
  return {
    name: machine.name,
    acceptBuilds: p.accept_builds,
    cpuPercent: p.foreground_cpu_percent,
    memoryPercent: p.foreground_memory_percent,
    memoryMinGib: toGib(p.foreground_memory_min_bytes),
    availableFloorGib: toGib(p.available_memory_floor_bytes),
    diskPercent: p.disk_free_floor_percent,
    diskMinGib: toGib(p.disk_free_floor_min_bytes),
    maxAttempts: p.max_concurrent_attempts ?? null,
    requireAc: p.require_ac_power,
  };
}

/** The policy a draft describes, or the first thing wrong with the draft. */
export function policyFromDraft(base: MachinePolicy, draft: MachineDraft): MachinePolicy | string {
  const percent = (value: number | null, what: string) =>
    value === null || !Number.isInteger(value) || value < 0 || value > 100
      ? `${what} must be a whole number from 0 to 100.`
      : null;
  const amount = (value: number | null, what: string) =>
    value === null || value < 0 ? `${what} must be zero or more.` : null;
  const problem =
    percent(draft.cpuPercent, 'The CPU kept for the owner') ??
    percent(draft.memoryPercent, 'The memory kept for the owner') ??
    amount(draft.memoryMinGib, 'The minimum memory kept for the owner') ??
    amount(draft.availableFloorGib, 'The free-memory floor') ??
    percent(draft.diskPercent, 'The free-disk floor') ??
    amount(draft.diskMinGib, 'The minimum free disk') ??
    (draft.maxAttempts !== null && (!Number.isInteger(draft.maxAttempts) || draft.maxAttempts < 1)
      ? 'Jobs at once must be a whole number of at least 1, or empty for no limit.'
      : null);
  if (problem) return problem;
  return {
    ...base,
    accept_builds: draft.acceptBuilds,
    foreground_cpu_percent: draft.cpuPercent as number,
    foreground_memory_percent: draft.memoryPercent as number,
    foreground_memory_min_bytes: fromGib(draft.memoryMinGib as number),
    available_memory_floor_bytes: fromGib(draft.availableFloorGib as number),
    disk_free_floor_percent: draft.diskPercent as number,
    disk_free_floor_min_bytes: fromGib(draft.diskMinGib as number),
    max_concurrent_attempts: draft.maxAttempts ?? undefined,
    require_ac_power: draft.requireAc,
  };
}

function MachineForm({
  machine,
  disabled,
  saving,
  onSave,
}: {
  machine: MachineView;
  disabled: boolean;
  saving: boolean;
  onSave: (body: { name?: string; policy?: MachinePolicy }) => void;
}) {
  const initial = draftOf(machine);
  const [draft, setDraft] = useState(initial);
  const set = <K extends keyof MachineDraft>(key: K, value: MachineDraft[K]) =>
    setDraft((previous) => ({ ...previous, [key]: value }));

  const policy = policyFromDraft(machine.policy, draft);
  const nameProblem = draft.name.trim() === '' ? 'The name must not be empty.' : null;
  const problem = nameProblem ?? (typeof policy === 'string' ? policy : null);
  const nameChanged = draft.name.trim() !== machine.name;
  const policyChanged = !sameJson(draftOf(machine), { ...draft, name: machine.name });
  const hardware = machine.inventory.hardware;
  const after = typeof policy === 'string' ? null : managedCeiling(policy, hardware);
  const before = machine.managed_ceiling;

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (problem || typeof policy === 'string') return;
    onSave({
      ...(nameChanged ? { name: draft.name.trim() } : {}),
      ...(policyChanged ? { policy } : {}),
    });
  };

  return (
    <form onSubmit={submit} className="form">
      <fieldset disabled={disabled}>
        <legend className="sr-only">Policy of {machine.name}</legend>
        <Field label="Name" hint="A display label. The machine's identity does not change.">
          {(id, hintId) => (
            <input
              id={id}
              value={draft.name}
              aria-describedby={hintId}
              onChange={(event) => set('name', event.target.value)}
              autoComplete="off"
            />
          )}
        </Field>
        <CheckField
          label="Take new builds"
          hint="Turn off to stop new jobs from here. Running jobs continue."
          checked={draft.acceptBuilds}
          onChange={(value) => set('acceptBuilds', value)}
        />
        <h3 className="subhead">Kept for the owner</h3>
        <div className="form-grid">
          <NumberField
            label="CPU"
            hint="Share of the CPUs jobs never get."
            unit="%"
            max={100}
            value={draft.cpuPercent}
            onChange={(value) => set('cpuPercent', value)}
          />
          <NumberField
            label="Memory"
            hint="Share of memory jobs never get…"
            unit="%"
            max={100}
            value={draft.memoryPercent}
            onChange={(value) => set('memoryPercent', value)}
          />
          <NumberField
            label="Memory, at least"
            hint="…but never less than this."
            unit="GiB"
            step={0.5}
            value={draft.memoryMinGib}
            onChange={(value) => set('memoryMinGib', value)}
          />
        </div>
        <h3 className="subhead">Limits</h3>
        <div className="form-grid">
          <NumberField
            label="Free memory floor"
            hint="No new job starts if free memory would drop below this."
            unit="GiB"
            step={0.5}
            value={draft.availableFloorGib}
            onChange={(value) => set('availableFloorGib', value)}
          />
          <NumberField
            label="Free disk floor"
            hint="Share of the work disk that must stay free…"
            unit="%"
            max={100}
            value={draft.diskPercent}
            onChange={(value) => set('diskPercent', value)}
          />
          <NumberField
            label="Free disk, at least"
            hint="…but never less than this."
            unit="GiB"
            value={draft.diskMinGib}
            onChange={(value) => set('diskMinGib', value)}
          />
          <NumberField
            label="Jobs at once"
            hint="Leave empty for no limit."
            min={1}
            placeholder="no limit"
            value={draft.maxAttempts}
            onChange={(value) => set('maxAttempts', value)}
          />
        </div>
        <CheckField
          label="Take new jobs only on external power"
          checked={draft.requireAc}
          onChange={(value) => set('requireAc', value)}
        />

        <div className="ceiling" aria-live="polite">
          <p>
            Jobs may use up to <strong>{formatCpu(before.cpu_milli)}</strong> and{' '}
            <strong>{formatBytes(before.memory_bytes)}</strong> now.
          </p>
          {after ? (
            after.cpu_milli === before.cpu_milli && after.memory_bytes === before.memory_bytes ? (
              <p className="muted">These changes leave that as it is.</p>
            ) : (
              <p>
                After saving: <strong>{formatCpu(after.cpu_milli)}</strong> and{' '}
                <strong>{formatBytes(after.memory_bytes)}</strong>.
              </p>
            )
          ) : (
            <p className="muted">After saving: cannot be worked out until the form is valid.</p>
          )}
          {after &&
          (after.cpu_milli < machine.reserved.cpu_milli || after.memory_bytes < machine.reserved.memory_bytes) ? (
            <p className="note note-warn">
              That is below what jobs hold on this machine now ({formatResources(machine.reserved)}). They keep their
              reservation; nothing new starts until it fits again.
            </p>
          ) : null}
          <p className="hint">
            Worked out from its {hardware.logical_cpus} CPUs and {formatBytes(hardware.memory_bytes)} of memory. The
            controller's own figure is shown after saving.
          </p>
        </div>

        {problem ? <p className="note note-bad">{problem}</p> : null}
        <div className="actions actions-start">
          <button type="submit" className="btn btn-primary" disabled={problem !== null || (!nameChanged && !policyChanged)}>
            {saving ? 'Saving…' : 'Save'}
          </button>
          <button
            type="button"
            className="btn"
            disabled={!nameChanged && !policyChanged}
            onClick={() => setDraft(initial)}
          >
            Discard changes
          </button>
        </div>
      </fieldset>
    </form>
  );
}

// ------------------------------------------------------------- add machine

function AddMachineSection({ admin }: { admin: boolean }) {
  const create = useCreatePairingCode();
  const now = useNow(1000);
  const [profile, setProfile] = useState<string>('dedicated_builder');
  const [minutes, setMinutes] = useState<number | null>(null);
  const code = create.data;
  const remaining = code ? code.expires_at - now : 0;
  const command = code ? `bella worker enroll ${shellQuote(code.enrollment)}` : '';
  const minutesProblem =
    minutes !== null && (!Number.isInteger(minutes) || minutes < 1)
      ? 'The lifetime must be a whole number of minutes, or empty for the default.'
      : null;

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (minutesProblem) return;
    create.mutate({ profile, ...(minutes === null ? {} : { ttl_secs: minutes * 60 }) });
  };

  return (
    <Section
      id="add-machine"
      title="Add a machine"
      intro="Two steps: create a pairing code here, then run one command on the machine. The code works once and expires soon."
    >
      <div className="steps">
        <Card className="step-card">
          <p className="step-number">Step 1</p>
          <h3>Create a pairing code</h3>
          <form onSubmit={submit} className="form">
            <fieldset disabled={!admin || create.isPending}>
              <legend className="sr-only">New pairing code</legend>
              <Field label="What kind of machine" hint={profileDetail(profile)}>
                {(id, hintId) => (
                  <select id={id} value={profile} aria-describedby={hintId} onChange={(event) => setProfile(event.target.value)}>
                    {MachineProfile.known.map((value) => (
                      <option key={value} value={value}>
                        {profileLabel(value)}
                      </option>
                    ))}
                  </select>
                )}
              </Field>
              <NumberField
                label="Code lifetime"
                hint="Leave empty for the controller's default."
                unit="minutes"
                min={1}
                placeholder="default"
                value={minutes}
                onChange={setMinutes}
              />
              {minutesProblem ? <p className="note note-bad">{minutesProblem}</p> : null}
              <div className="actions actions-start">
                <button type="submit" className="btn btn-primary" disabled={minutesProblem !== null}>
                  <Icon name="key" size={16} /> {create.isPending ? 'Creating…' : 'Create pairing code'}
                </button>
              </div>
            </fieldset>
          </form>
          {create.error ? <ErrorPanel error={create.error} /> : null}
        </Card>

        <Card className={`step-card${code ? '' : ' step-waiting'}`}>
          <p className="step-number">Step 2</p>
          <h3>Run this on the machine</h3>
          {code ? (
            <div aria-live="polite">
              {remaining > 0 ? (
                <p>
                  <strong>Expires in {formatCountdown(remaining)}</strong>{' '}
                  <span className="muted">(at {formatTimestamp(code.expires_at)}). Works once.</span>
                </p>
              ) : (
                <Callout tone="bad">This code expired at {formatTimestamp(code.expires_at)}. Create a new one.</Callout>
              )}
              <div className="codeblock">
                <pre>{command}</pre>
                <CopyButton value={command} label="Copy command" text />
              </div>
              <details className="disclosure">
                <summary>Pairing code and enrollment string</summary>
                <p>
                  Code <code className="big">{code.code}</code> <CopyButton value={code.code} label="Copy pairing code" />
                </p>
                <p className="muted">
                  The enrollment string carries the controller's address, its certificate fingerprint and the code.
                </p>
                <div className="codeblock">
                  <pre>{code.enrollment}</pre>
                  <CopyButton value={code.enrollment} label="Copy enrollment string" text />
                </div>
              </details>
            </div>
          ) : (
            <p className="muted">The command to run appears here once the code is created.</p>
          )}
        </Card>
      </div>
    </Section>
  );
}

// ---------------------------------------------------------------- projects

function ProjectsSection({ admin }: { admin: boolean }) {
  const projects = useProjects();
  const [editing, setEditing] = useState<string | null>(null);
  const project = projects.data?.find((p) => p.name === editing);
  return (
    <Section
      id="projects"
      title="Projects"
      intro="Projects share the fleet by weight. Limits here cap how many jobs and services a project runs at once."
    >
      <Card flush>
        <Loadable query={projects} what="projects" lines={3}>
          {(list) =>
            list.length === 0 ? (
              <Empty title="No projects yet" icon="folder">
                A project appears the first time a job is submitted for it.
              </Empty>
            ) : (
              <div className="table-wrap">
                <table className="table table-stack">
                  <thead>
                    <tr>
                      <th scope="col">Project</th>
                      <th scope="col" className="num">
                        Weight
                      </th>
                      <th scope="col">Jobs at once</th>
                      <th scope="col">Services at once</th>
                      <th scope="col">Cloud</th>
                      <th scope="col">
                        <span className="sr-only">Actions</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {list.map((p) => (
                      <tr key={p.id}>
                        <th scope="row" data-label="Project">
                          {p.name}
                        </th>
                        <td data-label="Weight" className="num">
                          {p.policy.weight}
                        </td>
                        <td data-label="Jobs at once">{p.policy.max_concurrent_attempts ?? 'no limit'}</td>
                        <td data-label="Services at once">{p.policy.max_environments ?? 'no limit'}</td>
                        <td data-label="Cloud">
                          {cloudModeLabel(p.policy.cloud)}
                          {p.policy.source_lan_only ? <span className="muted">, source stays on the LAN</span> : null}
                        </td>
                        <td data-label="Actions" className="cell-actions">
                          <button type="button" className="btn btn-small" onClick={() => setEditing(p.name)}>
                            {admin ? 'Edit' : 'View'}
                            <span className="sr-only"> {p.name}</span>
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )
          }
        </Loadable>
      </Card>
      <Drawer open={project !== undefined} title={project ? `Project ${project.name}` : 'Project'} onClose={() => setEditing(null)}>
        {project ? <ProjectForm project={project} admin={admin} /> : null}
      </Drawer>
    </Section>
  );
}

function ProjectForm({ project, admin }: { project: ProjectView; admin: boolean }) {
  const put = usePutProject(project.name);
  const [draft, setDraft] = useState<ProjectPolicy>(project.policy);
  const [weight, setWeight] = useState<number | null>(project.policy.weight);
  // When the saved policy changes, start again from what the controller has.
  const saved = JSON.stringify(project.policy);
  useEffect(() => {
    const policy = JSON.parse(saved) as ProjectPolicy;
    setDraft(policy);
    setWeight(policy.weight);
  }, [saved]);
  const limit = (value: number | null | undefined, what: string) =>
    value !== null && value !== undefined && (!Number.isInteger(value) || value < 0)
      ? `${what} must be a whole number, or empty for no limit.`
      : null;
  const problem =
    (weight === null || !Number.isInteger(weight) || weight < 1 ? 'The weight must be a whole number of at least 1.' : null) ??
    limit(draft.max_concurrent_attempts, 'Jobs at once') ??
    limit(draft.max_environments, 'Services at once');
  const next: ProjectPolicy = { ...draft, weight: weight ?? project.policy.weight };
  const changed = !sameJson(next, project.policy);
  const cloudModes = CloudMode.known.includes(draft.cloud as (typeof CloudMode.known)[number])
    ? CloudMode.known
    : [...CloudMode.known, draft.cloud];

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!problem) put.mutate(next);
  };

  return (
    <>
      <form onSubmit={submit} className="form">
        <fieldset disabled={!admin || put.isPending}>
          <legend className="sr-only">Policy of project {project.name}</legend>
          <div className="form-grid">
            <NumberField
              label="Fair-share weight"
              hint="Relative to other projects. More sessions do not add weight."
              min={1}
              value={weight}
              onChange={setWeight}
            />
            <NumberField
              label="Jobs at once"
              hint="Running plus starting. Leave empty for no limit."
              placeholder="no limit"
              value={draft.max_concurrent_attempts ?? null}
              onChange={(value) => setDraft({ ...draft, max_concurrent_attempts: value ?? undefined })}
            />
            <NumberField
              label="Services at once"
              hint="Services kept running at once. Leave empty for no limit."
              placeholder="no limit"
              value={draft.max_environments ?? null}
              onChange={(value) => setDraft({ ...draft, max_environments: value ?? undefined })}
            />
          </div>
          <Field label="Cloud capacity" hint="Whether paid capacity may be created for this project's jobs.">
            {(id, hintId) => (
              <select
                id={id}
                value={draft.cloud}
                aria-describedby={hintId}
                onChange={(event) => setDraft({ ...draft, cloud: event.target.value })}
              >
                {cloudModes.map((mode) => (
                  <option key={mode} value={mode}>
                    {cloudModeLabel(mode)}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <CheckField
            label="Source may not leave the LAN"
            hint="Jobs are never placed on cloud or remote machines."
            checked={draft.source_lan_only}
            onChange={(value) => setDraft({ ...draft, source_lan_only: value })}
          />
          <CheckField
            label="May use urgent priority"
            checked={draft.allow_urgent}
            onChange={(value) => setDraft({ ...draft, allow_urgent: value })}
          />
          {problem ? <p className="note note-bad">{problem}</p> : null}
          <div className="actions actions-start">
            <button type="submit" className="btn btn-primary" disabled={problem !== null || !changed}>
              {put.isPending ? 'Saving…' : 'Save'}
            </button>
          </div>
        </fieldset>
      </form>
      <div aria-live="polite">
        {put.isSuccess ? (
          <Callout tone="ok">
            Saved. Weight {put.data.policy.weight}, "{cloudModeLabel(put.data.policy.cloud)}".
          </Callout>
        ) : null}
      </div>
      {put.error ? <ErrorPanel error={put.error} /> : null}
    </>
  );
}

// ------------------------------------------------------------------ tokens

const ROLE_DETAIL: Record<string, string> = {
  viewer: 'can look at everything, change nothing',
  developer: 'can also submit and cancel jobs and manage services',
  admin: 'can also change machines, projects and tokens',
};

function TokensSection({ admin }: { admin: boolean }) {
  const create = useCreateToken();
  const [name, setName] = useState('');
  const [role, setRole] = useState<string>('developer');

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (name.trim() === '') return;
    create.mutate({ name: name.trim(), role }, { onSuccess: () => setName('') });
  };

  return (
    <Section
      id="tokens"
      title="Access tokens"
      intro="A token signs in a person, the CLI or an agent. It is shown once, when it is created."
    >
      <Card>
        <form onSubmit={submit} className="form">
          <fieldset disabled={!admin || create.isPending}>
            <legend className="sr-only">New access token</legend>
            <div className="form-grid">
              <Field label="Name" hint="Who or what will use it.">
                {(id, hintId) => (
                  <input
                    id={id}
                    value={name}
                    aria-describedby={hintId}
                    onChange={(event) => setName(event.target.value)}
                    autoComplete="off"
                  />
                )}
              </Field>
              <Field label="Role" hint={`A ${roleLabel(role)} ${ROLE_DETAIL[role] ?? ''}.`}>
                {(id, hintId) => (
                  <select id={id} value={role} aria-describedby={hintId} onChange={(event) => setRole(event.target.value)}>
                    {Role.known.map((value) => (
                      <option key={value} value={value}>
                        {roleLabel(value)}
                      </option>
                    ))}
                  </select>
                )}
              </Field>
            </div>
            <div className="actions actions-start">
              <button type="submit" className="btn btn-primary" disabled={name.trim() === ''}>
                <Icon name="plus" size={16} /> {create.isPending ? 'Creating…' : 'Create token'}
              </button>
            </div>
          </fieldset>
        </form>
        {create.error ? <ErrorPanel error={create.error} /> : null}
        {create.data ? (
          <div className="token-once" aria-live="polite">
            <Callout tone="warn" title="Copy this token now">
              It is shown only once: the controller keeps only a hash of it.
            </Callout>
            <div className="codeblock">
              <pre>{create.data.token}</pre>
              <CopyButton value={create.data.token} label="Copy token" text />
            </div>
            <button type="button" className="btn btn-small" onClick={() => create.reset()}>
              I have copied it; hide it
            </button>
          </div>
        ) : null}
      </Card>
    </Section>
  );
}
