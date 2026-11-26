//! Shared plumbing for the SHA-256 newtypes ([`crate::Fingerprint`],
//! [`crate::TokenHash`], [`crate::PairingCodeHash`]).

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Length of a SHA-256 digest in bytes.
pub(crate) const LEN: usize = 32;

/// SHA-256 of `data`.
pub(crate) fn sha256(data: &[u8]) -> [u8; LEN] {
    Sha256::digest(data).into()
}

/// SHA-256 of the concatenation of `parts`.
pub(crate) fn sha256_parts(parts: &[&[u8]]) -> [u8; LEN] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Compares two digests without an early exit on the first differing byte.
pub(crate) fn ct_eq(a: &[u8; LEN], b: &[u8; LEN]) -> bool {
    a.as_slice().ct_eq(b.as_slice()).into()
}

/// A stored digest is not 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HexDigestError {
    #[error("expected 64 hexadecimal characters, found {found}")]
    Length { found: usize },
    #[error("character {position} is not a lowercase hexadecimal digit")]
    InvalidCharacter {
        /// One-based position of the offending character.
        position: usize,
    },
}

/// Parses the canonical form written by `to_hex`: exactly 64 lowercase
/// hexadecimal characters. Upper case is rejected so that a digest has one
/// textual form and can be compared or indexed as a string.
pub(crate) fn parse_hex(input: &str) -> Result<[u8; LEN], HexDigestError> {
    let bytes = input.as_bytes();
    if bytes.len() != LEN * 2 {
        return Err(HexDigestError::Length {
            found: input.chars().count(),
        });
    }
    let mut out = [0u8; LEN];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let high = lower_hex_value(pair[0]).ok_or(HexDigestError::InvalidCharacter {
            position: index * 2 + 1,
        })?;
        let low = lower_hex_value(pair[1]).ok_or(HexDigestError::InvalidCharacter {
            position: index * 2 + 2,
        })?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

fn lower_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Defines a 32-byte SHA-256 newtype with constant-time equality, lowercase
/// hexadecimal `Display`, and serde as a string parsed by `$parse`.
macro_rules! sha256_newtype {
    (
        $(#[$meta:meta])*
        $name:ident, parse = $parse:path, error = $error:ty
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy)]
        pub struct $name([u8; 32]);

        impl $name {
            /// Wraps an already computed SHA-256 value.
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// The raw 32 bytes.
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// 64 lowercase hexadecimal characters.
            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }
        }

        /// Equality runs in constant time with respect to the contents.
        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                $crate::digest::ct_eq(&self.0, &other.0)
            }
        }

        impl Eq for $name {}

        impl std::hash::Hash for $name {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.0.hash(state);
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.to_hex())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $error;

            fn from_str(input: &str) -> Result<Self, Self::Err> {
                $parse(input)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_hex())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(
                    deserializer,
                )?;
                $parse(&text).map_err(serde::de::Error::custom)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_published_test_vector() {
        // FIPS 180-2 appendix B.1: SHA-256("abc").
        assert_eq!(
            hex::encode(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_parts(&[b"a", b"bc"]), sha256(b"abc"));
    }

    #[test]
    fn hex_round_trips_and_is_strict() {
        let digest = sha256(b"abc");
        let text = hex::encode(digest);
        assert_eq!(parse_hex(&text), Ok(digest));

        assert_eq!(
            parse_hex(&text[..63]),
            Err(HexDigestError::Length { found: 63 })
        );
        assert_eq!(
            parse_hex(&text.to_uppercase()),
            Err(HexDigestError::InvalidCharacter { position: 1 })
        );
        let mut bad = text.clone();
        bad.replace_range(10..11, "g");
        assert_eq!(
            parse_hex(&bad),
            Err(HexDigestError::InvalidCharacter { position: 11 })
        );
        // Multi-byte characters must not be sliced mid-character.
        let wide = "é".repeat(32);
        assert_eq!(wide.len(), 64);
        assert!(matches!(
            parse_hex(&wide),
            Err(HexDigestError::InvalidCharacter { position: 1 })
        ));
    }

    #[test]
    fn constant_time_compare_agrees_with_plain_equality() {
        let a = sha256(b"a");
        let mut b = a;
        assert!(ct_eq(&a, &b));
        b[31] ^= 1;
        assert!(!ct_eq(&a, &b));
        b = a;
        b[0] ^= 0x80;
        assert!(!ct_eq(&a, &b));
    }
}
