//! The event outbox and the record of commands already seen.

use crate::codec::{from_db, from_json, time_from_db, time_to_db, to_db, to_json};
use crate::error::{JournalError, Result};
use crate::tx::JournalTx;
use bella_core::protocol::{AttemptEvent, AttemptEventMsg, AttemptRef, CommandResult};
use bella_core::{AttemptId, CommandId, UnixMillis};
use rusqlite::params;
use std::time::Duration;

impl JournalTx<'_> {
    /// Queues an event for the controller under the attempt's next
    /// sequence number.
    ///
    /// Sequences start at 1 and have no gaps: the number is taken in the
    /// same transaction that stores the event, so a rolled-back transaction
    /// consumes none. Bundle the event with the state change it reports in
    /// one [`with_transaction`](crate::Journal::with_transaction), and the
    /// controller can never hear of a change the journal does not hold, nor
    /// miss one it does. The event stays queued until
    /// [`ack_events`](Self::ack_events) covers it.
    pub fn append_event(
        &self,
        attempt: &AttemptRef,
        event: AttemptEvent,
    ) -> Result<AttemptEventMsg> {
        self.require_boot()?;
        let head = self.fenced(attempt)?;
        let seq = head
            .last_event_seq
            .checked_add(1)
            .ok_or(JournalError::OutOfRange("event sequence"))?;
        let stored_seq = to_db(seq, "event sequence")?;
        let id = attempt.attempt_id.as_str();
        self.exec(
            "INSERT INTO events (attempt_id, seq, at, event) VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                stored_seq,
                time_to_db(self.now),
                to_json(&event, "attempt event")?,
            ],
        )?;
        self.exec(
            "UPDATE attempts SET last_event_seq = ?2 WHERE attempt_id = ?1",
            params![id, stored_seq],
        )?;
        Ok(AttemptEventMsg {
            attempt: attempt.clone(),
            seq,
            at: self.now,
            event,
        })
    }

    /// The attempt's events the controller has not acknowledged, in
    /// sequence order. This is what the worker resends after a reconnect
    /// or a restart.
    pub fn unacked_events(&self, attempt: &AttemptId) -> Result<Vec<AttemptEventMsg>> {
        let head = self.require(attempt)?;
        let rows = self.all(
            "SELECT seq, at, event FROM events WHERE attempt_id = ?1 AND seq > ?2 ORDER BY seq",
            params![
                attempt.as_str(),
                to_db(head.acked_event_seq, "event sequence")?
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.into_iter()
            .map(|(seq, at, event)| {
                Ok(AttemptEventMsg {
                    attempt: AttemptRef {
                        attempt_id: attempt.clone(),
                        generation: head.generation,
                    },
                    seq: from_db(seq, "event sequence")?,
                    at: time_from_db(at, "event timestamp")?,
                    event: from_json(&event, "attempt event")?,
                })
            })
            .collect()
    }

    /// Records that the controller holds the attempt's events up to and
    /// including `through_seq`, and drops them from the outbox.
    ///
    /// An acknowledgement never moves backwards: one at or below the
    /// current mark changes nothing. One beyond the highest sequence
    /// produced fails with `AckBeyondProduced` and changes nothing, because
    /// it acknowledges events that do not exist.
    pub fn ack_events(&self, attempt: &AttemptId, through_seq: u64) -> Result<()> {
        let head = self.require(attempt)?;
        if through_seq > head.last_event_seq {
            return Err(JournalError::AckBeyondProduced {
                produced: head.last_event_seq,
                acknowledged: through_seq,
            });
        }
        if through_seq <= head.acked_event_seq {
            return Ok(());
        }
        let through = to_db(through_seq, "event sequence")?;
        self.exec(
            "UPDATE attempts SET acked_event_seq = ?2 WHERE attempt_id = ?1",
            params![attempt.as_str(), through],
        )?;
        self.exec(
            "DELETE FROM events WHERE attempt_id = ?1 AND seq <= ?2",
            params![attempt.as_str(), through],
        )?;
        Ok(())
    }

    /// The result recorded for a command, or `None` when the command has
    /// not been seen (or was seen longer ago than the retention passed to
    /// [`prune`](Self::prune)).
    pub fn command_result(&self, command: &CommandId) -> Result<Option<CommandResult>> {
        self.one(
            "SELECT result FROM commands WHERE command_id = ?1",
            params![command.as_str()],
            |row| row.get::<_, String>(0),
        )?
        .map(|text| from_json(&text, "command result"))
        .transpose()
    }

    /// Records the result of a command so that a retransmission is
    /// answered from the journal instead of being applied twice.
    ///
    /// The first result stands: returns `true` when this call recorded it
    /// and `false` when the command already had one. Record it in the
    /// transaction that applies the command, so the two cannot disagree
    /// after a crash.
    pub fn record_command(&self, command: &CommandId, result: &CommandResult) -> Result<bool> {
        let inserted = self.exec(
            "INSERT INTO commands (command_id, result, recorded_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (command_id) DO NOTHING",
            params![
                command.as_str(),
                to_json(result, "command result")?,
                time_to_db(self.now),
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Forgets command results recorded more than `keep` before `now` and
    /// returns how many were removed. `keep` has to outlast the longest
    /// time the controller may still retransmit a command.
    pub fn prune(&self, now: UnixMillis, keep: Duration) -> Result<usize> {
        self.exec(
            "DELETE FROM commands WHERE recorded_at < ?1",
            params![time_to_db(now.minus(keep))],
        )
    }
}
