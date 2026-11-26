//! TLS identity, certificate pinning, tokens and pairing strings.
//!
//! Builderella moves source code and credentials between machines on a
//! local network. There is no public certificate authority for such a
//! network and no stable host name to verify, so the trust model is:
//!
//! * The controller generates one self-signed fleet certificate
//!   ([`ServerIdentity`]) and keeps it for its lifetime.
//! * Every worker and client stores the SHA-256 [`Fingerprint`] of that
//!   certificate when it is paired and accepts exactly that certificate
//!   afterwards ([`pinned_client_config`]). The host name is not part of
//!   the decision; possession of the certificate's private key is, because
//!   the handshake signature is still verified.
//! * Plain HTTP is accepted only for loopback, or when the caller passes an
//!   explicit flag ([`ControllerUrl::parse`]).
//!
//! This crate has no "accept any certificate" mode and never falls back to
//! the public web PKI.
//!
//! Bearer tokens ([`Token`]) and pairing codes ([`PairingCode`]) are
//! generated from the operating system's random number generator. The
//! ledger stores only their hashes ([`TokenHash`], [`PairingCodeHash`]).
//!
//! See `docs/architecture.md` §6.11 and decisions D5 and D13.

#![forbid(unsafe_code)]

#[macro_use]
mod digest;

mod credentials;
mod enrollment;
mod fingerprint;
mod identity;
mod pairing;
mod private_file;
mod random;
mod tls;
mod token;
mod url;

pub use credentials::{ClientCredentials, CredentialsError, config_dir};
pub use digest::HexDigestError;
pub use enrollment::{Enrollment, EnrollmentError};
pub use fingerprint::{Fingerprint, FingerprintError};
pub use identity::{CERT_FILE, IdentityError, KEY_FILE, ServerIdentity, VALIDITY};
pub use pairing::{PairingCode, PairingCodeHash, PairingError};
pub use private_file::{
    MAX_PRIVATE_FILE_BYTES, PrivateFile, PrivateFileError, PrivateFileMode, read_private,
    write_private,
};
pub use random::RandomError;
pub use tls::{
    ALPN_H2, ALPN_HTTP11, PinMismatch, PinnedCertVerifier, TlsError, pin_mismatch,
    pinned_client_config, server_config,
};
pub use token::{TOKEN_PREFIX, Token, TokenError, TokenHash, TokenKind};
pub use url::{ControllerUrl, Host, Scheme, UrlError, is_loopback};
