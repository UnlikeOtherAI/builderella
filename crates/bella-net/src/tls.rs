//! rustls configurations: a server for the fleet certificate and a client
//! that trusts exactly one certificate, by fingerprint.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, OtherError, ServerConfig,
    SignatureScheme,
};

use crate::fingerprint::Fingerprint;
use crate::identity::{ServerIdentity, provider};

/// ALPN identifier for HTTP/2.
pub const ALPN_H2: &[u8] = b"h2";
/// ALPN identifier for HTTP/1.1, which the worker WebSocket upgrades from.
pub const ALPN_HTTP11: &[u8] = b"http/1.1";

/// A rustls configuration could not be built.
#[derive(Debug, thiserror::Error)]
#[error("could not build the TLS configuration: {0}")]
pub struct TlsError(#[from] rustls::Error);

/// The server presented a certificate other than the pinned one.
///
/// Carried inside the `rustls::Error` a failed handshake returns; use
/// [`pin_mismatch`] to get it back. Either the client is talking to the
/// wrong machine, someone is intercepting the connection, or the fleet
/// identity was replaced and this client has to be paired again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the server's certificate does not match the pinned fleet fingerprint \
     (pinned {expected}, presented {presented})"
)]
pub struct PinMismatch {
    /// The fingerprint this client was paired with.
    pub expected: Fingerprint,
    /// The fingerprint of the certificate the server sent.
    pub presented: Fingerprint,
}

/// Extracts the [`PinMismatch`] from a handshake error, if that is why the
/// handshake failed.
pub fn pin_mismatch(error: &rustls::Error) -> Option<&PinMismatch> {
    match error {
        rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(inner))) => {
            inner.downcast_ref::<PinMismatch>()
        }
        _ => None,
    }
}

/// Server configuration presenting the fleet certificate.
///
/// TLS 1.3 and 1.2 with the `ring` provider's default suites, no client
/// certificates (clients authenticate with bearer tokens), and ALPN
/// offering `h2` then `http/1.1`.
pub fn server_config(identity: &ServerIdentity) -> Result<Arc<ServerConfig>, TlsError> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert_der().clone()], identity.private_key())?;
    config.alpn_protocols = vec![ALPN_H2.to_vec(), ALPN_HTTP11.to_vec()];
    Ok(Arc::new(config))
}

/// Client configuration that accepts one certificate: the one whose
/// SHA-256 fingerprint is `fingerprint`.
///
/// See [`PinnedCertVerifier`] for exactly what is and is not checked. The
/// configuration offers no ALPN protocols; a caller that needs them clones
/// the configuration and sets `alpn_protocols`.
pub fn pinned_client_config(fingerprint: Fingerprint) -> Result<Arc<ClientConfig>, TlsError> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(fingerprint)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Certificate verifier for a fleet with one self-signed certificate and
/// no certificate authority.
///
/// **Checked**
///
/// * The SHA-256 of the server's end-entity certificate equals the pinned
///   fingerprint, compared in constant time. Any other certificate is
///   refused with [`PinMismatch`], whatever it claims and whoever signed
///   it.
/// * The handshake signature, with the public key of that certificate and
///   the provider's algorithms. A peer that replays the fleet certificate
///   without holding its private key is refused.
///
/// **Not checked, by design**
///
/// * The server name. Machines are reached by IP address or mDNS name,
///   both of which change; the pin identifies the fleet instead.
/// * The validity period. The fingerprint already fixes the exact
///   certificate, so its dates add no assurance, and honouring them would
///   stop every machine in the fleet on one day. Rotating the identity is
///   a re-pairing, not an expiry.
/// * Chains, revocation and the public web PKI. There is no fallback to
///   any of them.
pub struct PinnedCertVerifier {
    pinned: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedCertVerifier {
    /// A verifier that accepts only the certificate with this fingerprint.
    pub fn new(pinned: Fingerprint) -> Self {
        Self {
            pinned,
            algorithms: provider().signature_verification_algorithms,
        }
    }

    /// The fingerprint this verifier accepts.
    pub fn fingerprint(&self) -> Fingerprint {
        self.pinned
    }

    fn check_pin(&self, certificate: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let presented = Fingerprint::of_certificate(certificate.as_ref());
        if presented == self.pinned {
            return Ok(());
        }
        Err(rustls::Error::InvalidCertificate(CertificateError::Other(
            OtherError(Arc::new(PinMismatch {
                expected: self.pinned,
                presented,
            })),
        )))
    }
}

impl fmt::Debug for PinnedCertVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedCertVerifier")
            .field("pinned", &self.pinned)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check_pin(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // rustls passes the certificate it already had verified; checking
        // again keeps this method safe on its own.
        self.check_pin(cert)?;
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.check_pin(cert)?;
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn identity() -> (tempfile::TempDir, ServerIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let identity =
            ServerIdentity::load_or_generate(dir.path(), &["build-box.local".to_owned()]).unwrap();
        (dir, identity)
    }

    fn verify(
        verifier: &PinnedCertVerifier,
        certificate: &CertificateDer<'_>,
        name: &str,
    ) -> Result<ServerCertVerified, rustls::Error> {
        verifier.verify_server_cert(
            certificate,
            &[],
            &ServerName::try_from(name.to_owned()).unwrap(),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(1_767_225_600)),
        )
    }

