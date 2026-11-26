//! Errors: what [`crate::Controller::start`] can fail with, and the one
//! shape every HTTP failure takes.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bella_core::store::StoreError;
use bella_core::{ApiError, ErrorCode};
use bella_ledger::LedgerError;
use std::net::SocketAddr;

/// Why the controller could not start.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// Plain HTTP was requested on an address other than loopback without
    /// `allow_insecure`.
    #[error(
        "refusing to serve plain HTTP on {0}: use TLS, a loopback address, or set allow_insecure"
    )]
    InsecureListen(SocketAddr),
    #[error("invalid public URL: {0}")]
    PublicUrl(String),
    #[error("could not prepare the data directory: {0}")]
    DataDir(#[source] std::io::Error),
    #[error("could not open the ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("could not open the object store: {0}")]
    Store(#[from] StoreError),
    #[error("could not load or create the fleet certificate: {0}")]
    Identity(#[from] bella_net::IdentityError),
    #[error("could not build the TLS configuration: {0}")]
    Tls(#[from] bella_net::TlsError),
    #[error("could not create the bootstrap admin token: {0}")]
    AdminToken(String),
    #[error("could not listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("a start-up task failed: {0}")]
    Task(String),
}

/// An HTTP failure. Serialized as the `ApiError` JSON body with the status
/// of its code, as the wire contract requires of every non-2xx response.
#[derive(Debug)]
pub(crate) struct Failure {
    error: ApiError,
    retry_after_secs: Option<u64>,
}

pub(crate) type ApiResult<T> = Result<T, Failure>;

impl Failure {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ApiError::new(code, message),
            retry_after_secs: None,
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, message)
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, message)
    }

    pub(crate) fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what} not found"))
    }

    /// An unexpected failure. The cause goes to the log; the client is told
    /// only that something went wrong, because the cause can name paths and
    /// database details.
    pub(crate) fn internal(cause: impl std::fmt::Display) -> Self {
        tracing::error!(%cause, "internal error");
        Self::new(
            ErrorCode::Internal,
            "internal error; see the controller log",
        )
    }

    pub(crate) fn with_details(mut self, details: serde_json::Value) -> Self {
        self.error = self.error.with_details(details);
        self
    }

    pub(crate) fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    pub(crate) fn code(&self) -> ErrorCode {
        self.error.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.error.message
    }

    #[cfg(test)]
    pub(crate) fn details(&self) -> &serde_json::Value {
        &self.error.details
    }
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self {
            error,
            retry_after_secs: None,
        }
    }
}

impl From<LedgerError> for Failure {
    fn from(error: LedgerError) -> Self {
        if error.code() == ErrorCode::Internal {
            return Failure::internal(error);
        }
        Failure::from(error.to_api_error())
    }
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        match &error {
            StoreError::NotFound(digest) => Failure::not_found(format!("object {digest}")),
            // The wire contract answers a mismatch with 409.
            StoreError::DigestMismatch { .. } => Failure::new(ErrorCode::Conflict, error.to_string()),
            StoreError::TooLarge { .. } => Failure::new(ErrorCode::TooLarge, error.to_string()),
            StoreError::Unavailable(_) => Failure::new(ErrorCode::Unavailable, error.to_string()),
            StoreError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData => {
                Failure::bad_request(format!("the object is not well formed: {io}"))
            }
            StoreError::Io(_) => Failure::internal(error),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self.error).unwrap_or_else(|_| {
            br#"{"code":"internal","message":"internal error"}"#.to_vec()
        });
        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if self.error.code == ErrorCode::Unauthorized {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(secs) = self.retry_after_secs {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::Digest;

    #[test]
    fn store_errors_map_onto_the_wire_statuses() {
        let digest = Digest::of_bytes(b"x");
        let cases = [
            (StoreError::NotFound(digest.clone()), ErrorCode::NotFound),
            (
                StoreError::DigestMismatch {
                    expected: digest.clone(),
                    actual: Digest::of_bytes(b"y"),
                },
                ErrorCode::Conflict,
            ),
            (StoreError::TooLarge { limit: 1 }, ErrorCode::TooLarge),
            (
                StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated",
                )),
                ErrorCode::BadRequest,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(Failure::from(error).code(), code);
        }
    }

    #[test]
    fn internal_ledger_errors_do_not_leak_their_cause() {
        let failure = Failure::from(LedgerError::Corrupt("row 7 of table jobs".into()));
        assert_eq!(failure.code(), ErrorCode::Internal);
        assert!(!failure.error.message.contains("jobs"));
    }
}
