// Load bars. A summary bar has three parts, in use, reserved and available,
// with the share protected for the machine's owner hatched at the end. The
// breakdown on a machine's page splits "in use" into managed work and
// everything else, and says how strongly the limits are enforced.

import type { Capacity, LoadSummary, SegmentKind } from '../lib/capacity';
import { summarizeLoad } from '../lib/capacity';
import { formatPercent } from '../lib/format';
import { enforcementDetail, enforcementLabel } from '../lib/labels';
import { Help } from './ui';

export function loadText(summary: LoadSummary, format: (value: number) => string): string {
  const parts = [
    summary.inUse === null ? 'use not measured' : `${format(summary.inUse)} in use`,
    `${format(summary.reserved)} reserved and not yet used`,
    `${format(summary.available)} available`,
  ];
  if (summary.protectedUnused > 0) parts.push(`${format(summary.protectedUnused)} protected for the owner`);
  return `${parts.join(', ')}, of ${format(summary.total)}.${
    summary.overBy > 0 ? ` Over-committed by ${format(summary.overBy)}.` : ''
  }`;
}

function Track({ summary, thin = false }: { summary: LoadSummary; thin?: boolean }) {
  const f = summary.fractions;
  return (
    <span className={thin ? 'bar bar-thin' : 'bar'} aria-hidden="true">
      {f.inUse > 0 ? <span className="bar-part bar-use" style={{ flexGrow: f.inUse }} /> : null}
      {f.reserved > 0 ? <span className="bar-part bar-reserved" style={{ flexGrow: f.reserved }} /> : null}
      {f.available > 0 ? <span className="bar-part bar-free" style={{ flexGrow: f.available }} /> : null}
      {f.protected > 0 ? <span className="bar-part bar-protected" style={{ flexGrow: f.protected }} /> : null}
    </span>
  );
}

/** One resource of one machine as a simple bar with "X free of Y". */
export function LoadBar({
  label,
  capacity,
  format,
}: {
  label: string;
  capacity: Capacity;
  format: (value: number) => string;
}) {
  const summary = summarizeLoad(capacity);
  return (
    <span className="load" role="img" aria-label={`${label}: ${loadText(summary, format)}`}>
      <Track summary={summary} />
      <span className="load-text">
        {summary.overBy > 0 ? (
          <strong className="text-bad">over by {format(summary.overBy)}</strong>
        ) : (
          <>
            {format(summary.available)} <span className="muted">free of {format(summary.total)}</span>
          </>
        )}
      </span>
    </span>
  );
}

/** CPU and memory of one machine in one small element, for one-line lists. */
export function MiniLoad({ cpu, memory }: { cpu: Capacity; memory: Capacity }) {
  const c = summarizeLoad(cpu);
  const m = summarizeLoad(memory);
  return (
    <span
      className="mini-load"
      role="img"
      aria-label={`CPU ${formatPercent(c.load)} in use or reserved, memory ${formatPercent(m.load)}`}
    >
      <span className="mini-bars" aria-hidden="true">
        <Track summary={c} thin />
        <Track summary={m} thin />
      </span>
      <span className="mini-text" aria-hidden="true">
        <span>CPU {formatPercent(c.load)}</span>
        <span>Mem {formatPercent(m.load)}</span>
      </span>
    </span>
  );
}

export function LoadLegend() {
  return (
    <ul className="legend" aria-label="What the bar colours mean">
      <li>
        <span className="swatch bar-use" aria-hidden="true" />
        In use
      </li>
      <li>
        <span className="swatch bar-reserved" aria-hidden="true" />
        Reserved
      </li>
      <li>
        <span className="swatch bar-free" aria-hidden="true" />
        Available
      </li>
      <li>
        <span className="swatch bar-protected" aria-hidden="true" />
        Protected for the owner
      </li>
    </ul>
  );
}

// --------------------------------------------------------------- breakdown

const SEGMENT_LABEL: Record<SegmentKind, string> = {
  managed_used: 'Jobs, measured use',
  reserved_unused: 'Reserved for jobs, not yet used',
  external: "Everything else on the machine (the owner's apps, the OS)",
  protected: 'Protected for the owner, unused',
};

const SEGMENT_CLASS: Record<SegmentKind, string> = {
  managed_used: 'bar-use',
  reserved_unused: 'bar-reserved',
  external: 'bar-external',
  protected: 'bar-protected',
};

/** The full split of one resource, with what each part means and how limits are enforced. */
export function LoadBreakdown({
  label,
  capacity,
  format,
  enforcement,
  verified,
}: {
  label: string;
  capacity: Capacity;
  format: (value: number) => string;
  enforcement: string;
  verified: boolean;
}) {
  const summary = `${label}: ${capacity.segments
    .map((s) => `${format(s.value)} ${SEGMENT_LABEL[s.kind].toLowerCase()}`)
    .join(', ')}, ${format(capacity.free)} available, of ${format(capacity.total)}.`;
  return (
    <div className="breakdown">
      <div className="breakdown-head">
        <h3>{label}</h3>
        <span className="muted">{format(capacity.total)} in the machine</span>
      </div>
      <span className="bar bar-tall" role="img" aria-label={summary}>
        {capacity.segments.map((segment) =>
          segment.fraction > 0 ? (
            <span
              key={segment.kind}
              className={`bar-part ${SEGMENT_CLASS[segment.kind]}`}
              style={{ flexGrow: segment.fraction }}
            />
          ) : null,
        )}
        {capacity.freeFraction > 0 ? (
          <span className="bar-part bar-free" style={{ flexGrow: capacity.freeFraction }} />
        ) : null}
        {capacity.totalMarker !== null ? (
          <span className="bar-marker" style={{ left: `${capacity.totalMarker * 100}%` }} />
        ) : null}
      </span>
      <dl className="breakdown-legend">
        {capacity.segments.map((segment) => (
          <div key={segment.kind}>
            <dt>
              <span className={`swatch ${SEGMENT_CLASS[segment.kind]}`} aria-hidden="true" />
              {SEGMENT_LABEL[segment.kind]}
            </dt>
            <dd>
              {format(segment.value)}
              {segment.kind === 'protected' ? (
                <span className="muted"> of {format(capacity.protectedShare)}</span>
              ) : null}
            </dd>
          </div>
        ))}
        <div>
          <dt>
            <span className="swatch bar-free" aria-hidden="true" />
            Available
          </dt>
          <dd>{format(capacity.free)}</dd>
        </div>
      </dl>
      {capacity.overBy > 0 ? (
        <p className="note note-bad">
          Over-committed: these add up to {format(capacity.scale)}, {format(capacity.overBy)} more than the
          machine has. The line on the bar marks the machine's total.
        </p>
      ) : null}
      {!capacity.measured ? (
        <p className="note note-warn">No measurements: only reservations and the protected share are drawn.</p>
      ) : null}
      {capacity.managedOverReservation > 0 ? (
        <p className="note note-warn">
          Jobs are using {format(capacity.managedOverReservation)} more than is reserved for them.
        </p>
      ) : null}
      <p className="breakdown-enforcement">
        Limit: <strong>{enforcementLabel(enforcement)}</strong>{' '}
        <Help label={`What "${enforcementLabel(enforcement)}" means`}>
          {enforcementDetail(enforcement)}.
        </Help>
        {verified ? null : <span className="muted"> Not checked by a self-test on this machine.</span>}
      </p>
    </div>
  );
}
