//! Numbered schema migrations and the per-open bookkeeping.

use crate::db::{all_rows, exec, opt_row, text, uint32};
use crate::events;
use crate::{LedgerError, Result};
use bella_core::UnixMillis;
use bella_core::api::EntityKind;
use bella_core::ids::FleetId;
use rusqlite::{Connection, params};

/// Component name of the ledger's own tables.
pub(crate) const LEDGER: &str = "ledger";

/// The ledger's migrations, in order. A released migration is never edited;
/// changes are appended.
pub(crate) const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

/// Bring `component` up to `migrations.len()`. Must run inside a
/// transaction so that a failing migration leaves the database untouched.
pub(crate) fn migrate(conn: &Connection, component: &str, migrations: &[&str]) -> Result<u32> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_versions (
             component TEXT PRIMARY KEY,
             version   INTEGER NOT NULL
         ) WITHOUT ROWID;",
    )?;
    let found = opt_row(
        conn,
        "SELECT version FROM schema_versions WHERE component = ?1",
        params![component],
        |row| uint32(row, 0),
    )?
    .unwrap_or(0);
    let supported = u32::try_from(migrations.len())
        .map_err(|_| LedgerError::Invalid("too many migrations".into()))?;
    if found > supported {
        return Err(LedgerError::SchemaTooNew {
            component: component.to_string(),
            found,
            supported,
        });
    }
    if found == supported {
        return Ok(supported);
    }
    for sql in &migrations[found as usize..] {
        conn.execute_batch(sql)?;
    }
    exec(
        conn,
        "INSERT INTO schema_versions (component, version) VALUES (?1, ?2)
         ON CONFLICT (component) DO UPDATE SET version = excluded.version",
        params![component, supported],
    )?;
    Ok(supported)
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    opt_row(
        conn,
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| text(row, 0),
    )
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    exec(
        conn,
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Refuse to write through a ledger that no longer owns the database: a
/// later open bumped the epoch, so this process is a superseded controller.
pub(crate) fn ensure_owner(conn: &Connection, epoch: u64) -> Result<()> {
    let current = match meta(conn, "controller_epoch")? {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| LedgerError::Corrupt(format!("controller epoch `{value}`")))?,
        None => 0,
    };
    if current != epoch {
        return Err(LedgerError::StaleEpoch {
            got: epoch,
            current,
        });
    }
    Ok(())
}

/// Take over the database: bump and persist the controller epoch, create
/// the fleet id on first use, and mark every machine disconnected.
pub(crate) fn begin_epoch(conn: &Connection, now: UnixMillis) -> Result<(u64, FleetId)> {
    let previous = match meta(conn, "controller_epoch")? {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| LedgerError::Corrupt(format!("controller epoch `{value}`")))?,
        None => 0,
    };
    let epoch = previous
        .checked_add(1)
        .ok_or_else(|| LedgerError::Corrupt("controller epoch overflow".into()))?;
    set_meta(conn, "controller_epoch", &epoch.to_string())?;

    let fleet_id = match meta(conn, "fleet_id")? {
        Some(value) => crate::db::parse_id::<FleetId>(&value)?,
        None => {
            let id = FleetId::generate();
            set_meta(conn, "fleet_id", id.as_str())?;
            events::append(
                conn,
                now,
                EntityKind::Fleet,
                id.as_str(),
                None,
                "fleet.created",
                &serde_json::Value::Null,
            )?;
            id
        }
    };
    events::append(
        conn,
        now,
        EntityKind::Fleet,
        fleet_id.as_str(),
        None,
        "fleet.controller_started",
        &serde_json::json!({ "epoch": epoch }),
    )?;
    // No connection survives a controller restart. Reservations do.
    let connected = all_rows(
        conn,
        "SELECT id FROM machines WHERE connected = 1 ORDER BY enrolled_at, id",
        params![],
        |row| text(row, 0),
    )?;
    exec(
        conn,
        "UPDATE machines SET connected = 0, admitting = 0 WHERE connected = 1",
        params![],
    )?;
    for machine in connected {
        events::append(
            conn,
            now,
            EntityKind::Machine,
            &machine,
            None,
            "machine.disconnected",
            &serde_json::json!({ "reason": "controller restarted", "epoch": epoch }),
        )?;
    }
    Ok((epoch, fleet_id))
}
