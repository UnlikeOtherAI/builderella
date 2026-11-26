import { useEffect, useState, type FormEvent } from 'react';
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
import {
  CheckField,
  Chip,
  ConfirmDialog,
  CopyButton,
  Empty,
  ErrorPanel,
  Field,
  Loadable,
  NumberField,
} from '../components/ui';
import { formatBytes, formatCpu, formatResources, formatTimestamp, shellQuote } from '../lib/format';
import { cloudModeLabel, lifecycleLabel, profileDetail, profileLabel, roleLabel } from '../lib/labels';
import { formatCountdown } from '../lib/lease';
import { managedCeiling } from '../lib/policy';
import { sameJson } from '../lib/same';

const GIB = 1024 ** 3;
const toGib = (bytes: number) => Math.round((bytes / GIB) * 100) / 100;
const fromGib = (gib: number) => Math.round(gib * GIB);

export function AdminView() {
  const who = useWhoAmI();
  const admin = who.data?.role === 'admin';
  return (
    <>
      <h1>Administration</h1>
      {who.data && !admin ? (
        <p className="note note-warn" role="status">
          You are signed in as {roleLabel(who.data.role)}. Everything here is read-only: changing
          machines, projects, pairing codes and tokens needs the admin role.
        </p>
      ) : null}
      {who.error && !who.data ? <ErrorPanel error={who.error} onRetry={() => void who.refetch()} /> : null}
      <MachinesSection admin={admin} />
      <AddMachineSection admin={admin} />
      <ProjectsSection admin={admin} />
      <TokensSection admin={admin} />
    </>
  );
}

// ---------------------------------------------------------------- machines

function MachinesSection({ admin }: { admin: boolean }) {
  const fleet = useFleet();
  return (
    <section aria-labelledby="admin-machines">
      <h2 id="admin-machines">Machines</h2>
      <p className="muted">
        A machine's policy belongs to its owner. A project recipe can ask for less than these limits,
        never more.
      </p>
      <Loadable query={fleet} what="machines">
        {(data) =>
          data.machines.length === 0 ? (
            <Empty title="No machines are enrolled">Add one below.</Empty>
          ) : (
            <div className="stack-list">
              {data.machines.map((machine) => (
                <MachineAdmin key={machine.id} machine={machine} admin={admin} />
              ))}
            </div>
          )
        }
      </Loadable>
    </section>
  );
}

