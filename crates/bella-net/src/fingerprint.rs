//! SHA-256 certificate fingerprints.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

sha256_newtype!(
    /// SHA-256 of a certificate's DER encoding: the value a client pins.
    ///
    /// Displays as 64 lowercase hexadecimal characters. The 43-character
    /// unpadded base64url form ([`Fingerprint::to_base64url`]) is the short
    /// form used inside enrollment strings. Equality is constant time.
    ///
    /// A fingerprint is public: it identifies the fleet certificate and is
    /// safe to print, log and send over an untrusted channel. What must be
    /// trustworthy is the channel over which a person or a pairing string
    /// delivers it.
    Fingerprint,
    parse = Fingerprint::parse,
    error = FingerprintError
);

/// Length of the unpadded base64url form.
const BASE64URL_LEN: usize = 43;
/// Length of the plain hexadecimal form.
const HEX_LEN: usize = 64;
/// Length of the colon-separated form printed by OpenSSL (`AB:CD:…`).
const COLON_HEX_LEN: usize = 95;

/// A string is not a SHA-256 fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    #[error(
        "a fingerprint is 64 hexadecimal characters or 43 base64url characters, found {found} characters"
    )]
    Length { found: usize },
    #[error("character {position} of the fingerprint is not a hexadecimal digit")]
    InvalidHex {
        /// One-based position in the input.
        position: usize,
    },
    #[error("the fingerprint is not canonical unpadded base64url")]
    InvalidBase64Url,
}

impl Fingerprint {
    /// Fingerprint of a certificate given its DER encoding.
    pub fn of_certificate(der: &[u8]) -> Self {
        Self(crate::digest::sha256(der))
    }

    /// 43 characters of unpadded base64url (RFC 4648 §5).
    pub fn to_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// Parses the base64url form only. Non-canonical encodings (padding,
    /// stray trailing bits) are rejected, so every fingerprint has exactly
    /// one base64url spelling.
    pub fn from_base64url(input: &str) -> Result<Self, FingerprintError> {
        if input.len() != BASE64URL_LEN {
            return Err(FingerprintError::Length {
                found: input.chars().count(),
            });
        }
        let mut bytes = [0u8; 32];
        // 43 characters decode to at most 33 bytes; a strict decoder only
        // accepts those that encode exactly 32.
        let mut buffer = [0u8; 33];
        let written = URL_SAFE_NO_PAD
            .decode_slice(input, &mut buffer)
            .map_err(|_| FingerprintError::InvalidBase64Url)?;
        if written != bytes.len() {
            return Err(FingerprintError::InvalidBase64Url);
        }
        bytes.copy_from_slice(&buffer[..written]);
        Ok(Self(bytes))
    }

    /// Parses any of the accepted spellings:
    ///
    /// * 64 hexadecimal characters in either case;
    /// * the same with a colon between bytes, as `openssl x509 -fingerprint
    ///   -sha256` prints it;
    /// * 43 characters of unpadded base64url.
    ///
    /// Surrounding whitespace is ignored. The lengths differ, so the forms
    /// cannot be confused with one another.
    pub fn parse(input: &str) -> Result<Self, FingerprintError> {
        let input = input.trim();
        match input.len() {
            BASE64URL_LEN => Self::from_base64url(input),
            HEX_LEN | COLON_HEX_LEN => Self::from_hex_lenient(input),
            _ => Err(FingerprintError::Length {
                found: input.chars().count(),
            }),
        }
    }

