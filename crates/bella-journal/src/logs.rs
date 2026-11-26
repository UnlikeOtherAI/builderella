//! Log stream offsets.
//!
//! The log bytes live in spool files that the worker manages. The journal
//! holds three facts per `(attempt, step, stream)`: how far the spool has
//! been written, how far the controller has confirmed, and whether the
//! stream was cut off at its allowance.

use crate::codec::{from_db, from_text, to_db, to_text};
use crate::error::{JournalError, Result};
use crate::tx::JournalTx;
use bella_core::AttemptId;
use bella_core::protocol::LogStream;
use rusqlite::params;

/// Where one log stream stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LogOffsets {
    /// Bytes written to the local spool.
    pub produced: u64,
    /// Bytes the controller has confirmed as durable. Never above
    /// `produced`.
    pub acked: u64,
    /// The producer hit its allowance: `produced` is final.
    pub truncated: bool,
}

/// One log stream of an attempt and its offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCursor {
    pub step: String,
    pub stream: LogStream,
    pub offsets: LogOffsets,
}

impl JournalTx<'_> {
    fn require_attempt(&self, attempt: &AttemptId) -> Result<()> {
        self.require(attempt).map(|_| ())
    }

    fn read_offsets(
        &self,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
    ) -> Result<LogOffsets> {
        let row = self.one(
            "SELECT produced, acked, truncated FROM log_offsets \
             WHERE attempt_id = ?1 AND step = ?2 AND stream = ?3",
            params![attempt.as_str(), step, to_text(&stream, "log stream")?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )?;
        match row {
            Some((produced, acked, truncated)) => Ok(LogOffsets {
                produced: from_db(produced, "log offset")?,
                acked: from_db(acked, "log offset")?,
                truncated,
            }),
            None => Ok(LogOffsets::default()),
        }
    }

    fn write_offsets(
        &self,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
        offsets: LogOffsets,
    ) -> Result<()> {
        self.exec(
            "INSERT INTO log_offsets (attempt_id, step, stream, produced, acked, truncated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (attempt_id, step, stream) DO UPDATE SET produced = excluded.produced, \
             acked = excluded.acked, truncated = excluded.truncated",
            params![
                attempt.as_str(),
                step,
                to_text(&stream, "log stream")?,
                to_db(offsets.produced, "log offset")?,
                to_db(offsets.acked, "log offset")?,
                offsets.truncated,
            ],
        )?;
        Ok(())
    }

    /// The offsets of one stream; all zero for a stream that has produced
    /// nothing yet.
    pub fn log_offsets(
        &self,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
    ) -> Result<LogOffsets> {
        self.require_attempt(attempt)?;
        self.read_offsets(attempt, step, stream)
    }

    /// Every stream of the attempt that has an offset recorded, ordered by
    /// step and stream. After a reconnect the worker resends each stream
    /// from its `acked` offset.
    pub fn log_cursors(&self, attempt: &AttemptId) -> Result<Vec<LogCursor>> {
        self.require_attempt(attempt)?;
        let rows = self.all(
            "SELECT step, stream, produced, acked, truncated FROM log_offsets \
             WHERE attempt_id = ?1 ORDER BY step, stream",
            params![attempt.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )?;
        rows.into_iter()
            .map(|(step, stream, produced, acked, truncated)| {
                Ok(LogCursor {
                    step,
                    stream: from_text(&stream, "log stream")?,
                    offsets: LogOffsets {
                        produced: from_db(produced, "log offset")?,
                        acked: from_db(acked, "log offset")?,
                        truncated,
                    },
                })
            })
            .collect()
    }

    /// Records that the spool of this stream now holds `produced` bytes.
    ///
    /// The offset only moves forwards; a lower value changes nothing.
    /// Record it before sending the bytes it covers, so the controller can
    /// never confirm bytes the journal does not know of. A truncated stream
    /// cannot grow (`LogTruncated`).
    pub fn record_log_produced(
        &self,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
        produced: u64,
    ) -> Result<LogOffsets> {
        self.require_attempt(attempt)?;
        let mut offsets = self.read_offsets(attempt, step, stream)?;
        if produced <= offsets.produced {
            return Ok(offsets);
        }
        if offsets.truncated {
            return Err(JournalError::LogTruncated {
                attempt: attempt.clone(),
                step: step.to_owned(),
                produced: offsets.produced,
            });
        }
        offsets.produced = produced;
        self.write_offsets(attempt, step, stream, offsets)?;
        Ok(offsets)
    }

    /// Records that the controller holds this stream up to
    /// `through_offset`, so the spool before it may be discarded.
    ///
    /// The mark never moves backwards. An offset beyond what was produced
    /// fails with `AckBeyondProduced` and changes nothing.
    pub fn ack_log(
        &self,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
        through_offset: u64,
    ) -> Result<LogOffsets> {
        self.require_attempt(attempt)?;
        let mut offsets = self.read_offsets(attempt, step, stream)?;
        if through_offset > offsets.produced {
            return Err(JournalError::AckBeyondProduced {
                produced: offsets.produced,
                acknowledged: through_offset,
            });
        }
        if through_offset <= offsets.acked {
            return Ok(offsets);
        }
        offsets.acked = through_offset;
        self.write_offsets(attempt, step, stream, offsets)?;
        Ok(offsets)
    }

    /// Marks the stream as cut off at its current `produced` offset. The
    /// mark is permanent.
    pub fn mark_log_truncated(
        &self,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
    ) -> Result<LogOffsets> {
        self.require_attempt(attempt)?;
        let mut offsets = self.read_offsets(attempt, step, stream)?;
        if !offsets.truncated {
            offsets.truncated = true;
            self.write_offsets(attempt, step, stream, offsets)?;
        }
        Ok(offsets)
    }
}
