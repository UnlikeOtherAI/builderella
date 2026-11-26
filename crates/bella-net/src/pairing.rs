//! Pairing codes: the short secret a person carries from the controller to
//! a machine that is joining the fleet.
//!
//! # Strength
//!
//! A code is 10 Crockford base32 symbols, 50 random bits, about
//! 1.1 × 10^15 possibilities. That is short enough to read aloud and far
//! too short to be a long-lived secret. It is safe only under the three
//! conditions the controller and ledger must enforce:
//!
//! * **Single use.** A code is consumed by the first enrollment that
//!   presents it.
//! * **Short lived.** `bella_core::defaults::PAIRING_CODE_TTL` is ten
//!   minutes.
//! * **Rate limited.** The server must limit enrollment attempts, per
//!   source address and in total. At 10 attempts per second in total an
//!   attacker makes 6 000 guesses in a code's lifetime, a success chance
//!   of 6 000 / 2^50 ≈ 5 × 10^-12 per outstanding code. Without a limit, a
//!   million guesses per second would reach about 5 × 10^-7 per code, and
//!   nothing would stop it being repeated indefinitely.
//!
//! # What the hash does and does not protect
//!
//! [`PairingCode::hash`] keeps live codes out of the ledger file and its
//! backups in readable form. It is **not** resistant to offline search: 2^50
//! SHA-256 evaluations are within reach of commodity hardware in days.
//! Someone who reads the ledger within a code's lifetime can recover the
//! code. The defence is the lifetime, not the hash.

use std::fmt;

use subtle::ConstantTimeEq;

use crate::digest::HexDigestError;
use crate::random::{self, RandomError};

/// Crockford base32: digits and upper-case letters without I, L, O and U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Symbols in a code.
const SYMBOLS: usize = 10;

/// Hashed in front of the code so a pairing-code hash can never equal the
/// hash of some other kind of secret.
const HASH_DOMAIN: &[u8] = b"builderella pairing code v1\n";

/// A single-use code that lets one machine enroll.
///
/// Holds the canonical ten symbols. Displays as `XXXXX-XXXXX`. `Debug`
/// output is redacted; equality is constant time.
#[derive(Clone, Copy)]
pub struct PairingCode([u8; SYMBOLS]);

/// Text is not a pairing code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("a pairing code has 10 characters (XXXXX-XXXXX), found {found}")]
    Length { found: usize },
    #[error(
        "character {position} (`{character}`) cannot appear in a pairing code; codes use digits and letters except U"
    )]
    InvalidCharacter {
        character: char,
        /// One-based position in the text as typed.
        position: usize,
    },
}

impl PairingCode {
    /// Number of random bits in a code.
    pub const BITS: u32 = 50;

    /// Draws a new code from the operating system's random number
    /// generator. Every symbol is uniform: 32 symbols take exactly five
    /// bits each, so there is no modulo bias.
    pub fn generate() -> Result<Self, RandomError> {
        let mut bytes = [0u8; SYMBOLS];
        random::fill(&mut bytes)?;
        let mut symbols = [0u8; SYMBOLS];
        for (symbol, byte) in symbols.iter_mut().zip(bytes) {
            *symbol = ALPHABET[usize::from(byte & 0x1f)];
        }
        Ok(Self(symbols))
    }

    /// Parses a code as a person might type it.
    ///
    /// Accepts either case, the dash anywhere or nowhere, spaces, and the
    /// Crockford substitutions for look-alike characters: `O` is read as
    /// `0`, `I` and `L` as `1`. `U` is not part of the alphabet and is
    /// rejected along with every other character.
    pub fn parse(input: &str) -> Result<Self, PairingError> {
        let mut symbols = [0u8; SYMBOLS];
        let mut found = 0usize;
        for (index, character) in input.chars().enumerate() {
            if character == '-' || character.is_ascii_whitespace() {
                continue;
            }
            let symbol = match character.to_ascii_uppercase() {
                'O' => b'0',
                'I' | 'L' => b'1',
                upper if upper.is_ascii() && ALPHABET.contains(&(upper as u8)) => upper as u8,
                _ => {
                    return Err(PairingError::InvalidCharacter {
                        character,
                        position: index + 1,
                    });
                }
            };
            if found < SYMBOLS {
                symbols[found] = symbol;
            }
            found += 1;
        }
        if found != SYMBOLS {
            return Err(PairingError::Length { found });
        }
        Ok(Self(symbols))
    }

    /// The canonical display form (`XXXXX-XXXXX`) of whatever was typed.
    pub fn normalize(input: &str) -> Result<String, PairingError> {
        Self::parse(input).map(|code| code.to_string())
    }

    /// The ten symbols without the dash.
    pub fn compact(&self) -> String {
        self.0.iter().map(|byte| char::from(*byte)).collect()
    }

    /// The value the ledger stores and looks codes up by.
    ///
    /// SHA-256 over a fixed domain prefix and the ten canonical symbols, so
    /// every accepted spelling of a code has the same hash. See the module
    /// documentation for why this hash does not make a stored code safe
    /// beyond its lifetime.
    pub fn hash(&self) -> PairingCodeHash {
        PairingCodeHash(crate::digest::sha256_parts(&[HASH_DOMAIN, &self.0]))
    }
}

/// Equality runs in constant time with respect to the contents.
impl PartialEq for PairingCode {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice().ct_eq(other.0.as_slice()).into()
    }
}

impl Eq for PairingCode {}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.compact();
        write!(f, "{}-{}", &text[..5], &text[5..])
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