    fn from_hex_lenient(input: &str) -> Result<Self, FingerprintError> {
        let colon_separated = input.len() == COLON_HEX_LEN;
        let mut bytes = [0u8; 32];
        let mut nibbles = 0usize;
        for (index, byte) in input.bytes().enumerate() {
            let position = index + 1;
            if colon_separated && index % 3 == 2 {
                if byte != b':' {
                    return Err(FingerprintError::InvalidHex { position });
                }
                continue;
            }
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(FingerprintError::InvalidHex { position }),
            };
            let slot = &mut bytes[nibbles / 2];
            *slot = (*slot << 4) | value;
            nibbles += 1;
        }
        Ok(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Fingerprint {
        Fingerprint::of_certificate(b"abc")
    }

    #[test]
    fn is_sha256_of_the_der_bytes() {
        assert_eq!(
            sample().to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sample().to_string(), sample().to_hex());
        assert_eq!(sample().as_bytes().len(), 32);
    }

    #[test]
    fn base64url_form_is_43_unpadded_url_safe_characters() {
        // All-ones bytes encode to `_` characters, which only the URL-safe
        // alphabet has.
        let all_ones = Fingerprint::from_bytes([0xff; 32]);
        let text = all_ones.to_base64url();
        assert_eq!(text.len(), 43);
        assert!(text.starts_with("____"));
        assert!(!text.contains('='));
        assert!(!text.contains('+') && !text.contains('/'));
        assert_eq!(Fingerprint::from_base64url(&text), Ok(all_ones));
    }

    #[test]
    fn every_spelling_round_trips() {
        let fp = sample();
        assert_eq!(Fingerprint::parse(&fp.to_hex()), Ok(fp));
        assert_eq!(Fingerprint::parse(&fp.to_hex().to_uppercase()), Ok(fp));
        assert_eq!(Fingerprint::parse(&fp.to_base64url()), Ok(fp));
        assert_eq!(fp.to_hex().parse::<Fingerprint>(), Ok(fp));
        assert_eq!(Fingerprint::parse(&format!("  {fp}\n")), Ok(fp));

        let colon: Vec<String> = fp.as_bytes().iter().map(|b| format!("{b:02X}")).collect();
        let colon = colon.join(":");
        assert_eq!(colon.len(), 95);
        assert_eq!(Fingerprint::parse(&colon), Ok(fp));
    }

    #[test]
    fn malformed_input_is_rejected_with_a_position() {
        let hex = sample().to_hex();
        assert_eq!(
            Fingerprint::parse(&hex[..62]),
            Err(FingerprintError::Length { found: 62 })
        );
        assert_eq!(
            Fingerprint::parse(""),
            Err(FingerprintError::Length { found: 0 })
        );

        let mut bad = hex.clone();
        bad.replace_range(4..5, "x");
        assert_eq!(
            Fingerprint::parse(&bad),
            Err(FingerprintError::InvalidHex { position: 5 })
        );

        // A colon form with a separator in the wrong place.
        let colon: Vec<String> = sample()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut colon = colon.join(":");
        colon.replace_range(2..3, "-");
        assert_eq!(
            Fingerprint::parse(&colon),
            Err(FingerprintError::InvalidHex { position: 3 })
        );
    }

    #[test]
    fn base64url_must_be_canonical() {
        let text = sample().to_base64url();

        // Standard alphabet characters are not URL-safe base64.
        let mut plus = text.clone();
        plus.replace_range(0..1, "+");
        assert_eq!(
            Fingerprint::from_base64url(&plus),
            Err(FingerprintError::InvalidBase64Url)
        );

        // The last character carries 4 data bits and 2 padding bits that
        // must be zero. Setting a padding bit yields a second spelling of
        // the same bytes, which is refused.
        let last = text.as_bytes()[42];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let index = alphabet.iter().position(|c| *c == last).unwrap();
        assert_eq!(index % 4, 0, "canonical encodings end on a multiple of 4");
        let mut sloppy = text.clone();
        sloppy.replace_range(42..43, &(alphabet[index + 1] as char).to_string());
        assert_eq!(
            Fingerprint::from_base64url(&sloppy),
            Err(FingerprintError::InvalidBase64Url)
        );

        // Padding is not accepted, and neither is the hex form here.
        assert_eq!(
            Fingerprint::from_base64url(&format!("{text}=")),
            Err(FingerprintError::Length { found: 44 })
        );
        assert_eq!(
            Fingerprint::from_base64url(&sample().to_hex()),
            Err(FingerprintError::Length { found: 64 })
        );
    }

    #[test]
    fn equality_and_hashing_follow_the_bytes() {
        use std::collections::HashSet;
        let a = Fingerprint::of_certificate(b"a");
        let b = Fingerprint::of_certificate(b"b");
        assert_eq!(a, Fingerprint::from_bytes(*a.as_bytes()));
        assert_ne!(a, b);
        let set: HashSet<Fingerprint> = [a, b, a].into_iter().collect();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn serde_uses_the_hex_string_and_accepts_base64url() {
        let fp = sample();
        let json = serde_json::to_string(&fp).unwrap();
        assert_eq!(json, format!("\"{}\"", fp.to_hex()));
        assert_eq!(serde_json::from_str::<Fingerprint>(&json).unwrap(), fp);
        let short = format!("\"{}\"", fp.to_base64url());
        assert_eq!(serde_json::from_str::<Fingerprint>(&short).unwrap(), fp);
        assert!(serde_json::from_str::<Fingerprint>("\"nope\"").is_err());
        assert_eq!(format!("{fp:?}"), format!("Fingerprint({})", fp.to_hex()));
    }
}
