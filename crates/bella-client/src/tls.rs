//! The one transport rule of this crate: `https` means the pinned fleet
//! certificate, `http` means no TLS at all, and nothing ever trusts the
//! public web PKI.

use crate::error::ClientError;
use bella_net::{ALPN_HTTP11, ControllerUrl, Fingerprint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::sync::Arc;

/// The TLS configuration for a controller URL, or `None` for plain HTTP.
///
/// An `https` URL without a fingerprint is refused, because the only other
/// way to accept a certificate would be the public web PKI, which a fleet
/// certificate is not part of. A fingerprint given with an `http` URL is
/// refused as well: the caller expects a pinned connection and would get an
/// unauthenticated one.
///
/// The configuration offers `http/1.1` only, which both the REST client and
/// the WebSocket upgrade use.
pub(crate) fn pinned_config(
    url: &ControllerUrl,
    fingerprint: Option<&Fingerprint>,
) -> Result<Option<Arc<rustls::ClientConfig>>, ClientError> {
    match (url.is_tls(), fingerprint) {
        (true, None) => Err(ClientError::Tls {
            pin_mismatch: false,
            message: format!(
                "{url} uses https, which needs the fleet certificate fingerprint; \
                 there is no fallback to public certificate authorities"
            ),
        }),
        (false, Some(_)) => Err(ClientError::invalid(format!(
            "a certificate fingerprint was given for {url}, which is plain http and \
             cannot be pinned"
        ))),
        (false, None) => Ok(None),
        (true, Some(fingerprint)) => {
            let pinned =
                bella_net::pinned_client_config(*fingerprint).map_err(|e| ClientError::Tls {
                    pin_mismatch: false,
                    message: e.to_string(),
                })?;
            let mut config = (*pinned).clone();
            config.alpn_protocols = vec![ALPN_HTTP11.to_vec()];
            Ok(Some(Arc::new(config)))
        }
    }
}

/// A configuration that completes no handshake, for HTTP clients of a
/// plain-`http` controller.
///
/// Such a client never starts TLS: its URL is `http`, redirects are off and
/// `https` requests are impossible to build. Handing the HTTP library this
/// configuration removes its own default (the platform's trust store) as
/// well, so even a bug elsewhere cannot make it trust a public certificate.
pub(crate) fn refusing_config() -> Result<rustls::ClientConfig, ClientError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ClientError::Tls {
            pin_mismatch: false,
            message: e.to_string(),
        })?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(RefuseAll { schemes }))
        .with_no_client_auth();
    Ok(config)
}

/// Refuses every certificate.
#[derive(Debug)]
struct RefuseAll {
    schemes: Vec<SignatureScheme>,
}

impl RefuseAll {
    fn refusal() -> rustls::Error {
        rustls::Error::General("this client is configured for plain http and accepts no TLS".into())
    }
}

impl ServerCertVerifier for RefuseAll {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Err(Self::refusal())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(Self::refusal())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(Self::refusal())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint() -> Fingerprint {
        Fingerprint::of_certificate(b"not a real certificate")
    }

    #[test]
    fn https_needs_a_fingerprint_and_http_must_not_have_one() {
        let https = ControllerUrl::parse("https://127.0.0.1:7420", false).unwrap();
        let http = ControllerUrl::parse("http://127.0.0.1:7420", false).unwrap();

        let error = pinned_config(&https, None).unwrap_err();
        assert!(matches!(
            error,
            ClientError::Tls {
                pin_mismatch: false,
                ..
            }
        ));
        assert!(error.to_string().contains("fingerprint"));

        assert!(matches!(
            pinned_config(&http, Some(&fingerprint())).unwrap_err(),
            ClientError::Invalid { .. }
        ));
        assert!(pinned_config(&http, None).unwrap().is_none());

        let config = pinned_config(&https, Some(&fingerprint()))
            .unwrap()
            .unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn the_plain_http_configuration_accepts_no_certificate() {
        let verifier = RefuseAll {
            schemes: Vec::new(),
        };
        let certificate = CertificateDer::from(vec![1u8, 2, 3]);
        let name = ServerName::try_from("controller.example").unwrap();
        assert!(
            verifier
                .verify_server_cert(
                    &certificate,
                    &[],
                    &name,
                    &[],
                    UnixTime::since_unix_epoch(std::time::Duration::from_secs(1)),
                )
                .is_err()
        );
        assert!(refusing_config().is_ok());
    }
}
