// Shared building blocks: page headers, cards, status words, chips, tabs,
// help popovers, loading / empty / error states, copy buttons, dialogs and
// form fields.

import type { UseQueryResult } from '@tanstack/react-query';
import {
  useEffect,
  useId,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from 'react';
import { describeError } from '../api/client';
import { formatRelative, formatTimestamp, shortId } from '../lib/format';
import type { Tone } from '../lib/labels';
import type { FlagChip, Status } from '../lib/status';
import { Icon, type IconName } from './icons';

// ------------------------------------------------------------------ layout

export function PageHeader({
  title,
  description,
  actions,
  children,
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  /** Status words, chips or facts under the title. */
  children?: ReactNode;
}) {
  return (
    <header className="page-header">
      <div className="page-header-text">
        <h1>{title}</h1>
        {description ? <p className="page-description">{description}</p> : null}
        {children ? <div className="page-meta">{children}</div> : null}
      </div>
      {actions ? <div className="page-actions">{actions}</div> : null}
    </header>
  );
}

/** A white card with an optional heading row. */
export function Card({
  title,
  description,
  actions,
  children,
  className,
  flush = false,
}: {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  /** No inner padding, for a table that runs edge to edge. */
  flush?: boolean;
}) {
  const id = useId();
  return (
    <section className={`card${className ? ` ${className}` : ''}`} aria-labelledby={title ? id : undefined}>
      {title ? (
        <div className="card-head">
          <div>
            <h2 id={id}>{title}</h2>
            {description ? <p className="card-description">{description}</p> : null}
          </div>
          {actions ? <div className="card-actions">{actions}</div> : null}
        </div>
      ) : null}
      <div className={flush ? 'card-body card-flush' : 'card-body'}>{children}</div>
    </section>
  );
}

// ---------------------------------------------------------------- statuses

/** A coloured dot and a word. The word carries the meaning; the colour repeats it. */
export function StatusBadge({ status, live = false }: { status: Status; live?: boolean }) {
  return (
    <span className={`status status-${status.tone}`} title={status.precise}>
      <span className={live ? 'dot dot-live' : 'dot'} aria-hidden="true" />
      {status.label}
    </span>
  );
}

export function Chip({ tone = 'neutral', children, title }: { tone?: Tone; children: ReactNode; title?: string }) {
  return (
    <span className={`chip chip-${tone}`} title={title}>
      {children}
    </span>
  );
}

/** The most important chips, then `+N` for the rest, so a row stays one line. */
export function FlagSummary({ flags, max = 1 }: { flags: FlagChip[]; max?: number }) {
  if (flags.length === 0) return null;
  const shown = flags.slice(0, max);
  const rest = flags.slice(max);
  return (
    <span className="chips chips-nowrap">
      {shown.map((flag) => (
        <Chip key={flag.flag} tone={flag.tone} title={flag.detail}>
          {flag.label}
        </Chip>
      ))}
      {rest.length > 0 ? (
        <span className="chip chip-more" title={rest.map((flag) => flag.label).join(', ')}>
          +{rest.length}
          <span className="sr-only">: {rest.map((flag) => flag.label).join(', ')}</span>
        </span>
      ) : null}
    </span>
  );
}

export function Callout({
  tone = 'info',
  title,
  children,
  role,
}: {
  tone?: Tone;
  title?: ReactNode;
  children?: ReactNode;
  role?: 'alert' | 'status';
}) {
  const icon: IconName = tone === 'bad' || tone === 'warn' ? 'alert' : tone === 'ok' ? 'check' : 'info';
  return (
    <div className={`callout callout-${tone}`} role={role}>
      <Icon name={icon} />
      <div>
        {title ? <p className="callout-title">{title}</p> : null}
        {children ? <div className="callout-body">{children}</div> : null}
      </div>
    </div>
  );
}

// -------------------------------------------------------------------- help

/**
 * A small "?" button that explains a term in place. The explanation is a
 * popover, closed with Escape or a second click.
 */
export function Help({ label, children }: { label: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const ref = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (event: MouseEvent) => {
      if (ref.current && !ref.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', close);
    return () => document.removeEventListener('mousedown', close);
  }, [open]);
  return (
    <span
      className="help"
      ref={ref}
      onKeyDown={(event) => {
        if (event.key === 'Escape' && open) {
          event.stopPropagation();
          setOpen(false);
        }
      }}
    >
      <button
        type="button"
        className="help-button"
        aria-expanded={open}
        aria-controls={id}
        aria-label={label}
        title={label}
        onClick={() => setOpen((value) => !value)}
      >
        <Icon name="help" size={15} />
      </button>
      {open ? (
        <span className="help-pop" id={id} role="note">
          {children}
        </span>
      ) : null}
    </span>
  );
}

// -------------------------------------------------------------------- tabs

export interface TabItem {
  id: string;
  label: string;
  count?: number;
}

/** Tabs with arrow-key navigation. The panels are rendered by the caller with `TabPanel`. */
export function Tabs({
  label,
  tabs,
  value,
  onChange,
  idBase,
}: {
  label: string;
  tabs: TabItem[];
  value: string;
  onChange: (id: string) => void;
  idBase: string;
}) {
  const refs = useRef<Record<string, HTMLButtonElement | null>>({});
  const move = (event: KeyboardEvent, index: number) => {
    const delta = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
    let next = index + delta;
    if (event.key === 'Home') next = 0;
    if (event.key === 'End') next = tabs.length - 1;
    if (next === index) return;
    event.preventDefault();
    const tab = tabs[(next + tabs.length) % tabs.length];
    if (!tab) return;
    onChange(tab.id);
    refs.current[tab.id]?.focus();
  };
  return (
    <div className="tabs" role="tablist" aria-label={label}>
      {tabs.map((tab, index) => {
        const selected = tab.id === value;
        return (
          <button
            key={tab.id}
            ref={(element) => {
              refs.current[tab.id] = element;
            }}
            type="button"
            role="tab"
            id={`${idBase}-tab-${tab.id}`}
            aria-selected={selected}
            aria-controls={`${idBase}-panel-${tab.id}`}
            tabIndex={selected ? 0 : -1}
            className={selected ? 'tab tab-selected' : 'tab'}
            onClick={() => onChange(tab.id)}
            onKeyDown={(event) => move(event, index)}
          >
            {tab.label}
            {tab.count === undefined ? null : <span className="tab-count">{tab.count}</span>}
          </button>
        );
      })}
    </div>
  );
}

export function TabPanel({ idBase, id, children }: { idBase: string; id: string; children: ReactNode }) {
  return (
    <div
      role="tabpanel"
      id={`${idBase}-panel-${id}`}
      aria-labelledby={`${idBase}-tab-${id}`}
      className="tab-panel"
      tabIndex={0}
    >
      {children}
    </div>
  );
}

// --------------------------------------------------- loading, empty, error

/** Placeholder lines in the shape of what is loading, so nothing jumps when it arrives. */
export function Loading({ what, lines = 3 }: { what: string; lines?: number }) {
  return (
    <div className="skeleton" role="status">
      <span className="sr-only">Loading {what}…</span>
      {Array.from({ length: lines }, (_, index) => (
        <span key={index} className="skeleton-line" aria-hidden="true" />
      ))}
    </div>
  );
}

export function Empty({
  title,
  children,
  icon = 'info',
  action,
}: {
  title: string;
  children?: ReactNode;
  icon?: IconName;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <span className="empty-icon" aria-hidden="true">
        <Icon name={icon} size={20} />
      </span>
      <p className="empty-title">{title}</p>
      {children ? <p className="empty-detail">{children}</p> : null}
      {action ? <div className="empty-action">{action}</div> : null}
    </div>
  );
}

export function ErrorPanel({
  error,
  onRetry,
  context,
}: {
  error: unknown;
  onRetry?: () => void;
  /** What the error means for what is on screen, e.g. that it may be out of date. */
  context?: string;
}) {
  const text = describeError(error);
  return (
    <div className="callout callout-bad" role="alert">
      <Icon name="alert" />
      <div>
        <p className="callout-title">{text.title}</p>
        <p className="callout-body pre">{text.detail}</p>
        {context ? <p className="callout-body">{context}</p> : null}
        {onRetry ? (
          <button type="button" className="btn btn-small" onClick={onRetry}>
            <Icon name="refresh" size={14} /> Try again
          </button>
        ) : null}
      </div>
    </div>
  );
}

/**
 * Loading, error and data states of one query. When a refresh fails but an
 * earlier answer exists, the error is shown above that answer together with
 * its age, so stale data is never mistaken for current data.
 */
export function Loadable<T>({
  query,
  what,
  lines,
  children,
}: {
  query: UseQueryResult<T>;
  what: string;
  lines?: number;
  children: (data: T) => ReactNode;
}) {
  if (query.data === undefined) {
    if (query.error) return <ErrorPanel error={query.error} onRetry={() => void query.refetch()} />;
    return <Loading what={what} lines={lines} />;
  }
  return (
    <>
      {query.error ? (
        <ErrorPanel
          error={query.error}
          onRetry={() => void query.refetch()}
          context={`What is shown below was read at ${formatTimestamp(query.dataUpdatedAt)} and may be out of date.`}
        />
      ) : null}
      {children(query.data)}
    </>
  );
}

// -------------------------------------------------------------------- copy

async function copyText(text: string): Promise<boolean> {
  try {
    if (window.isSecureContext && navigator.clipboard) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Fall through to the selection-based copy.
  }
  // The clipboard API needs a secure context; a controller on plain HTTP is
  // not one, so fall back to copying a selection.
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.className = 'offscreen';
  document.body.append(area);
  area.select();
  let copied = false;
  try {
    copied = document.execCommand('copy');
  } catch {
    copied = false;
  }
  area.remove();
  return copied;
}

/** Copies `value`. With `text`, the button shows its label; otherwise it is an icon. */
export function CopyButton({ value, label = 'Copy', text = false }: { value: string; label?: string; text?: boolean }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle');
  useEffect(() => {
    if (state === 'idle') return;
    const timer = setTimeout(() => setState('idle'), 2500);
    return () => clearTimeout(timer);
  }, [state]);
  const said = state === 'copied' ? 'Copied' : state === 'failed' ? 'Copy failed: select it by hand' : label;
  return (
    <button
      type="button"
      className={text ? 'btn btn-small' : 'icon-btn icon-btn-small'}
      title={text ? undefined : said}
      aria-label={text ? undefined : said}
      onClick={() => void copyText(value).then((copied) => setState(copied ? 'copied' : 'failed'))}
    >
      <Icon name={state === 'copied' ? 'check' : 'copy'} size={text ? 14 : 15} />
      {text ? <span aria-live="polite">{said}</span> : <span className="sr-only" aria-live="polite">{state === 'idle' ? '' : said}</span>}
    </button>
  );
}

/** A long identifier shown by its end, with the full value on hover and on copy. */
export function Ident({ value, copy = false }: { value: string; copy?: boolean }) {
  return (
    <span className="ident">
      <code title={value}>{shortId(value)}</code>
      {copy ? <CopyButton value={value} label="Copy full id" /> : null}
    </span>
  );
}

/** A relative time with the absolute time on hover. */
export function Ago({ at, now }: { at: number; now: number }) {
  return (
    <time dateTime={new Date(at).toISOString()} title={formatTimestamp(at)}>
      {formatRelative(at, now)}
    </time>
  );
}

// ----------------------------------------------------------------- dialogs

function useModal(open: boolean) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) {
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    } else if (!open && dialog.open) {
      if (typeof dialog.close === 'function') dialog.close();
      else dialog.removeAttribute('open');
    }
  }, [open]);
  return ref;
}

