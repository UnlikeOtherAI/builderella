//! Conversions between domain values and what the database stores.
//!
//! Integers are stored as SQLite's signed 64-bit integers, structured
//! values as JSON text, and enums under their wire names.

use crate::error::{JournalError, Result};
use bella_core::UnixMillis;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A counter or offset for storage. Values past `i64::MAX` are refused
/// rather than wrapped.
pub(crate) fn to_db(value: u64, what: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| JournalError::OutOfRange(what))
}

pub(crate) fn from_db(value: i64, what: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| JournalError::corrupt(what, "it is negative"))
}

/// A timestamp for storage. Times past `i64::MAX` milliseconds mean
/// "never" and saturate.
pub(crate) fn time_to_db(time: UnixMillis) -> i64 {
    i64::try_from(time.0).unwrap_or(i64::MAX)
}

pub(crate) fn time_from_db(value: i64, what: &'static str) -> Result<UnixMillis> {
    from_db(value, what).map(UnixMillis)
}

pub(crate) fn to_json<T: Serialize>(value: &T, what: &'static str) -> Result<String> {
    serde_json::to_string(value).map_err(|error| JournalError::corrupt(what, error))
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str, what: &'static str) -> Result<T> {
    serde_json::from_str(text).map_err(|error| JournalError::corrupt(what, error))
}

pub(crate) fn opt_from_json<T: DeserializeOwned>(
    text: Option<String>,
    what: &'static str,
) -> Result<Option<T>> {
    text.map(|text| from_json(&text, what)).transpose()
}

/// The wire name of a unit enum variant, e.g. `start_authorized`.
pub(crate) fn to_text<T: Serialize>(value: &T, what: &'static str) -> Result<String> {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => Ok(name),
        Ok(other) => Err(JournalError::corrupt(what, format!("`{other}` is no name"))),
        Err(error) => Err(JournalError::corrupt(what, error)),
    }
}

pub(crate) fn from_text<T: DeserializeOwned>(text: &str, what: &'static str) -> Result<T> {
    serde_json::from_value(Value::String(text.to_owned()))
        .map_err(|error| JournalError::corrupt(what, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::protocol::{LocalAttemptState, LogStream, StepState};

    #[test]
    fn enums_are_stored_under_their_wire_names() {
        assert_eq!(
            to_text(&LocalAttemptState::StartAuthorized, "state").unwrap(),
            "start_authorized"
        );
        assert_eq!(
            from_text::<LocalAttemptState>("lost_across_boot", "state").unwrap(),
            LocalAttemptState::LostAcrossBoot
        );
        assert_eq!(to_text(&StepState::TimedOut, "step").unwrap(), "timed_out");
        assert_eq!(to_text(&LogStream::Stderr, "stream").unwrap(), "stderr");
        assert!(from_text::<LocalAttemptState>("running!", "state").is_err());
    }

    #[test]
    fn integers_do_not_wrap() {
        assert!(matches!(
            to_db(u64::MAX, "counter"),
            Err(JournalError::OutOfRange("counter"))
        ));
        assert_eq!(time_to_db(UnixMillis(u64::MAX)), i64::MAX);
        assert!(from_db(-1, "counter").is_err());
    }
}
