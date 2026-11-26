//! Tables this crate keeps in the controller's database.
//!
//! The crate tracks its own schema version in `cloud_schema`, so the ledger
//! can apply these migrations as one component next to its own.

use crate::error::{CloudError, Result};
use rusqlite::{Connection, OptionalExtension};

/// Schema migrations in order. Entry `n` takes the schema from version `n`
/// to `n + 1`. Published entries are never edited; changes are appended.
pub const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE cloud_allowances (
    scope_kind          TEXT NOT NULL CHECK (scope_kind IN ('fleet', 'project')),
    scope_id            TEXT NOT NULL,
    period              TEXT NOT NULL CHECK (period IN ('daily', 'monthly')),
    currency            TEXT NOT NULL,
    limit_micros        INTEGER NOT NULL CHECK (limit_micros >= 0),
    max_hourly_micros   INTEGER NOT NULL CHECK (max_hourly_micros >= 0),
    max_active          INTEGER NOT NULL CHECK (max_active >= 0),
    max_pending         INTEGER NOT NULL CHECK (max_pending >= 0),
    allowed_presets     TEXT NOT NULL,
    allowed_regions     TEXT NOT NULL,
    allowed_purchase    TEXT NOT NULL,
    max_lifetime_secs   INTEGER NOT NULL CHECK (max_lifetime_secs >= 0),
    max_attempts        INTEGER NOT NULL CHECK (max_attempts >= 0),
    set_by              TEXT NOT NULL,
    set_at              INTEGER NOT NULL,
    PRIMARY KEY (scope_kind, scope_id, period)
);