export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel,
  cancelLabel,
  danger = false,
  busy = false,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  children: ReactNode;
  confirmLabel: string;
  cancelLabel: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const ref = useModal(open);
  const titleId = useId();
  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        if (!busy) onCancel();
      }}
    >
      {open ? (
        <>
          <h2 id={titleId}>{title}</h2>
          <div className="dialog-body">{children}</div>
          <div className="actions">
            <button type="button" className="btn" onClick={onCancel} disabled={busy}>
              {cancelLabel}
            </button>
            <button
              type="button"
              className={danger ? 'btn btn-danger' : 'btn btn-primary'}
              onClick={onConfirm}
              disabled={busy}
            >
              {busy ? 'Working…' : confirmLabel}
            </button>
          </div>
        </>
      ) : null}
    </dialog>
  );
}

/** A side panel over the page, for editing one thing without leaving the list. */
export function Drawer({
  open,
  title,
  onClose,
  children,
}: {
  open: boolean;
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useModal(open);
  const titleId = useId();
  return (
    <dialog
      ref={ref}
      className="drawer"
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      {open ? (
        <>
          <div className="drawer-head">
            <h2 id={titleId}>{title}</h2>
            <button type="button" className="icon-btn" aria-label="Close" onClick={onClose}>
              <Icon name="close" />
            </button>
          </div>
          <div className="drawer-body">{children}</div>
        </>
      ) : null}
    </dialog>
  );
}

// ------------------------------------------------------------------- forms

export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: (id: string, hintId: string | undefined) => ReactNode;
}) {
  const id = useId();
  const hintId = hint ? `${id}-hint` : undefined;
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      {children(id, hintId)}
      {hint ? (
        <p className="hint" id={hintId}>
          {hint}
        </p>
      ) : null}
    </div>
  );
}

