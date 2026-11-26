//! Database schema and numbered migrations.
//!
//! The schema version lives in SQLite's `user_version`. Each migration runs
//! once, in order, inside the transaction that also bumps the version, so a
//! crash leaves either the old schema or the new one.

use crate::error::{JournalError, Result};
use rusqlite::{Connection, TransactionBehavior};

/// The schema version this build reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Marks the file as a worker journal (`BJRN`), so another SQLite database
/// is never migrated by mistake.
const APPLICATION_ID: i64 = 0x424A_524E;

/// `(version, statements)`, numbered from 1 without gaps.
const MIGRATIONS: &[(u32, &str)] = &[(1, V1)];

const V1: &str = r#"
CREATE TABLE identity (
    id                    INTEGER PRIMARY KEY CHECK (id = 1),
    enrollment            TEXT,
    policy                TEXT,
    connection_generation INTEGER NOT NULL DEFAULT 0,
    known_epoch           INTEGER NOT NULL DEFAULT 0,
    local_control         TEXT
);
INSERT INTO identity (id) VALUES (1);

CREATE TABLE attempts (
    attempt_id          TEXT PRIMARY KEY,
    generation          INTEGER NOT NULL,
    job_id              TEXT NOT NULL,
    boot_id             TEXT NOT NULL,
    active_boot_id      TEXT NOT NULL,
    offer               TEXT NOT NULL,
    reservation         TEXT NOT NULL,
    expires_at          INTEGER NOT NULL,
    state               TEXT NOT NULL,
    authority           TEXT,
    scope               TEXT,
    stale_scope         TEXT,
    workspace           TEXT,
    outcome             TEXT,
    usage               TEXT,
    cancel_requested_at INTEGER,
    cancel_reason       TEXT,
    last_event_seq      INTEGER NOT NULL DEFAULT 0,
    acked_event_seq     INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    authorized_at       INTEGER,
    released_at         INTEGER
);
CREATE INDEX attempts_by_job ON attempts (job_id, generation);

CREATE TABLE attempt_steps (
    attempt_id TEXT NOT NULL REFERENCES attempts (attempt_id) ON DELETE CASCADE,
    step       TEXT NOT NULL,
    state      TEXT NOT NULL,
    exit_code  INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (attempt_id, step)
) WITHOUT ROWID;

CREATE TABLE events (
    attempt_id TEXT NOT NULL REFERENCES attempts (attempt_id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    at         INTEGER NOT NULL,
    event      TEXT NOT NULL,
    PRIMARY KEY (attempt_id, seq)
) WITHOUT ROWID;

CREATE TABLE commands (
    command_id  TEXT PRIMARY KEY,
    result      TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX commands_by_age ON commands (recorded_at);

CREATE TABLE log_offsets (
    attempt_id TEXT NOT NULL REFERENCES attempts (attempt_id) ON DELETE CASCADE,
    step       TEXT NOT NULL,
    stream     TEXT NOT NULL,
    produced   INTEGER NOT NULL DEFAULT 0,
    acked      INTEGER NOT NULL DEFAULT 0,
    truncated  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (attempt_id, step, stream)
) WITHOUT ROWID;
"#;

/// What to do when the stored schema is older than this build's.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upgrade {
    /// Run the missing migrations.
    Apply,
    /// Fail with [`JournalError::SchemaTooOld`].
    Refuse,
}

/// Checks that the database is a journal this build may open and returns
/// its schema version. Reads only.
pub(crate) fn inspect(conn: &Connection, upgrade: Upgrade) -> Result<u32> {
    let application_id: i64 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(JournalError::sqlite)?;
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(JournalError::sqlite)?;
    if application_id == 0 && found == 0 {
        let objects: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .map_err(JournalError::sqlite)?;
        if objects != 0 {
            return Err(JournalError::NotAJournal);
        }
    } else if application_id != APPLICATION_ID {
        return Err(JournalError::NotAJournal);
    }
    let found = u32::try_from(found).map_err(|_| JournalError::NotAJournal)?;
    if found > SCHEMA_VERSION {
        return Err(JournalError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found < SCHEMA_VERSION && upgrade == Upgrade::Refuse {
        return Err(JournalError::SchemaTooOld {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(found)
}

/// Brings the database to [`SCHEMA_VERSION`], or fails without changing it.
pub(crate) fn migrate(conn: &mut Connection, upgrade: Upgrade) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(JournalError::sqlite)?;
    let found = inspect(&tx, upgrade)?;
    if found == SCHEMA_VERSION {
        return Ok(());
    }
    for (version, statements) in MIGRATIONS {
        if *version > found {
            tx.execute_batch(statements).map_err(JournalError::sqlite)?;
            tx.execute_batch(&format!("PRAGMA user_version = {version}"))
                .map_err(JournalError::sqlite)?;
        }
    }
    tx.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID}"))
        .map_err(JournalError::sqlite)?;
    tx.commit().map_err(JournalError::sqlite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_numbered_without_gaps_up_to_the_schema_version() {
        for (index, (version, _)) in MIGRATIONS.iter().enumerate() {
            assert_eq!(*version as usize, index + 1);
        }
        assert_eq!(MIGRATIONS.len(), SCHEMA_VERSION as usize);
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, Upgrade::Apply).unwrap();
        migrate(&mut conn, Upgrade::Apply).unwrap();
        migrate(&mut conn, Upgrade::Refuse).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::from(SCHEMA_VERSION));
    }

    #[test]
    fn an_empty_database_is_not_upgraded_by_a_handle_that_may_not_migrate() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            migrate(&mut conn, Upgrade::Refuse),
            Err(JournalError::SchemaTooOld { found: 0, .. })
        ));
    }
}
