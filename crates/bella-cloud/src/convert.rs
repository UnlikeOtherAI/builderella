//! Conversions between domain values and SQLite columns.
//!
//! SQLite integers are signed 64-bit. Amounts and timestamps are unsigned in
//! the API, so every crossing is checked instead of cast.

use crate::error::{CloudError, Result};
use bella_core::UnixMillis;
use rusqlite::Row;
use rusqlite::types::Type;
use serde::de::DeserializeOwned;

const MILLIS_PER_HOUR: u128 = 3_600_000;

/// An unsigned value as a SQLite integer.
pub(crate) fn sql_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| CloudError::AmountOverflow)
}

/// A timestamp as a SQLite integer.
pub(crate) fn ms(at: UnixMillis) -> Result<i64> {
    sql_i64(at.0)
}

/// An optional timestamp as a nullable SQLite integer.
pub(crate) fn opt_ms(at: Option<UnixMillis>) -> Result<Option<i64>> {
    at.map(ms).transpose()
}

/// A value as JSON text.
pub(crate) fn json_text<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| CloudError::Corrupt(e.to_string()))
}

/// `hourly_micros` charged for `millis`, rounded up to a whole micro-unit.
/// `None` when the result does not fit.
pub(crate) fn prorate_up(hourly_micros: u64, millis: u64) -> Option<u64> {
    let product = u128::from(hourly_micros) * u128::from(millis);
    u64::try_from(product.div_ceil(MILLIS_PER_HOUR)).ok()
}

/// Checked addition of amounts.
pub(crate) fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(CloudError::AmountOverflow)
}

pub(crate) fn bad_column(idx: usize, message: impl Into<String>) -> rusqlite::Error {
    let message: String = message.into();
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(message),
    )
}

pub(crate) fn col_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value).map_err(|_| bad_column(idx, format!("negative value {value}")))
}

pub(crate) fn col_opt_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = row.get(idx)?;
    value
        .map(|v| u64::try_from(v).map_err(|_| bad_column(idx, format!("negative value {v}"))))
        .transpose()
}

pub(crate) fn col_u32(row: &Row<'_>, idx: usize) -> rusqlite::Result<u32> {
    let value: i64 = row.get(idx)?;
    u32::try_from(value).map_err(|_| bad_column(idx, format!("value {value} out of range")))
}

pub(crate) fn col_ms(row: &Row<'_>, idx: usize) -> rusqlite::Result<UnixMillis> {
    col_u64(row, idx).map(UnixMillis)
}

pub(crate) fn col_opt_ms(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<UnixMillis>> {
    Ok(col_opt_u64(row, idx)?.map(UnixMillis))
}

pub(crate) fn col_parsed<T>(
    row: &Row<'_>,
    idx: usize,
    parse: impl Fn(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    parse(&text).ok_or_else(|| bad_column(idx, format!("unrecognised value `{text}`")))
}

pub(crate) fn col_opt_parsed<T>(
    row: &Row<'_>,
    idx: usize,
    parse: impl Fn(&str) -> Option<T>,
) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = row.get(idx)?;
    match text {
        None => Ok(None),
        Some(text) => parse(&text)
            .map(Some)
            .ok_or_else(|| bad_column(idx, format!("unrecognised value `{text}`"))),
    }
}

pub(crate) fn col_json<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    serde_json::from_str(&text).map_err(|e| bad_column(idx, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proration_rounds_up_and_never_wraps() {
        assert_eq!(prorate_up(3_600_000, 1), Some(1));
        assert_eq!(prorate_up(1, 1), Some(1));
        assert_eq!(prorate_up(0, 86_400_000), Some(0));
        assert_eq!(prorate_up(200_000, 3_600_000), Some(200_000));
        assert_eq!(prorate_up(200_000, 1_800_000), Some(100_000));
        assert_eq!(prorate_up(u64::MAX, u64::MAX), None);
        assert!(matches!(sql_i64(u64::MAX), Err(CloudError::AmountOverflow)));
        assert!(matches!(add(u64::MAX, 1), Err(CloudError::AmountOverflow)));
    }
}
