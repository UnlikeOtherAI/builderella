//! The error type of every client call.

use bella_core::{ApiError, ErrorCode};
use std::error::Error as StdError;
use std::io;

/// Why a call to the controller failed.
///
/// The variants separate what the controller said ([`ClientError::Api`])
/// from failures to talk to it at all, so a caller can tell "the job does
/// not exist" from "the controller is unreachable".
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ClientError {
    /// The controller answered with a non-2xx status and an [`ApiError`]
    /// body.
    #[error("{0}")]
    Api(#[from] ApiError),

    /// The connection could not be made or broke while in use.
    #[error("cannot reach the controller: {message}")]
    Transport { message: String },

    /// The TLS handshake failed. With `pin_mismatch` set, the server
    /// presented a certificate other than the pinned fleet certificate:
    /// either this is the wrong machine, the connection is intercepted, or
    /// the fleet identity was replaced and this client must be paired
    /// again. Nothing was sent to that server.
    #[error("{message}")]
    Tls { pin_mismatch: bool, message: String },

    /// A response arrived but was not what the API promises: invalid JSON,
    /// a missing header, a malformed event or frame.
    #[error("unexpected response from the controller: {message}")]
    Decode { message: String },

    /// No answer arrived within the configured time.
    #[error("the controller did not answer in time")]
    Timeout,

    /// A non-2xx status whose body is not an [`ApiError`], for example an
    /// answer from something in front of the controller.
    #[error("the controller answered HTTP {status}: {message}")]
    Http { status: u16, message: String },

    /// The call was refused locally, before anything was sent: an `https`
    /// URL without a fingerprint, an empty idempotency key, a blocking call
    /// made from asynchronous code.
    #[error("{message}")]
    Invalid { message: String },
}

impl ClientError {
    pub(crate) fn transport(message: impl Into<String>) -> Self {
        ClientError::Transport {
            message: message.into(),
        }
    }

    pub(crate) fn decode(message: impl Into<String>) -> Self {
        ClientError::Decode {
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        ClientError::Invalid {
            message: message.into(),
        }
    }

    /// The server presented a certificate other than the pinned one.
    pub fn is_pin_mismatch(&self) -> bool {
        matches!(
            self,
            ClientError::Tls {
                pin_mismatch: true,
                ..
            }
        )
    }

    /// The controller's own error, when that is what this is.
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            ClientError::Api(error) => Some(error),
            _ => None,
        }
    }

    /// The controller's error code, when the controller answered.
    pub fn code(&self) -> Option<ErrorCode> {
        self.api().map(|error| error.code)
    }

    /// Whether repeating the request could succeed: a transport failure, a
    /// timeout, a 502/503/504, or [`ErrorCode::Unavailable`]. This says
    /// nothing about whether the request is safe to repeat.
    pub fn is_transient(&self) -> bool {
        match self {
            ClientError::Transport { .. } | ClientError::Timeout => true,
            ClientError::Http { status, .. } => matches!(status, 502..=504),
            ClientError::Api(error) => error.code == ErrorCode::Unavailable,
            ClientError::Tls { .. } | ClientError::Decode { .. } | ClientError::Invalid { .. } => {
                false
            }
        }
    }

    /// Expresses the failure as an [`ApiError`], for interfaces whose error
    /// type is the API's (the MCP tools, `--json` output). An API error is
    /// returned unchanged; anything that kept the controller from answering
    /// becomes [`ErrorCode::Unavailable`].
    pub fn into_api_error(self) -> ApiError {
        let message = self.to_string();
        match self {
            ClientError::Api(error) => error,
            ClientError::Transport { .. } | ClientError::Timeout | ClientError::Tls { .. } => {
                ApiError::new(ErrorCode::Unavailable, message)
            }
            ClientError::Decode { .. } => ApiError::new(ErrorCode::Internal, message),
            ClientError::Invalid { .. } => ApiError::new(ErrorCode::BadRequest, message),
            ClientError::Http { status, .. } => {
                let code = match status {
                    400 => ErrorCode::BadRequest,
                    401 => ErrorCode::Unauthorized,
                    403 => ErrorCode::Forbidden,
                    404 => ErrorCode::NotFound,
                    409 => ErrorCode::Conflict,
                    413 => ErrorCode::TooLarge,
                    502..=504 => ErrorCode::Unavailable,
                    _ => ErrorCode::Internal,
                };
                ApiError::new(code, message)
            }
        }
    }

    /// Classifies a failure of the HTTP client.
    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        if let Some(tls) = find_tls_error(&error) {
            return Self::from_rustls(tls);
        }
        if error.is_timeout() {
            return ClientError::Timeout;
        }
        ClientError::transport(describe(&error))
    }

    /// Classifies an I/O failure of a connection this crate made itself.
    pub(crate) fn from_io(error: &io::Error) -> Self {
        if let Some(tls) = find_tls_error(error) {
            return Self::from_rustls(tls);
        }
        if error.kind() == io::ErrorKind::TimedOut {
            return ClientError::Timeout;
        }
        ClientError::transport(describe(error))
    }

    fn from_rustls(error: &rustls::Error) -> Self {
        match bella_net::pin_mismatch(error) {
            Some(mismatch) => ClientError::Tls {
                pin_mismatch: true,
                message: format!(
                    "the controller's certificate fingerprint changed: {mismatch}. \
                     Nothing was sent. Pair this machine again if the fleet identity \
                     was replaced on purpose"
                ),
            },
            None => ClientError::Tls {
                pin_mismatch: false,
                message: format!("TLS handshake with the controller failed: {error}"),
            },
        }
    }
}