    #[test]
    fn only_the_pinned_certificate_is_accepted() {
        let (_dir_a, a) = identity();
        let (_dir_b, b) = identity();
        let verifier = PinnedCertVerifier::new(a.fingerprint());
        assert_eq!(verifier.fingerprint(), a.fingerprint());

        assert!(verify(&verifier, a.cert_der(), "build-box.local").is_ok());

        let error = verify(&verifier, b.cert_der(), "build-box.local").unwrap_err();
        let mismatch = pin_mismatch(&error).expect("a pin mismatch");
        assert_eq!(mismatch.expected, a.fingerprint());
        assert_eq!(mismatch.presented, b.fingerprint());
        assert!(error.to_string().contains(&a.fingerprint().to_hex()));
    }

    #[test]
    fn the_server_name_and_time_do_not_matter() {
        let (_dir, a) = identity();
        let verifier = PinnedCertVerifier::new(a.fingerprint());
        for name in ["build-box.local", "unrelated.example", "127.0.0.1", "::1"] {
            assert!(verify(&verifier, a.cert_der(), name).is_ok(), "{name}");
        }
        // Long after the certificate's notAfter date.
        let far_future = UnixTime::since_unix_epoch(Duration::from_secs(4_102_444_800 * 2));
        assert!(
            verifier
                .verify_server_cert(
                    a.cert_der(),
                    &[],
                    &ServerName::try_from("x.example").unwrap(),
                    &[],
                    far_future
                )
                .is_ok()
        );
    }

    #[test]
    fn anything_that_is_not_the_exact_certificate_is_refused() {
        let (_dir, a) = identity();
        let verifier = PinnedCertVerifier::new(a.fingerprint());

        let mut altered = a.cert_der().as_ref().to_vec();
        let last = altered.len() - 1;
        altered[last] ^= 1;
        let altered = CertificateDer::from(altered);
        assert!(pin_mismatch(&verify(&verifier, &altered, "x.example").unwrap_err()).is_some());

        let empty = CertificateDer::from(Vec::new());
        assert!(pin_mismatch(&verify(&verifier, &empty, "x.example").unwrap_err()).is_some());

        // The pinned certificate hidden among intermediates does not count.
        let (_dir_b, b) = identity();
        let error = verifier
            .verify_server_cert(
                b.cert_der(),
                std::slice::from_ref(a.cert_der()),
                &ServerName::try_from("x.example").unwrap(),
                &[],
                UnixTime::since_unix_epoch(Duration::from_secs(1_767_225_600)),
            )
            .unwrap_err();
        assert!(pin_mismatch(&error).is_some());
    }

    #[test]
    fn signature_schemes_come_from_the_ring_provider() {
        let (_dir, a) = identity();
        let verifier = PinnedCertVerifier::new(a.fingerprint());
        let schemes = verifier.supported_verify_schemes();
        assert_eq!(
            schemes,
            provider()
                .signature_verification_algorithms
                .supported_schemes()
        );
        assert!(schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256));
        assert!(!schemes.is_empty());
    }

    #[test]
    fn server_config_offers_h2_then_http11() {
        let (_dir, a) = identity();
        let config = server_config(&a).unwrap();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn configurations_need_no_process_wide_crypto_provider() {
        // Nothing in this crate or its tests installs a default provider,
        // so building both configurations proves they carry their own.
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        let (_dir, a) = identity();
        server_config(&a).unwrap();
        pinned_client_config(a.fingerprint()).unwrap();
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }

    #[test]
    fn other_errors_are_not_reported_as_pin_mismatches() {
        assert!(pin_mismatch(&rustls::Error::HandshakeNotComplete).is_none());
        assert!(
            pin_mismatch(&rustls::Error::InvalidCertificate(
                CertificateError::BadSignature
            ))
            .is_none()
        );
    }
}
