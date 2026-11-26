//! The enrollment string: everything a machine needs to join a fleet, in
//! one line that can be pasted into `bella worker enroll`.
//!
//! ```text
//! bella://<host>:<port>/#<fingerprint, 43 base64url characters>.<pairing code>
//! bella+http://<host>:<port>/#<pairing code>
//! ```
//!
//! The first form means HTTPS with the fleet certificate pinned to the
//! given fingerprint. The second is the explicit plaintext mode: no TLS and
//! therefore no fingerprint.
//!
//! The secret part sits in the fragment, the one URL component that
//! clients do not send to servers and that proxies and access logs do not
//! record, in case the string is ever handled by generic URL tooling.
//!
//! Whoever can alter the string in transit can substitute their own
//! controller and fingerprint. It has to reach the new machine over a
//! channel the administrator trusts (their own terminal, an SSH session),
//! exactly like the pairing code inside it.

use std::fmt;

use crate::fingerprint::{Fingerprint, FingerprintError};
use crate::pairing::{PairingCode, PairingError};
use crate::url::{ControllerUrl, Scheme, UrlError, parse_authority};

const SCHEME_TLS: &str = "bella";
const SCHEME_PLAINTEXT: &str = "bella+http";

/// A controller address, the fingerprint to pin and a pairing code.
///
/// Invariant: an HTTPS URL always comes with a fingerprint and an HTTP URL
/// never does, so a value of this type cannot describe TLS without a pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enrollment {
    url: ControllerUrl,
    fingerprint: Option<Fingerprint>,
    code: PairingCode,
}

/// An enrollment string or its parts were rejected.
///
/// Messages never include the pairing code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnrollmentError {
    #[error("the enrollment string is empty")]
    Empty,
    #[error("the enrollment string contains whitespace or control characters")]
    IllegalCharacter,
    #[error("an enrollment string starts with bella:// (or bella+http:// for plaintext mode)")]
    UnknownScheme,
    #[error("the enrollment string has no `#` part carrying the fingerprint and pairing code")]
    MissingFragment,
    #[error(
        "a bella:// enrollment string needs `<fingerprint>.<pairing code>` after the `#`; the fingerprint is missing"
    )]
    MissingFingerprint,
    #[error(
        "a bella+http:// enrollment string carries only a pairing code after the `#`; plaintext mode has no certificate to pin"
    )]
    UnexpectedFingerprint,
    #[error("invalid fingerprint in the enrollment string: {0}")]
    Fingerprint(#[source] FingerprintError),
    #[error("invalid pairing code in the enrollment string: {0}")]
    Code(#[source] PairingError),
    #[error("invalid controller address in the enrollment string: {0}")]
    Url(#[source] UrlError),
    #[error("an https controller needs a certificate fingerprint to pin")]
    FingerprintRequired,
    #[error("a plain http controller has no certificate, so a fingerprint cannot apply")]
    FingerprintNotApplicable,
}

impl Enrollment {
    /// Combines the parts, enforcing that HTTPS has a fingerprint and HTTP
    /// has none.
    pub fn new(
        url: ControllerUrl,
        fingerprint: Option<Fingerprint>,
        code: PairingCode,
    ) -> Result<Enrollment, EnrollmentError> {
        match (url.scheme(), fingerprint) {
            (Scheme::Https, None) => Err(EnrollmentError::FingerprintRequired),
            (Scheme::Http, Some(_)) => Err(EnrollmentError::FingerprintNotApplicable),
            _ => Ok(Enrollment {
                url,
                fingerprint,
                code,
            }),
        }
    }

    pub fn url(&self) -> &ControllerUrl {
        &self.url
    }

    /// The certificate to pin; `None` exactly when the URL is plain HTTP.
    pub fn fingerprint(&self) -> Option<Fingerprint> {
        self.fingerprint
    }

    pub fn code(&self) -> PairingCode {
        self.code
    }

    /// Parses an enrollment string.
    ///
    /// The shape is strict: one of the two schemes, `host[:port]`, an
    /// optional `/`, then `#` and the payload. No path, query or user
    /// information. The fingerprint must be the 43-character base64url
    /// form. The pairing code is read with [`PairingCode::parse`], so its
    /// case and dash do not matter. Surrounding whitespace is ignored.
    ///
    /// A `bella+http://` string naming a host other than loopback is
    /// refused unless `allow_insecure` is true: pasting a string must not
    /// be enough to make a machine send its credentials unencrypted.
    pub fn parse(input: &str, allow_insecure: bool) -> Result<Enrollment, EnrollmentError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(EnrollmentError::Empty);
        }
        if input.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(EnrollmentError::IllegalCharacter);
        }
        let (scheme_text, rest) = input
            .split_once("://")
            .ok_or(EnrollmentError::UnknownScheme)?;
        let scheme = if scheme_text.eq_ignore_ascii_case(SCHEME_TLS) {
            Scheme::Https
        } else if scheme_text.eq_ignore_ascii_case(SCHEME_PLAINTEXT) {
            Scheme::Http
        } else {
            return Err(EnrollmentError::UnknownScheme);
        };
        let (address, payload) = rest
            .split_once('#')
            .ok_or(EnrollmentError::MissingFragment)?;

        let (host, port) = parse_authority(address).map_err(EnrollmentError::Url)?;
        let url =
            ControllerUrl::new(scheme, host, port, allow_insecure).map_err(EnrollmentError::Url)?;

        let (fingerprint, code_text) = match (scheme, payload.split_once('.')) {
            (Scheme::Https, Some((fingerprint, code))) => {
                let fingerprint = Fingerprint::from_base64url(fingerprint)
                    .map_err(EnrollmentError::Fingerprint)?;
                (Some(fingerprint), code)
            }
            (Scheme::Https, None) => return Err(EnrollmentError::MissingFingerprint),
            (Scheme::Http, Some(_)) => return Err(EnrollmentError::UnexpectedFingerprint),
            (Scheme::Http, None) => (None, payload),
        };
        let code = PairingCode::parse(code_text).map_err(EnrollmentError::Code)?;
        Enrollment::new(url, fingerprint, code)
    }
}

