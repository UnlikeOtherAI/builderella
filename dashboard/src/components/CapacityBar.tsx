import type { Capacity, SegmentKind } from '../lib/capacity';
import { enforcementDetail, enforcementLabel } from '../lib/labels';

const SEGMENT_LABEL: Record<SegmentKind, string> = {
  managed_used: 'Managed work, measured',
  reserved_unused: 'Reserved for managed work, not in use',
  external: "External activity (the owner's own apps)",
  protected: 'Protected for the foreground, unused',
};

const SEGMENT_SHORT: Record<SegmentKind, string> = {
  managed_used: 'managed work in use',
  reserved_unused: 'reserved but not in use',
  external: 'external activity',
  protected: 'protected for the foreground and unused',
};

/**
 * One stacked bar for one resource of one machine, with a legend that gives
 * every segment in words and numbers and the enforcement strength beside it.
 * Segments differ by pattern as well as colour.
 */
export function CapacityBar({
  label,
  capacity,
  format,
  enforcement,
  verified,
}: {
  label: string;
  capacity: Capacity;
  format: (value: number) => string;
  /** How strongly this resource's limit is enforced on the machine. */
  enforcement: string;
  /** Whether a self-test exercised the limit. */
  verified: boolean;
}) {
  const parts = capacity.segments.map((s) => `${format(s.value)} ${SEGMENT_SHORT[s.kind]}`);
  const summary =
    `${label}: ${parts.join(', ')}, ${format(capacity.free)} free, of ${format(capacity.total)}.` +
    (capacity.overBy > 0 ? ` Over-committed by ${format(capacity.overBy)}.` : '');

  return (
    <section className="cap" aria-label={`${label} capacity`}>
      <div className="cap-head">
        <h3>{label}</h3>
        <span className="cap-total">{format(capacity.total)} in the machine</span>
      </div>

      <div className="cap-bar" role="img" aria-label={summary}>
        {capacity.segments.map((segment) =>
          segment.fraction > 0 ? (
            <span
              key={segment.kind}
              className={`seg seg-${segment.kind}`}
              style={{ flexGrow: segment.fraction }}
            />
          ) : null,
        )}
        {capacity.freeFraction > 0 ? (
          <span className="seg seg-free" style={{ flexGrow: capacity.freeFraction }} />
        ) : null}
        {capacity.totalMarker !== null ? (
          <span className="cap-marker" style={{ left: `${capacity.totalMarker * 100}%` }} />
        ) : null}
      </div>

      {capacity.overBy > 0 ? (
        <p className="note note-bad">
          Over-committed: these add up to {format(capacity.scale)}, which is {format(capacity.overBy)}{' '}
          more than the machine has. The line on the bar marks the machine's total.
        </p>
      ) : null}

      <dl className="cap-legend">
        {capacity.segments.map((segment) => (
          <div key={segment.kind}>
            <dt>
              <span className={`swatch seg-${segment.kind}`} aria-hidden="true" />
              {SEGMENT_LABEL[segment.kind]}
            </dt>
            <dd>
              {format(segment.value)}
              {segment.kind === 'protected' ? (
                <span className="muted"> of {format(capacity.protectedShare)} protected</span>
              ) : null}
            </dd>
          </div>
        ))}
        <div>
          <dt>
            <span className="swatch seg-free" aria-hidden="true" />
            Free
          </dt>
          <dd>{format(capacity.free)}</dd>
        </div>
      </dl>

      {!capacity.measured ? (
        <p className="note note-warn">
          No telemetry: use is not measured here, so only reservations and the protected share are
          drawn.
        </p>
      ) : null}
      {capacity.managedOverReservation > 0 ? (
        <p className="note note-warn">
          Managed work is using {format(capacity.managedOverReservation)} more than is reserved for it.
        </p>
      ) : null}

      <p className={`cap-enforcement enforcement-${enforcement === 'hard' ? 'hard' : 'soft'}`}>
        {label} limit enforcement: <strong>{enforcementLabel(enforcement)}</strong> (
        {enforcementDetail(enforcement)}){verified ? '.' : '. Not exercised by a self-test on this machine.'}
      </p>
    </section>
  );
}