/// How deep an error chain is followed. Chains from the HTTP stack are a
/// handful of links; the bound only guards against a cycle.
const MAX_CHAIN: usize = 16;

/// Finds the rustls error behind a failed connection.
///
/// The handshake error travels inside an `io::Error`, whose `source()`
/// skips the error it wraps, so each link is also checked for a wrapped
/// inner error.
fn find_tls_error<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a rustls::Error> {
    fn walk<'a>(error: &'a (dyn StdError + 'static), depth: usize) -> Option<&'a rustls::Error> {
        if depth > MAX_CHAIN {
            return None;
        }
        let mut current = Some(error);
        let mut steps = 0;
        while let Some(link) = current {
            if let Some(tls) = link.downcast_ref::<rustls::Error>() {
                return Some(tls);
            }
            if let Some(io_error) = link.downcast_ref::<io::Error>()
                && let Some(inner) = io_error.get_ref()
                && let Some(tls) = walk(inner, depth + 1)
            {
                return Some(tls);
            }
            steps += 1;
            if steps > MAX_CHAIN {
                return None;
            }
            current = link.source();
        }
        None
    }
    walk(error, 0)
}

/// The error and its causes on one line.
fn describe(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut current = error.source();
    let mut steps = 0;
    while let Some(cause) = current {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        steps += 1;
        if steps > MAX_CHAIN {
            break;
        }
        current = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors_are_the_ones_worth_repeating() {
        assert!(ClientError::transport("reset").is_transient());
        assert!(ClientError::Timeout.is_transient());
        for status in [502, 503, 504] {
            assert!(
                ClientError::Http {
                    status,
                    message: String::new()
                }
                .is_transient()
            );
        }
        assert!(ClientError::Api(ApiError::new(ErrorCode::Unavailable, "busy")).is_transient());

        assert!(
            !ClientError::Http {
                status: 500,
                message: String::new()
            }
            .is_transient()
        );
        assert!(!ClientError::Api(ApiError::bad_request("no")).is_transient());
        assert!(!ClientError::Api(ApiError::new(ErrorCode::Internal, "bug")).is_transient());
        assert!(!ClientError::decode("garbage").is_transient());
        assert!(!ClientError::invalid("no").is_transient());
        assert!(
            !ClientError::Tls {
                pin_mismatch: true,
                message: String::new()
            }
            .is_transient()
        );
    }

    #[test]
    fn a_tls_error_is_found_inside_an_io_error() {
        let tls = rustls::Error::HandshakeNotComplete;
        let io_error = io::Error::new(io::ErrorKind::InvalidData, tls);
        let outer = io::Error::other(io_error);
        let found = ClientError::from_io(&outer);
        assert!(matches!(
            found,
            ClientError::Tls {
                pin_mismatch: false,
                ..
            }
        ));
        assert!(!found.is_pin_mismatch());

        let plain = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        assert!(matches!(
            ClientError::from_io(&plain),
            ClientError::Transport { .. }
        ));
        let timed_out = io::Error::new(io::ErrorKind::TimedOut, "slow");
        assert_eq!(ClientError::from_io(&timed_out), ClientError::Timeout);
    }

    #[test]
    fn conversion_to_an_api_error_keeps_the_controllers_own_error() {
        let api = ApiError::not_found("job");
        assert_eq!(ClientError::Api(api.clone()).into_api_error(), api);
        assert_eq!(
            ClientError::Timeout.into_api_error().code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            ClientError::transport("down").into_api_error().code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            ClientError::Http {
                status: 404,
                message: "nope".into()
            }
            .into_api_error()
            .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            ClientError::decode("x").into_api_error().code,
            ErrorCode::Internal
        );
    }
}