export function NumberField({
  label,
  hint,
  value,
  onChange,
  min = 0,
  max,
  step = 1,
  unit,
  disabled,
  placeholder,
}: {
  label: string;
  hint?: string;
  /** `null` is an empty box, for limits that may be left unset. */
  value: number | null;
  onChange: (value: number | null) => void;
  min?: number;
  max?: number;
  step?: number;
  unit?: string;
  disabled?: boolean;
  placeholder?: string;
}) {
  return (
    <Field label={label} hint={hint}>
      {(id, hintId) => (
        <span className="input-unit">
          <input
            id={id}
            type="number"
            inputMode="decimal"
            min={min}
            max={max}
            step={step}
            value={value ?? ''}
            placeholder={placeholder}
            disabled={disabled}
            aria-describedby={hintId}
            onChange={(event) => {
              const text = event.target.value;
              const parsed = Number(text);
              onChange(text === '' || !Number.isFinite(parsed) ? null : parsed);
            }}
          />
          {unit ? <span className="unit">{unit}</span> : null}
        </span>
      )}
    </Field>
  );
}

export function CheckField({
  label,
  hint,
  checked,
  onChange,
  disabled,
}: {
  label: string;
  hint?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
}) {
  const id = useId();
  return (
    <div className="field field-check">
      <input
        id={id}
        type="checkbox"
        checked={checked}
        disabled={disabled}
        aria-describedby={hint ? `${id}-hint` : undefined}
        onChange={(event) => onChange(event.target.checked)}
      />
      <label htmlFor={id}>{label}</label>
      {hint ? (
        <p className="hint" id={`${id}-hint`}>
          {hint}
        </p>
      ) : null}
    </div>
  );
}

/** Label / value rows. */
export function Facts({ children, columns = false }: { children: ReactNode; columns?: boolean }) {
  return <dl className={columns ? 'facts facts-columns' : 'facts'}>{children}</dl>;
}

export function Fact({ label, children }: { label: ReactNode; children: ReactNode }) {
  return (
    <div className="fact">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}