impl std::str::FromStr for PairingCode {
    type Err = PairingError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

impl serde::Serialize for PairingCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for PairingCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

sha256_newtype!(
    /// Hash of a pairing code, as stored by the ledger. See
    /// [`PairingCode::hash`] and the module documentation for its limits.
    ///
    /// Displays as 64 lowercase hexadecimal characters, the only accepted
    /// stored form. Equality is constant time.
    PairingCodeHash,
    parse = PairingCodeHash::from_hex,
    error = HexDigestError
);

impl PairingCodeHash {
    /// Parses a stored hash: exactly 64 lowercase hexadecimal characters.
    pub fn from_hex(input: &str) -> Result<Self, HexDigestError> {
        crate::digest::parse_hex(input).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_codes_have_the_documented_shape() {
        let mut seen = HashSet::new();
        for _ in 0..500 {
            let code = PairingCode::generate().unwrap();
            let shown = code.to_string();
            assert_eq!(shown.len(), 11);
            assert_eq!(shown.as_bytes()[5], b'-');
            let compact = code.compact();
            assert_eq!(compact.len(), 10);
            assert!(compact.bytes().all(|b| ALPHABET.contains(&b)), "{compact}");
            assert_eq!(PairingCode::parse(&shown), Ok(code));
            assert_eq!(PairingCode::parse(&compact), Ok(code));
            seen.insert(compact);
        }
        // 500 draws from 2^50 values collide with probability about 1e-10.
        assert_eq!(seen.len(), 500);
    }

    #[test]
    fn the_alphabet_is_crockford_base32() {
        assert_eq!(ALPHABET.len(), 32);
        let distinct: HashSet<u8> = ALPHABET.iter().copied().collect();
        assert_eq!(distinct.len(), 32);
        for excluded in b"ILOU" {
            assert!(!ALPHABET.contains(excluded));
        }
        assert_eq!(PairingCode::BITS, 5 * SYMBOLS as u32);
    }

    #[test]
    fn every_symbol_value_can_be_generated() {
        // 2 000 symbols over 32 values: a missing value would mean the
        // mapping from random bits to symbols is broken, not bad luck
        // (chance of a false failure is about 32 × (31/32)^2000 ≈ 1e-26).
        let mut seen = HashSet::new();
        for _ in 0..200 {
            seen.extend(PairingCode::generate().unwrap().compact().bytes());
        }
        assert_eq!(seen.len(), 32);
    }

    #[test]
    fn normalisation_accepts_what_people_type() {
        let canonical = "0123A-BCDE1";
        for typed in [
            "0123A-BCDE1",
            "0123abcde1",
            "0123a-bcde1",
            "o123A-BCDEi",
            "O123A-BCDEL",
            "0123A-BCDEl",
            "  0123A BCDE1\n",
            "01-23-AB-CD-E1",
        ] {
            assert_eq!(
                PairingCode::normalize(typed).as_deref(),
                Ok(canonical),
                "{typed:?}"
            );
        }
        let code = PairingCode::parse(canonical).unwrap();
        assert_eq!(code.compact(), "0123ABCDE1");
        assert_eq!("0123abcde1".parse::<PairingCode>(), Ok(code));
    }

    #[test]
    fn malformed_codes_are_rejected_precisely() {
        assert_eq!(
            PairingCode::parse(""),
            Err(PairingError::Length { found: 0 })
        );
        assert_eq!(
            PairingCode::parse("-----"),
            Err(PairingError::Length { found: 0 })
        );
        assert_eq!(
            PairingCode::parse("0123A-BCDE"),
            Err(PairingError::Length { found: 9 })
        );
        assert_eq!(
            PairingCode::parse("0123A-BCDE12"),
            Err(PairingError::Length { found: 11 })
        );
        // U is the one letter Crockford base32 leaves out without mapping.
        assert_eq!(
            PairingCode::parse("0123U-BCDE1"),
            Err(PairingError::InvalidCharacter {
                character: 'U',
                position: 5
            })
        );
        assert_eq!(
            PairingCode::parse("0123a_bcde1"),
            Err(PairingError::InvalidCharacter {
                character: '_',
                position: 6
            })
        );
        assert_eq!(
            PairingCode::parse("0123é-BCDE1"),
            Err(PairingError::InvalidCharacter {
                character: 'é',
                position: 5
            })
        );
    }

    #[test]
    fn hash_is_the_same_for_every_spelling_and_differs_between_codes() {
        let a = PairingCode::parse("0123A-BCDE1").unwrap();
        let same = PairingCode::parse("o123abcdel").unwrap();
        let other = PairingCode::parse("0123A-BCDE2").unwrap();
        assert_eq!(a.hash(), same.hash());
        assert_ne!(a.hash(), other.hash());
        assert_ne!(a, other);

        let expected = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(b"builderella pairing code v1\n");
            hasher.update(b"0123ABCDE1");
            hex::encode(hasher.finalize())
        };
        assert_eq!(a.hash().to_hex(), expected);
        assert_eq!(PairingCodeHash::from_hex(&expected), Ok(a.hash()));
        // Not the bare SHA-256 of the code, so it cannot be mistaken for a
        // token hash of the same text.
        assert_ne!(
            a.hash().to_hex(),
            crate::TokenHash::of("0123ABCDE1").to_hex()
        );
    }

    #[test]
    fn debug_output_is_redacted_and_serde_uses_the_display_form() {
        let code = PairingCode::parse("0123A-BCDE1").unwrap();
        assert_eq!(format!("{code:?}"), "PairingCode(<redacted>)");
        let json = serde_json::to_string(&code).unwrap();
        assert_eq!(json, "\"0123A-BCDE1\"");
        assert_eq!(
            serde_json::from_str::<PairingCode>("\"0123abcde1\"").unwrap(),
            code
        );
        assert!(serde_json::from_str::<PairingCode>("\"short\"").is_err());
    }
}
