//! The event log: one row per durable state transition, written in the
//! same transaction as the change it describes.

use crate::db::{Ctx, all_rows, exec, from_json, int, ms, opt_id, opt_row, text, time, uint};
use crate::{Ledger, Result};
use bella_core::UnixMillis;
use bella_core::api::{EntityKind, EventRecord};
use bella_core::ids::JobId;
use rusqlite::{Connection, Row, params};
use serde_json::Value;

/// Insert one event row and return its sequence number.
pub(crate) fn append(
    conn: &Connection,
    at: UnixMillis,
    entity: EntityKind,
    entity_id: &str,
    job_id: Option<&JobId>,
    kind: &str,
    data: &Value,
) -> rusqlite::Result<u64> {
    let entity = match entity {
        EntityKind::Fleet => "fleet",
        EntityKind::Machine => "machine",
        EntityKind::Project => "project",
        EntityKind::Snapshot => "snapshot",
        EntityKind::Job => "job",
        EntityKind::Attempt => "attempt",
        EntityKind::Environment => "environment",
        EntityKind::Cloud => "cloud",
    };
    let mut stmt = conn.prepare_cached(
        "INSERT INTO events (at, entity, entity_id, job_id, kind, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    stmt.execute(params![
        ms(at),
        entity,
        entity_id,
        job_id.map(JobId::as_str),
        kind,
        data.to_string()
    ])?;
    // Row ids of an AUTOINCREMENT table are positive.
    Ok(conn.last_insert_rowid().unsigned_abs())
}

/// Write an event inside an operation's transaction. An event about a job
/// also becomes that job's `last_event`.
pub(crate) fn emit(
    c: &Ctx<'_>,
    entity: EntityKind,
    entity_id: &str,
    job_id: Option<&JobId>,
    kind: &str,
    data: Value,
) -> Result<u64> {
    let seq = append(c.conn, c.now, entity, entity_id, job_id, kind, &data)?;
    if let Some(job_id) = job_id {
        exec(
            c.conn,
            "UPDATE jobs SET last_event = ?1, updated_at = ?2 WHERE id = ?3",
            params![int(seq)?, ms(c.now), job_id.as_str()],
        )?;
    }
    Ok(seq)
}

fn record(row: &Row<'_>) -> Result<EventRecord> {
    let entity = match text(row, "entity")?.as_str() {
        "fleet" => EntityKind::Fleet,
        "machine" => EntityKind::Machine,
        "project" => EntityKind::Project,
        "snapshot" => EntityKind::Snapshot,
        "job" => EntityKind::Job,
        "attempt" => EntityKind::Attempt,
        "environment" => EntityKind::Environment,
        "cloud" => EntityKind::Cloud,
        other => {
            return Err(crate::LedgerError::Corrupt(format!(
                "unknown event entity `{other}`"
            )));
        }
    };
    Ok(EventRecord {
        seq: uint(row, "seq")?,
        at: time(row, "at")?,
        entity,
        entity_id: text(row, "entity_id")?,
        kind: text(row, "kind")?,
        job_id: opt_id(row, "job_id")?,
        data: from_json(&text(row, "data")?)?,
    })
}

fn page_size(limit: u32) -> i64 {
    i64::from(limit.clamp(1, 10_000))
}

impl Ledger {
    /// Events with a sequence greater than `seq`, oldest first, at most
    /// `limit` (capped at 10 000). Replaying from the last sequence seen
    /// never misses a transition.
    pub fn events_after(&self, seq: u64, limit: u32) -> Result<Vec<EventRecord>> {
        self.read(|c| {
            all_rows(
                c.conn,
                "SELECT seq, at, entity, entity_id, job_id, kind, data FROM events
                 WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                params![int(seq)?, page_size(limit)],
                record,
            )
        })
    }

    /// Like [`Ledger::events_after`], restricted to events about one job.
    pub fn job_events_after(&self, job: &JobId, seq: u64, limit: u32) -> Result<Vec<EventRecord>> {
        self.read(|c| {
            all_rows(
                c.conn,
                "SELECT seq, at, entity, entity_id, job_id, kind, data FROM events
                 WHERE job_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                params![job.as_str(), int(seq)?, page_size(limit)],
                record,
            )
        })
    }

    /// Sequence of the newest event, or 0 when there is none.
    pub fn last_event_seq(&self) -> Result<u64> {
        self.read(|c| {
            let seq = opt_row(c.conn, "SELECT MAX(seq) FROM events", params![], |row| {
                crate::db::opt_uint(row, 0)
            })?;
            Ok(seq.flatten().unwrap_or(0))
        })
    }

    /// Append an event from inside [`Ledger::with_transaction`], for
    /// components that keep their tables in this database. `at` is normally
    /// [`Ledger::now`].
    pub fn append_event(
        tx: &rusqlite::Transaction<'_>,
        at: UnixMillis,
        entity: EntityKind,
        entity_id: &str,
        job_id: Option<&JobId>,
        kind: &str,
        data: &Value,
    ) -> rusqlite::Result<u64> {
        let seq = append(tx, at, entity, entity_id, job_id, kind, data)?;
        if let Some(job_id) = job_id {
            tx.execute(
                "UPDATE jobs SET last_event = ?1, updated_at = ?2 WHERE id = ?3",
                params![
                    i64::try_from(seq).unwrap_or(i64::MAX),
                    ms(at),
                    job_id.as_str()
                ],
            )?;
        }
        Ok(seq)
    }
}
