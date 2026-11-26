// Small shared pieces: status chips, loading / empty / error states,
// copy-to-clipboard, confirmation dialogs and form fields.

import type { UseQueryResult } from '@tanstack/react-query';
import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { describeError } from '../api/client';
import { formatRelative, formatTimestamp, shortId } from '../lib/format';
import type { Tone } from '../lib/labels';

// The glyph repeats what the colour says, so tone never depends on colour.
const TONE_MARK: Record<Tone, string> = { ok: '✓', info: '•', warn: '!', bad: '✕', neutral: '–' };

export function Chip({ tone = 'neutral', children, title }: { tone?: Tone; children: ReactNode; title?: string }) {
  return (
    <span className={`chip chip-${tone}`} title={title}>
      <span className="chip-mark" aria-hidden="true">
        {TONE_MARK[tone]}
      </span>
      {children}
    </span>
  );
}

export function Loading({ what }: { what: string }) {
  return (
    <p className="state state-loading" role="status">
      Loading {what}…
    </p>
  );
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="state state-empty">
      <p className="state-title">{title}</p>
      {children ? <p className="state-detail">{children}</p> : null}
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
    <div className="state state-error" role="alert">
      <p className="state-title">{text.title}</p>
      <p className="state-detail pre">{text.detail}</p>
      {context ? <p className="state-detail">{context}</p> : null}
      {onRetry ? (
        <button type="button" className="btn" onClick={onRetry}>
          Try again
        </button>
      ) : null}
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
  children,
}: {
  query: UseQueryResult<T>;
  what: string;
  children: (data: T) => ReactNode;
}) {
  if (query.data === undefined) {
    if (query.error) return <ErrorPanel error={query.error} onRetry={() => void query.refetch()} />;
    return <Loading what={what} />;
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
  area.style.position = 'fixed';
  area.style.opacity = '0';
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

export function CopyButton({ value, label = 'Copy' }: { value: string; label?: string }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle');
  useEffect(() => {
    if (state === 'idle') return;
    const timer = setTimeout(() => setState('idle'), 2500);
    return () => clearTimeout(timer);
  }, [state]);
  return (
    <button
      type="button"
      className="btn btn-small"
      onClick={() => void copyText(value).then((copied) => setState(copied ? 'copied' : 'failed'))}
    >
      <span aria-live="polite">
        {state === 'copied' ? 'Copied' : state === 'failed' ? 'Copy failed: select it by hand' : label}
      </span>
    </button>
  );
}

/** A long identifier shown short, with the full value on hover and on copy. */
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
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();

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
export function Facts({ children }: { children: ReactNode }) {
  return <dl className="facts">{children}</dl>;
}

export function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="fact">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}
