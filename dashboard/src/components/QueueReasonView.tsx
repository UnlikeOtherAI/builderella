import type { QueueReason } from '../api/schemas';
import { reasonLabel } from '../lib/labels';
import { queueLine } from '../lib/status';
import { Chip } from './ui';

/**
 * Why a job is waiting: one line saying whether waiting will help, the
 * controller's own explanation, and what each machine said, split into
 * blockers that waiting can clear and blockers it cannot.
 */
export function QueueReasonView({ reason }: { reason: QueueReason }) {
  const line = queueLine(reason);
  const permanent = reason.blockers.filter((b) => b.permanent);
  const temporary = reason.blockers.filter((b) => !b.permanent);
  return (
    <div className="reason">
      <p className={`reason-line reason-head tone-${line.tone}`}>
        <span className="dot" aria-hidden="true" />
        <strong>{line.headline}</strong>
      </p>
      <p className="reason-message">{reason.message}</p>
      {reason.blockers.length > 0 ? (
        <div className="blockers">
          {temporary.length > 0 ? (
            <BlockerList title="Waiting can clear these" blockers={temporary} />
          ) : null}
          {permanent.length > 0 ? (
            <BlockerList title="Waiting will not change these" blockers={permanent} />
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function BlockerList({ title, blockers }: { title: string; blockers: QueueReason['blockers'] }) {
  return (
    <div>
      <h4 className="blockers-title">
        {title} <span className="muted">({blockers.length})</span>
      </h4>
      <ul className="blocker-list" aria-label={title}>
        {blockers.map((b) => (
          <li key={`${b.machine}/${b.code}`}>
            <strong>{b.machine}</strong>
            <Chip tone={b.permanent ? 'neutral' : 'info'}>{reasonLabel(b.code)}</Chip>
            <span className="muted">{b.message}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
