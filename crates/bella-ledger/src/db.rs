//! Transaction context and small helpers for moving values in and out of
//! SQLite.
//!
//! SQLite integers are signed 64-bit. Unsigned values are converted
//! explicitly so that nothing is silently reinterpreted.

use crate::{LedgerError, Result};
use bella_core::UnixMillis;
use bella_core::ids::{Digest, FleetId, IdError, SnapshotId};
use rusqlite::{Connection, Params, Row, RowIndex};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::str::FromStr;

/// What every operation needs while it runs inside its transaction.
pub(crate) struct Ctx<'a> {
    pub conn: &'a Connection,
    /// One timestamp for the whole transaction.
    pub now: UnixMillis,
    pub epoch: u64,
    pub fleet_id: &'a FleetId,
    /// Directory for log files; `None` keeps log bytes in the database.
    pub log_dir: Option<&'a Path>,
}

// ----------------------------------------------------------------- encoding

pub(crate) fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|e| LedgerError::Corrupt(format!("cannot encode a value: {e}")))
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text)
        .map_err(|e| LedgerError::Corrupt(format!("cannot decode a stored value: {e}")))
}

/// The serde name of a field-less enum variant, e.g. `queued`.
pub(crate) fn enum_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Ok(name),
        Ok(other) => Err(LedgerError::Corrupt(format!(
            "expected a plain enum, got {other}"
        ))),
        Err(e) => Err(LedgerError::Corrupt(format!("cannot encode an enum: {e}"))),
    }
}

pub(crate) fn enum_parse<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|e| LedgerError::Corrupt(format!("unknown stored enum value `{text}`: {e}")))
}

pub(crate) fn parse_id<T: FromStr<Err = IdError>>(text: &str) -> Result<T> {
    text.parse()
        .map_err(|e: IdError| LedgerError::Corrupt(e.to_string()))
}

/// An unsigned value as a SQLite integer. Out-of-range values are refused.
pub(crate) fn int(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| LedgerError::Invalid(format!("value {value} is too large to store")))
}

/// A timestamp or limit as a SQLite integer. Values beyond the signed range
/// mean "never" or "unlimited" and are stored as the largest integer.
pub(crate) fn clamp_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(crate) fn ms(at: UnixMillis) -> i64 {
    clamp_int(at.0)
}

// ------------------------------------------------------------------ queries

/// Run a query that returns at most one row.
pub(crate) fn opt_row<T>(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    map: impl FnOnce(&Row<'_>) -> Result<T>,
) -> Result<Option<T>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(row) => Ok(Some(map(row)?)),
        None => Ok(None),
    }
}

/// Run a query and collect every row.
pub(crate) fn all_rows<T>(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    mut map: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(map(row)?);
    }
    Ok(out)
}

/// Run a statement and return the number of rows it changed.
pub(crate) fn exec(conn: &Connection, sql: &str, params: impl Params) -> Result<usize> {
    let mut stmt = conn.prepare_cached(sql)?;
    Ok(stmt.execute(params)?)
}

/// A single unsigned value, e.g. a `COUNT(*)`.
pub(crate) fn count(conn: &Connection, sql: &str, params: impl Params) -> Result<u64> {
    let value = opt_row(conn, sql, params, |row| uint(row, 0))?;
    Ok(value.unwrap_or(0))
}

// ------------------------------------------------------------------ columns

pub(crate) fn text<I: RowIndex>(row: &Row<'_>, col: I) -> Result<String> {
    Ok(row.get::<_, String>(col)?)
}

pub(crate) fn opt_text<I: RowIndex>(row: &Row<'_>, col: I) -> Result<Option<String>> {
    Ok(row.get::<_, Option<String>>(col)?)
}

pub(crate) fn uint<I: RowIndex>(row: &Row<'_>, col: I) -> Result<u64> {
    let value = row.get::<_, i64>(col)?;
    u64::try_from(value)
        .map_err(|_| LedgerError::Corrupt(format!("stored integer {value} is negative")))
}

pub(crate) fn uint32<I: RowIndex>(row: &Row<'_>, col: I) -> Result<u32> {
    let value = row.get::<_, i64>(col)?;
    u32::try_from(value)
        .map_err(|_| LedgerError::Corrupt(format!("stored integer {value} is out of range")))
}

pub(crate) fn opt_uint<I: RowIndex>(row: &Row<'_>, col: I) -> Result<Option<u64>> {
    match row.get::<_, Option<i64>>(col)? {
        None => Ok(None),
        Some(value) => u64::try_from(value)
            .map(Some)
            .map_err(|_| LedgerError::Corrupt(format!("stored integer {value} is negative"))),
    }
}

pub(crate) fn flag<I: RowIndex>(row: &Row<'_>, col: I) -> Result<bool> {
    Ok(row.get::<_, i64>(col)? != 0)
}

pub(crate) fn time<I: RowIndex>(row: &Row<'_>, col: I) -> Result<UnixMillis> {
    uint(row, col).map(UnixMillis)
}

pub(crate) fn opt_time<I: RowIndex>(row: &Row<'_>, col: I) -> Result<Option<UnixMillis>> {
    Ok(opt_uint(row, col)?.map(UnixMillis))
}

pub(crate) fn json<T: DeserializeOwned, I: RowIndex>(row: &Row<'_>, col: I) -> Result<T> {
    from_json(&text(row, col)?)
}

pub(crate) fn opt_json<T: DeserializeOwned, I: RowIndex>(
    row: &Row<'_>,
    col: I,
) -> Result<Option<T>> {
    match opt_text(row, col)? {
        Some(value) => from_json(&value).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn id<T: FromStr<Err = IdError>, I: RowIndex>(row: &Row<'_>, col: I) -> Result<T> {
    parse_id(&text(row, col)?)
}

pub(crate) fn opt_id<T: FromStr<Err = IdError>, I: RowIndex>(
    row: &Row<'_>,
    col: I,
) -> Result<Option<T>> {
    match opt_text(row, col)? {
        Some(value) => parse_id(&value).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn digest<I: RowIndex>(row: &Row<'_>, col: I) -> Result<Digest> {
    Digest::parse(&text(row, col)?).map_err(|e| LedgerError::Corrupt(e.to_string()))
}

pub(crate) fn snapshot_id<I: RowIndex>(row: &Row<'_>, col: I) -> Result<SnapshotId> {
    digest(row, col).map(SnapshotId)
}

pub(crate) fn enumeration<T: DeserializeOwned, I: RowIndex>(row: &Row<'_>, col: I) -> Result<T> {
    enum_parse(&text(row, col)?)
}