/// The paste-able string. It contains the pairing code, so treat the
/// output as a secret for as long as the code is valid.
impl fmt::Display for Enrollment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.fingerprint {
            Some(fingerprint) => write!(
                f,
                "{SCHEME_TLS}://{}/#{}.{}",
                self.url.authority(),
                fingerprint.to_base64url(),
                self.code
            ),
            None => write!(
                f,
                "{SCHEME_PLAINTEXT}://{}/#{}",
                self.url.authority(),
                self.code
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "0123A-BCDE1";

    fn fingerprint() -> Fingerprint {
        Fingerprint::of_certificate(b"fleet certificate")
    }

    fn code() -> PairingCode {
        PairingCode::parse(CODE).unwrap()
    }

    fn pinned(url: &str) -> Enrollment {
        let url = ControllerUrl::parse(url, false).unwrap();
        Enrollment::new(url, Some(fingerprint()), code()).unwrap()
    }

    #[test]
    fn tls_enrollment_round_trips() {
        let enrollment = pinned("https://192.168.50.7:7420");
        let text = enrollment.to_string();
        assert_eq!(
            text,
            format!(
                "bella://192.168.50.7:7420/#{}.{CODE}",
                fingerprint().to_base64url()
            )
        );
        let parsed = Enrollment::parse(&text, false).unwrap();
        assert_eq!(parsed, enrollment);
        assert_eq!(parsed.url().to_string(), "https://192.168.50.7:7420");
        assert_eq!(parsed.fingerprint(), Some(fingerprint()));
        assert_eq!(parsed.code(), code());
        assert_eq!(parsed.to_string(), text);
    }

    #[test]
    fn names_and_ipv6_hosts_round_trip() {
        for url in [
            "https://build-box.local:7420",
            "https://[fd00::12]:9443",
            "https://localhost:1",
        ] {
            let enrollment = pinned(url);
            let text = enrollment.to_string();
            assert_eq!(Enrollment::parse(&text, false), Ok(enrollment), "{text}");
        }
        assert!(
            pinned("https://[fd00::12]:9443")
                .to_string()
                .starts_with("bella://[fd00::12]:9443/#")
        );
    }

    #[test]
    fn plaintext_enrollment_round_trips_on_loopback() {
        let url = ControllerUrl::parse("http://127.0.0.1:7420", false).unwrap();
        let enrollment = Enrollment::new(url, None, code()).unwrap();
        let text = enrollment.to_string();
        assert_eq!(text, format!("bella+http://127.0.0.1:7420/#{CODE}"));
        let parsed = Enrollment::parse(&text, false).unwrap();
        assert_eq!(parsed, enrollment);
        assert_eq!(parsed.fingerprint(), None);
        assert!(!parsed.url().is_tls());
    }

    #[test]
    fn plaintext_to_another_machine_needs_the_explicit_flag() {
        let text = format!("bella+http://192.168.50.7:7420/#{CODE}");
        assert_eq!(
            Enrollment::parse(&text, false),
            Err(EnrollmentError::Url(UrlError::InsecureNotAllowed {
                host: "192.168.50.7".to_owned()
            }))
        );
        let parsed = Enrollment::parse(&text, true).unwrap();
        assert!(parsed.url().is_insecure_remote());
        assert_eq!(parsed.fingerprint(), None);
        assert_eq!(parsed.to_string(), text);
    }

    #[test]
    fn tolerated_variations_parse_to_the_same_value() {
        let enrollment = pinned("https://build-box.local:7420");
        let fp = fingerprint().to_base64url();
        for text in [
            format!("bella://build-box.local:7420/#{fp}.{CODE}"),
            // No slash before the fragment, default port, upper-case scheme
            // and host, code without its dash and in lower case.
            format!("bella://build-box.local:7420#{fp}.{CODE}"),
            format!("bella://build-box.local/#{fp}.{CODE}"),
            format!("BELLA://Build-Box.LOCAL:7420/#{fp}.0123abcde1"),
            format!("  bella://build-box.local:7420/#{fp}.{CODE}\n"),
        ] {
            assert_eq!(
                Enrollment::parse(&text, false),
                Ok(enrollment.clone()),
                "{text}"
            );
        }
    }

    #[test]
    fn malformed_strings_are_rejected_precisely() {
        let fp = fingerprint().to_base64url();
        let parse = |text: &str| Enrollment::parse(text, false);

        assert_eq!(parse(""), Err(EnrollmentError::Empty));
        assert_eq!(parse(" \n"), Err(EnrollmentError::Empty));
        assert_eq!(
            parse(&format!("bella://host/#{fp}. {CODE}")),
            Err(EnrollmentError::IllegalCharacter)
        );
        for scheme in ["https", "http", "bella+https", "bellas", ""] {
            assert_eq!(
                parse(&format!("{scheme}://host:7420/#{fp}.{CODE}")),
                Err(EnrollmentError::UnknownScheme),
                "{scheme}"
            );
        }
        assert_eq!(
            parse(&format!("host:7420/#{fp}.{CODE}")),
            Err(EnrollmentError::UnknownScheme)
        );
        assert_eq!(
            parse("bella://host:7420/"),
            Err(EnrollmentError::MissingFragment)
        );
        assert_eq!(
            parse(&format!("bella://host:7420/#{CODE}")),
            Err(EnrollmentError::MissingFingerprint)
        );
        assert_eq!(
            parse(&format!("bella+http://127.0.0.1:7420/#{fp}.{CODE}")),
            Err(EnrollmentError::UnexpectedFingerprint)
        );
        assert_eq!(
            parse("bella://host:7420/#"),
            Err(EnrollmentError::MissingFingerprint)
        );
    }

    #[test]
    fn the_fingerprint_must_be_canonical_base64url() {
        let parse = |text: String| Enrollment::parse(&text, false);
        // The hex spelling is not accepted inside an enrollment string.
        assert_eq!(
            parse(format!("bella://host/#{}.{CODE}", fingerprint().to_hex())),
            Err(EnrollmentError::Fingerprint(FingerprintError::Length {
                found: 64
            }))
        );
        let fp = fingerprint().to_base64url();
        assert_eq!(
            parse(format!("bella://host/#{}.{CODE}", &fp[..42])),
            Err(EnrollmentError::Fingerprint(FingerprintError::Length {
                found: 42
            }))
        );
        let mut bad = fp.clone();
        bad.replace_range(3..4, "!");
        assert_eq!(
            parse(format!("bella://host/#{bad}.{CODE}")),
            Err(EnrollmentError::Fingerprint(
                FingerprintError::InvalidBase64Url
            ))
        );
        assert_eq!(
            parse(format!("bella://host/#.{CODE}")),
            Err(EnrollmentError::Fingerprint(FingerprintError::Length {
                found: 0
            }))
        );
    }

    #[test]
    fn the_code_and_address_are_validated() {
        let fp = fingerprint().to_base64url();
        let parse = |text: String| Enrollment::parse(&text, false);

        assert_eq!(
            parse(format!("bella://host/#{fp}.")),
            Err(EnrollmentError::Code(PairingError::Length { found: 0 }))
        );
        assert_eq!(
            parse(format!("bella://host/#{fp}.0123A-BCDE")),
            Err(EnrollmentError::Code(PairingError::Length { found: 9 }))
        );
        // A second dot ends up in the code, where it is not a valid symbol.
        assert!(matches!(
            parse(format!("bella://host/#{fp}.{CODE}.extra")),
            Err(EnrollmentError::Code(PairingError::InvalidCharacter {
                character: '.',
                ..
            }))
        ));
        assert_eq!(
            parse(format!("bella://host:0/#{fp}.{CODE}")),
            Err(EnrollmentError::Url(UrlError::InvalidPort {
                port: "0".to_owned()
            }))
        );
        assert_eq!(
            parse(format!("bella://host/path#{fp}.{CODE}")),
            Err(EnrollmentError::Url(UrlError::UnexpectedPath))
        );
        assert_eq!(
            parse(format!("bella://host/?q=1#{fp}.{CODE}")),
            Err(EnrollmentError::Url(UrlError::UnexpectedQuery))
        );
        assert_eq!(
            parse(format!("bella://user@host/#{fp}.{CODE}")),
            Err(EnrollmentError::Url(UrlError::UserInfo))
        );
        assert_eq!(
            parse(format!("bella:///#{fp}.{CODE}")),
            Err(EnrollmentError::Url(UrlError::MissingHost))
        );
    }

    #[test]
    fn tls_without_a_pin_cannot_be_constructed() {
        let https = ControllerUrl::parse("https://host", false).unwrap();
        let http = ControllerUrl::parse("http://localhost", false).unwrap();
        assert_eq!(
            Enrollment::new(https, None, code()),
            Err(EnrollmentError::FingerprintRequired)
        );
        assert_eq!(
            Enrollment::new(http, Some(fingerprint()), code()),
            Err(EnrollmentError::FingerprintNotApplicable)
        );
    }

    #[test]
    fn errors_and_debug_output_do_not_reveal_the_code() {
        let enrollment = pinned("https://host");
        let debug = format!("{enrollment:?}");
        assert!(!debug.contains("0123A"), "{debug}");
        assert!(debug.contains(&fingerprint().to_hex()));

        let fp = fingerprint().to_base64url();
        let error = Enrollment::parse(&format!("bella://host/x#{fp}.{CODE}"), false).unwrap_err();
        assert!(!error.to_string().contains("0123A"));
    }
}