function MachineAdmin({ machine, admin }: { machine: MachineView; admin: boolean }) {
  const update = useUpdateMachine(machine.id);
  const [askRetire, setAskRetire] = useState(false);
  const draining = machine.flags.includes('draining') || machine.local_control.draining;
  const gone = machine.lifecycle === 'retired' || machine.lifecycle === 'revoked';

  return (
    <details className="card">
      <summary>
        <strong>{machine.name}</strong>{' '}
        <span className="muted">
          {profileLabel(machine.policy.profile)}, policy revision {machine.policy.revision}
        </span>{' '}
        {gone ? <Chip tone="warn">{lifecycleLabel(machine.lifecycle)}</Chip> : null}
        {draining ? <Chip tone="warn">Draining</Chip> : null}
        {!machine.policy.accept_builds ? <Chip tone="warn">Builds off</Chip> : null}
      </summary>

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
          <p className="note note-ok">
            Saved. The controller reports policy revision {update.data.policy.revision}, a managed
            ceiling of {formatCpu(update.data.managed_ceiling.cpu_milli)} and{' '}
            {formatBytes(update.data.managed_ceiling.memory_bytes)} memory, lifecycle "
            {lifecycleLabel(update.data.lifecycle)}".
          </p>
        ) : null}
      </div>
      {update.error ? <ErrorPanel error={update.error} /> : null}

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
      <p className="hint">
        Draining lets running work finish and accepts nothing new. Retiring stops offers and revokes the
        machine's credentials.
        {machine.local_control.paused ? ' The owner has also paused this machine locally; only they can resume it.' : ''}
      </p>

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
          Retiring stops all offers to this machine and revokes its credentials. To use it again it has
          to be enrolled again with a new pairing code.
        </p>
        <p>
          {machine.attempts.length > 0
            ? `It currently holds ${machine.attempts.length} ${machine.attempts.length === 1 ? 'attempt' : 'attempts'} (${formatResources(machine.reserved)}). Their reservations stay held until cleanup is confirmed.`
            : 'It holds no attempts right now.'}
        </p>
      </ConfirmDialog>
    </details>
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
    percent(draft.cpuPercent, 'The foreground CPU share') ??
    percent(draft.memoryPercent, 'The foreground memory share') ??
    amount(draft.memoryMinGib, 'The minimum foreground memory') ??
    amount(draft.availableFloorGib, 'The available-memory floor') ??
    percent(draft.diskPercent, 'The free-disk floor') ??
    amount(draft.diskMinGib, 'The minimum free disk') ??
    (draft.maxAttempts !== null && (!Number.isInteger(draft.maxAttempts) || draft.maxAttempts < 1)
      ? 'The concurrency limit must be a whole number of at least 1, or empty for no limit.'
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
        <div className="form-grid">
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
          <NumberField
            label="Foreground CPU share"
            hint="Share of logical CPUs withheld from managed work."
            unit="%"
            max={100}
            value={draft.cpuPercent}
            onChange={(value) => set('cpuPercent', value)}
          />
          <NumberField
            label="Foreground memory share"
            hint="Share of memory withheld from managed work…"
            unit="%"
            max={100}
            value={draft.memoryPercent}
            onChange={(value) => set('memoryPercent', value)}
          />
          <NumberField
            label="Minimum foreground memory"
            hint="…but never less than this."
            unit="GiB"
            step={0.5}
            value={draft.memoryMinGib}
            onChange={(value) => set('memoryMinGib', value)}
          />
          <NumberField
            label="Available-memory floor"
            hint="New work is refused when free memory would drop below this."
            unit="GiB"
            step={0.5}
            value={draft.availableFloorGib}
            onChange={(value) => set('availableFloorGib', value)}
          />
          <NumberField
            label="Free-disk floor"
            hint="Share of the work volume that must stay free…"
            unit="%"
            max={100}
            value={draft.diskPercent}
            onChange={(value) => set('diskPercent', value)}
          />
          <NumberField
            label="Minimum free disk"
            hint="…but never less than this."
            unit="GiB"
            value={draft.diskMinGib}
            onChange={(value) => set('diskMinGib', value)}
          />
          <NumberField
            label="Concurrent attempts"
            hint="Leave empty for no limit."
            min={1}
            placeholder="no limit"
            value={draft.maxAttempts}
            onChange={(value) => set('maxAttempts', value)}
          />
        </div>
        <CheckField
          label="Accept new builds"
          hint="Turn off to pause this machine from here. Running work continues."
          checked={draft.acceptBuilds}
          onChange={(value) => set('acceptBuilds', value)}
        />
        <CheckField
          label="Accept new work only on external power"
          checked={draft.requireAc}
          onChange={(value) => set('requireAc', value)}
        />

        <div className="ceiling" aria-live="polite">
          <h3>Managed ceiling</h3>
          <p>
            Now: {formatCpu(before.cpu_milli)}, {formatBytes(before.memory_bytes)} memory.
          </p>
          {after ? (
            <p>
              After saving: <strong>{formatCpu(after.cpu_milli)}</strong>,{' '}
              <strong>{formatBytes(after.memory_bytes)}</strong> memory
              {after.cpu_milli === before.cpu_milli && after.memory_bytes === before.memory_bytes
                ? ' (unchanged).'
                : '.'}
            </p>
          ) : (
            <p>After saving: cannot be worked out until the form is valid.</p>
          )}
          {after &&
          (after.cpu_milli < machine.reserved.cpu_milli || after.memory_bytes < machine.reserved.memory_bytes) ? (
            <p className="note note-warn">
              That is below what is reserved on this machine now ({formatResources(machine.reserved)}).
              Running work keeps its reservation; nothing new starts until the reservations fit again.
            </p>
          ) : null}
          <p className="hint">
            Worked out here from this machine's {hardware.logical_cpus} logical CPUs and{' '}
            {formatBytes(hardware.memory_bytes)} of memory. The controller's own figure is shown after
            saving.
          </p>
        </div>

        {problem ? <p className="note note-bad">{problem}</p> : null}
        <div className="actions actions-start">
          <button
            type="submit"
            className="btn btn-primary"
            disabled={problem !== null || (!nameChanged && !policyChanged)}
          >
            {saving ? 'Saving…' : 'Save policy'}
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
    <section aria-labelledby="admin-add">
      <h2 id="admin-add">Add machine</h2>
      <p className="muted">
        Create a pairing code, then run the command it gives you on the machine you want to add. The
        code works once and only for a short time.
      </p>
      <form onSubmit={submit} className="form">
        <fieldset disabled={!admin || create.isPending}>
          <legend className="sr-only">New pairing code</legend>
          <div className="form-grid">
            <Field label="Profile" hint={profileDetail(profile)}>
              {(id, hintId) => (
                <select
                  id={id}
                  value={profile}
                  aria-describedby={hintId}
                  onChange={(event) => setProfile(event.target.value)}
                >
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
          </div>
          {minutesProblem ? <p className="note note-bad">{minutesProblem}</p> : null}
          <div className="actions actions-start">
            <button type="submit" className="btn btn-primary" disabled={minutesProblem !== null}>
              {create.isPending ? 'Creating…' : 'Create pairing code'}
            </button>
          </div>
        </fieldset>
      </form>
      {create.error ? <ErrorPanel error={create.error} /> : null}

      {code ? (
        <div className={`card pairing ${remaining <= 0 ? 'pairing-expired' : ''}`} aria-live="polite">
          {remaining > 0 ? (
            <p>
              <strong>Expires in {formatCountdown(remaining)}</strong> (at {formatTimestamp(code.expires_at)}
              ). Single use.
            </p>
          ) : (
            <p className="note note-bad">
              This code expired at {formatTimestamp(code.expires_at)}. Create a new one.
            </p>
          )}
          <h3>Run this on the machine</h3>
          <pre className="block">{command}</pre>
          <CopyButton value={command} label="Copy command" />
          <h3>Pairing code</h3>
          <p>
            <code className="big">{code.code}</code>
          </p>
          <h3>Enrollment string</h3>
          <p className="muted">
            Carries the controller address, its certificate fingerprint and the code.
          </p>
          <pre className="block">{code.enrollment}</pre>
          <CopyButton value={code.enrollment} label="Copy enrollment string" />
        </div>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------- projects

function ProjectsSection({ admin }: { admin: boolean }) {
  const projects = useProjects();
  return (
    <section aria-labelledby="admin-projects">
      <h2 id="admin-projects">Projects</h2>
      <Loadable query={projects} what="projects">
        {(list) =>
          list.length === 0 ? (
            <Empty title="No projects yet">A project is created the first time a snapshot is registered for it.</Empty>
          ) : (
            <div className="stack-list">
              {list.map((project) => (
                <ProjectAdmin key={project.id} project={project} admin={admin} />
              ))}
            </div>
          )
        }
      </Loadable>
    </section>
  );
}

function ProjectAdmin({ project, admin }: { project: ProjectView; admin: boolean }) {
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
    (weight === null || !Number.isInteger(weight) || weight < 1
      ? 'The weight must be a whole number of at least 1.'
      : null) ??
    limit(draft.max_concurrent_attempts, 'The attempt limit') ??
    limit(draft.max_environments, 'The environment limit');
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
    <details className="card">
      <summary>
        <strong>{project.name}</strong>{' '}
        <span className="muted">
          weight {project.policy.weight}, {cloudModeLabel(project.policy.cloud).toLowerCase()}
        </span>
      </summary>
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
              label="Concurrent attempts"
              hint="Running plus preparing. Leave empty for no limit."
              placeholder="no limit"
              value={draft.max_concurrent_attempts ?? null}
              onChange={(value) => setDraft({ ...draft, max_concurrent_attempts: value ?? undefined })}
            />
            <NumberField
              label="Environments"
              hint="Kept-alive environments at once. Leave empty for no limit."
              placeholder="no limit"
              value={draft.max_environments ?? null}
              onChange={(value) => setDraft({ ...draft, max_environments: value ?? undefined })}
            />
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
          </div>
          <CheckField
            label="Source may not leave the LAN"
            hint="Jobs are never placed on cloud or remote workers."
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
              {put.isPending ? 'Saving…' : 'Save project policy'}
            </button>
          </div>
        </fieldset>
      </form>
      <div aria-live="polite">
        {put.isSuccess ? (
          <p className="note note-ok">
            Saved. The controller reports weight {put.data.policy.weight} and "
            {cloudModeLabel(put.data.policy.cloud)}".
          </p>
        ) : null}
      </div>
      {put.error ? <ErrorPanel error={put.error} /> : null}
    </details>
  );
}

// ------------------------------------------------------------------ tokens

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
    <section aria-labelledby="admin-tokens">
      <h2 id="admin-tokens">API tokens</h2>
      <p className="muted">
        A token signs in an agent, the CLI or a person. A developer can submit and cancel jobs; only an
        admin can change machines, projects and tokens.
      </p>
      <form onSubmit={submit} className="form">
        <fieldset disabled={!admin || create.isPending}>
          <legend className="sr-only">New API token</legend>
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
            <Field label="Role">
              {(id) => (
                <select id={id} value={role} onChange={(event) => setRole(event.target.value)}>
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
              {create.isPending ? 'Creating…' : 'Create token'}
            </button>
          </div>
        </fieldset>
      </form>
      {create.error ? <ErrorPanel error={create.error} /> : null}
      {create.data ? (
        <div className="card pairing" aria-live="polite">
          <p className="note note-warn">
            Copy this token now. It is shown once: the controller keeps only a hash, so it cannot be
            shown again.
          </p>
          <pre className="block">{create.data.token}</pre>
          <div className="actions actions-start">
            <CopyButton value={create.data.token} label="Copy token" />
            <button type="button" className="btn" onClick={() => create.reset()}>
              I have copied it; hide it
            </button>
          </div>
        </div>
      ) : null}
    </section>
  );
}
