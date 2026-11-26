//! Exit codes and the error every command returns.

use bella_client::ClientError;
use bella_core::{ApiError, ErrorCode};
use serde_json::json;
use std::fmt;

/// The process exit status. Documented in `docs/cli.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Success = 0,
    /// The job finished without succeeding.
    JobFailed = 1,
    /// Bad arguments, or a local failure before or after talking to the
    /// controller.
    Usage = 2,
    /// A wait ran out of time; the job keeps running.
    WaitTimeout = 3,
    /// The controller refused the request.
    Refused = 4,
    /// The controller could not be reached, or did not prove its identity.
    Unreachable = 5,
    /// Waiting was interrupted by Ctrl-C; the job keeps running.
    Interrupted = 130,
}

impl Exit {
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Why a command failed, with the exit status it maps to.
#[derive(Debug)]
pub struct CliError {
    pub exit: Exit,
    pub message: String,
    /// The controller's own error, when it answered.
    pub api: Option<ApiError>,
}

impl CliError {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: message.into(),
            api: None,
        }
    }

    /// A usage or local error (exit 2).
    pub fn local(message: impl Into<String>) -> Self {
        Self::new(Exit::Usage, message)
    }

    /// A failure to talk to the controller, named for what was attempted.
    pub fn client(error: ClientError) -> Self {
        match error {
            ClientError::Api(api) => Self {
                exit: Exit::Refused,
                message: format!("{} ({})", api.message, code_name(api.code)),
                api: Some(api),
            },
            ClientError::Tls {
                pin_mismatch: true, ..
            } => Self::new(Exit::Unreachable, PIN_MISMATCH),
            ClientError::Tls { message, .. } => Self::new(
                Exit::Unreachable,
                format!("TLS with the controller failed: {message}"),
            ),
            error @ (ClientError::Transport { .. } | ClientError::Timeout) => {
                Self::new(Exit::Unreachable, error.to_string())
            }
            ClientError::Http { status, message } if (502..=504).contains(&status) => Self::new(
                Exit::Unreachable,
                format!("the controller answered HTTP {status}: {message}"),
            ),
            error @ ClientError::Http { .. } => Self::new(Exit::Refused, error.to_string()),
            error @ ClientError::Decode { .. } => Self::new(Exit::Unreachable, error.to_string()),
            ClientError::Invalid { message } => Self::new(Exit::Usage, message),
        }
    }

    /// The JSON document printed on standard output for `--json`.
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "error": {
                "message": self.message,
                "code": self.api.as_ref().map(|api| code_name(api.code)),
                "details": self.api.as_ref().map(|api| api.details.clone()),
            },
            "exit_code": self.exit.code(),
        })
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        Self::client(error)
    }
}

pub type CliResult<T> = Result<T, CliError>;

/// What is printed when the controller presents a certificate other than
/// the pinned one.
pub const PIN_MISMATCH: &str = "the controller's certificate fingerprint changed: it is not the \
     certificate these credentials were paired with, so nothing was sent to it. If the \
     controller was reinstalled or its certificate replaced on purpose, pair again with \
     `bella login <url> --fingerprint <new fingerprint> --token-stdin` (the controller prints its \
     fingerprint when it starts). Otherwise the connection may be intercepted: do not re-pair \
     until you know why it changed.";

/// The snake_case name of an error code, as the API writes it.
pub fn code_name(code: ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{code:?}"))
}

/// The snake_case name of any enum the API serializes as a string.
pub fn wire_name<T: serde::Serialize + fmt::Debug>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        Ok(serde_json::Value::Object(map)) if map.len() == 1 => {
            map.keys().next().cloned().unwrap_or_default()
        }
        _ => format!("{value:?}"),
    }
}
