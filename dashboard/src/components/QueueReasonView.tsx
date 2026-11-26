import type { QueueReason } from '../api/schemas';
import { reasonLabel, reasonTone } from '../lib/labels';
import { Chip } from './ui';

/**
 * Why a job is waiting: a plain heading for the reason code, the
 * controller's own explanation, and what each machine said, split into
 * blockers that waiting can clear and blockers it cannot.
 */
export function QueueReasonView({ reason }: { reason: QueueReason }) {
  const permanent = reason.blockers.filter((b) => b.permanent);
  const temporary = reason.blockers.filter((b) => !b.permanent);
  const count = reason.blockers.length;

  return (
    <div className="reason">
      <p className="reason-head">
        <Chip tone={reasonTone(reason.code)}>{reasonLabel(reason.code)}</Chip>
      </p>
      <p className="reason-message">{reason.message}</p>
      {count > 0 ? (
        <details className="blockers">
          <summary>
            {count === 1 ? '1 machine checked' : `${count} machines checked`}: {temporary.length}{' '}
            temporary, {permanent.length} permanent
          </summary>
          {temporary.length > 0 ? (
            <>
              <p className="blockers-title">Temporary: waiting can clear these</p>
              <ul>
                {temporary.map((b) => (
                  <li key={`${b.machine}/${b.code}`}>
                    <strong>{b.machine}</strong> <span className="tag">{reasonLabel(b.code)}</span>{' '}
                    {b.message}
                  </li>
                ))}
              </ul>
            </>
          ) : null}
          {permanent.length > 0 ? (
            <>
              <p className="blockers-title">Permanent: waiting will not change these</p>
              <ul>
                {permanent.map((b) => (
                  <li key={`${b.machine}/${b.code}`}>
                    <strong>{b.machine}</strong> <span className="tag">{reasonLabel(b.code)}</span>{' '}
                    {b.message}
                  </li>
                ))}
              </ul>
            </>
          ) : null}
        </details>
      ) : null}
    </div>
  );
}