CREATE TABLE cloud_intents (
    id                  TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    fleet               TEXT NOT NULL,
    project             TEXT NOT NULL,
    job                 TEXT,
    covers              TEXT NOT NULL,
    chain               TEXT NOT NULL,
    retry_of            TEXT REFERENCES cloud_intents(id),
    preset              TEXT NOT NULL,
    region              TEXT NOT NULL,
    purchase            TEXT NOT NULL,
    image               TEXT NOT NULL,
    lifetime_secs       INTEGER NOT NULL CHECK (lifetime_secs > 0),
    expires_at          INTEGER NOT NULL,
    requested_by        TEXT NOT NULL,
    mode                TEXT NOT NULL CHECK (mode IN ('manual', 'automatic')),
    priority            TEXT NOT NULL,
    escalated           INTEGER NOT NULL,
    state               TEXT NOT NULL,
    state_since         INTEGER NOT NULL,
    reason              TEXT,
    failure             TEXT,
    bootstrap_hash      TEXT UNIQUE,
    bootstrap_used_at   INTEGER,
    create_attempts     INTEGER NOT NULL DEFAULT 0,
    last_create_at      INTEGER,
    create_uncertain    INTEGER NOT NULL DEFAULT 0,
    resources_seen      INTEGER NOT NULL DEFAULT 0,
    cancel_requested_at INTEGER,
    settled_at          INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX cloud_intents_open ON cloud_intents (provider, settled_at);
CREATE INDEX cloud_intents_project ON cloud_intents (project, state);
CREATE INDEX cloud_intents_chain ON cloud_intents (chain);

CREATE TABLE cloud_resources (
    provider            TEXT NOT NULL,
    resource_id         TEXT NOT NULL,
    intent              TEXT NOT NULL REFERENCES cloud_intents(id),
    kind                TEXT NOT NULL,
    state               TEXT NOT NULL,
    hourly_micros       INTEGER NOT NULL CHECK (hourly_micros >= 0),
    created_at          INTEGER NOT NULL,
    first_seen_at       INTEGER NOT NULL,
    last_seen_at        INTEGER NOT NULL,
    listed              INTEGER NOT NULL DEFAULT 0,
    expires_at          INTEGER,
    releasable_at       INTEGER,
    delete_requested_at INTEGER,
    gone_at             INTEGER,
    PRIMARY KEY (provider, resource_id)
);
CREATE INDEX cloud_resources_intent ON cloud_resources (intent, gone_at);

CREATE TABLE cloud_money (
    intent                    TEXT PRIMARY KEY REFERENCES cloud_intents(id),
    currency                  TEXT NOT NULL,
    amount_micros             INTEGER NOT NULL CHECK (amount_micros >= 0),
    startup_micros            INTEGER NOT NULL,
    lifetime_micros           INTEGER NOT NULL,
    minimum_micros            INTEGER NOT NULL,
    retries_micros            INTEGER NOT NULL,
    hourly_micros             INTEGER NOT NULL,
    minimum_commitment_micros INTEGER NOT NULL,
    attempts_allowed          INTEGER NOT NULL,
    quote_priced_at           INTEGER NOT NULL,
    quote_assumptions         TEXT NOT NULL,
    created_at                INTEGER NOT NULL,
    released_at               INTEGER,
    estimate_micros           INTEGER,
    estimate_at               INTEGER,
    estimate_assumptions      TEXT,
    assumed_micros            INTEGER NOT NULL DEFAULT 0,
    assumed_note              TEXT,
    actual_micros             INTEGER,
    actual_reported_at        INTEGER,
    actual_final              INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX cloud_money_open ON cloud_money (released_at);

CREATE TABLE cloud_orphans (
    provider            TEXT NOT NULL,
    resource_id         TEXT NOT NULL,
    kind                TEXT NOT NULL,
    hourly_micros       INTEGER NOT NULL,
    intent_label        TEXT,
    ownership_proven    INTEGER NOT NULL,
    note                TEXT NOT NULL,
    first_seen_at       INTEGER NOT NULL,
    last_seen_at        INTEGER NOT NULL,
    deleted_at          INTEGER,
    PRIMARY KEY (provider, resource_id)
);

CREATE TABLE cloud_events (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    intent              TEXT,
    at                  INTEGER NOT NULL,
    kind                TEXT NOT NULL,
    from_state          TEXT,
    to_state            TEXT,
    detail              TEXT NOT NULL
);
CREATE INDEX cloud_events_intent ON cloud_events (intent, seq);
"#];

/// Brings the cloud tables on `conn` up to date.
///
/// Safe to call on every start and inside or outside a caller's transaction:
/// the work happens in a savepoint, so a failed migration changes nothing.
/// Refuses a database written by a newer build.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("SAVEPOINT cloud_migrate")?;
    match apply(conn) {
        Ok(()) => {
            conn.execute_batch("RELEASE cloud_migrate")?;
            Ok(())
        }
        Err(error) => {
            // The original error is the useful one; a failed rollback of the
            // savepoint leaves the caller's transaction to roll back instead.
            let _ = conn.execute_batch("ROLLBACK TO cloud_migrate; RELEASE cloud_migrate");
            Err(error)
        }
    }
}

fn apply(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cloud_schema (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             version INTEGER NOT NULL
         )",
    )?;
    let found = schema_version(conn)?;
    let supported = MIGRATIONS.len();
    let applied = usize::try_from(found)
        .ok()
        .filter(|applied| *applied <= supported)
        .ok_or(CloudError::SchemaTooNew { found, supported })?;
    for migration in &MIGRATIONS[applied..] {
        conn.execute_batch(migration)?;
    }
    let version = i64::try_from(supported).map_err(|_| CloudError::AmountOverflow)?;
    conn.execute(
        "INSERT INTO cloud_schema (id, version) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET version = excluded.version",
        [version],
    )?;
    Ok(())
}

/// The cloud schema version recorded on `conn`; zero before the first migration.
pub fn schema_version(conn: &Connection) -> Result<i64> {
    let version: Option<i64> = conn
        .query_row("SELECT version FROM cloud_schema WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(version.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_repeatable_and_tracks_its_own_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len() as i64);
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name LIKE 'cloud_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 7);
    }

    #[test]
    fn migrate_works_inside_a_callers_transaction_and_rolls_back_with_it() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'cloud_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        migrate(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute("UPDATE cloud_schema SET version = 99", [])
            .unwrap();
        assert!(matches!(
            migrate(&conn),
            Err(CloudError::SchemaTooNew { found: 99, .. })
        ));
    }
}
