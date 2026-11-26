//! Bearer tokens and the hashes the ledger stores for them.
//!
//! A token looks like `bella_dev_<43 base64url characters>`. The fixed
//! prefix and kind tag make a leaked token recognisable to secret scanners
//! and tell a reader what it can do; all of the security is in the 32
//! random bytes that follow.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bella_core::api::Role;
use serde::{Deserialize, Serialize};

use crate::digest::HexDigestError;
use crate::random::{self, RandomError};

/// Every Builderella token starts with this.
pub const TOKEN_PREFIX: &str = "bella_";

/// Random bytes in a token.
const SECRET_BYTES: usize = 32;
/// Characters those bytes take as unpadded base64url.
const SECRET_CHARS: usize = 43;

/// What a token is for. The tag is part of the token text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Fleet administration: policy, enrollment, token minting.
    Admin,
    /// Submit, cancel and inspect jobs.
    Developer,
    /// Read-only access.
    Viewer,
    /// A worker's own credential, issued at enrollment.
    Machine,
}

impl TokenKind {
    /// All kinds, in a stable order.
    pub const ALL: [TokenKind; 4] = [
        TokenKind::Admin,
        TokenKind::Developer,
        TokenKind::Viewer,
        TokenKind::Machine,
    ];

    /// The tag that appears in the token: `adm`, `dev`, `view` or `mch`.
    pub const fn tag(self) -> &'static str {
        match self {
            TokenKind::Admin => "adm",
            TokenKind::Developer => "dev",
            TokenKind::Viewer => "view",
            TokenKind::Machine => "mch",
        }
    }

    /// The kind for a tag, if it is one of the four.
    pub fn from_tag(tag: &str) -> Option<TokenKind> {
        TokenKind::ALL.into_iter().find(|kind| kind.tag() == tag)
    }

    /// Reads the kind out of a token and checks that the whole token is
    /// well formed: prefix, known tag, and exactly 43 canonical base64url
    /// characters encoding 32 bytes.
    ///
    /// This says what the token claims to be. It is not authentication: a
    /// well-formed token is valid only if the ledger holds its hash, and
    /// the role recorded there is the one that counts.
    pub fn from_secret(secret: &str) -> Result<TokenKind, TokenError> {
        let rest = secret
            .strip_prefix(TOKEN_PREFIX)
            .ok_or(TokenError::MissingPrefix)?;
        // Tags contain no underscore, so the first one ends the tag even
        // though the base64url body may contain more.
        let (tag, body) = rest.split_once('_').ok_or(TokenError::Malformed)?;
        let kind = TokenKind::from_tag(tag).ok_or(TokenError::UnknownKind)?;
        if body.len() != SECRET_CHARS {
            return Err(TokenError::Malformed);
        }
        let mut buffer = [0u8; SECRET_BYTES + 1];
        match URL_SAFE_NO_PAD.decode_slice(body, &mut buffer) {
            Ok(SECRET_BYTES) => Ok(kind),
            _ => Err(TokenError::Malformed),
        }
    }

    /// The token kind issued to a principal with `role`.
    pub const fn for_role(role: Role) -> TokenKind {
        match role {
            Role::Admin => TokenKind::Admin,
            Role::Developer => TokenKind::Developer,
            Role::Viewer => TokenKind::Viewer,
        }
    }

    /// The principal role this kind corresponds to. Machine tokens belong
    /// to workers, not principals, and have none.
    pub const fn role(self) -> Option<Role> {
        match self {
            TokenKind::Admin => Some(Role::Admin),
            TokenKind::Developer => Some(Role::Developer),
            TokenKind::Viewer => Some(Role::Viewer),
            TokenKind::Machine => None,
        }
    }
}

/// A string is not a Builderella token.
///
/// The messages never repeat any part of the input, so a mistyped token
/// does not end up in a log through its own error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("not a Builderella token: it does not start with `bella_`")]
    MissingPrefix,
    #[error(
        "unknown token kind: expected `bella_adm_`, `bella_dev_`, `bella_view_` or `bella_mch_`"
    )]
    UnknownKind,
    #[error("malformed token: the part after the kind must be 43 base64url characters")]
    Malformed,
}

/// Token minting.
pub struct Token;

impl Token {
    /// Mints a token of `kind` and returns it with the hash to store.
    ///
    /// The secret is `bella_<tag>_<43 base64url characters>` carrying 32
    /// bytes read from the operating system's random number generator. It
    /// is returned once; persist only the [`TokenHash`].
    ///
    /// Fails only if the operating system cannot supply random bytes, in
    /// which case no token exists and nothing weaker is substituted.
    pub fn generate(kind: TokenKind) -> Result<(String, TokenHash), RandomError> {
        let mut bytes = [0u8; SECRET_BYTES];
        random::fill(&mut bytes)?;
        let mut secret =
            String::with_capacity(TOKEN_PREFIX.len() + kind.tag().len() + 1 + SECRET_CHARS);
        secret.push_str(TOKEN_PREFIX);
        secret.push_str(kind.tag());
        secret.push('_');
        URL_SAFE_NO_PAD.encode_string(bytes, &mut secret);
        let hash = TokenHash::of(&secret);
        Ok((secret, hash))
    }
}

sha256_newtype!(
    /// SHA-256 of a complete token string, as stored by the ledger.
    ///
    /// A plain, unsalted hash is appropriate because the input carries 256
    /// bits of entropy: it cannot be guessed or found in a table, so a slow
    /// password hash would add cost and no protection. Do not use this type
    /// for anything a person chose.
    ///
    /// Displays as 64 lowercase hexadecimal characters, the only accepted
    /// stored form. Equality is constant time.
    TokenHash,
    parse = TokenHash::from_hex,
    error = HexDigestError
);

impl TokenHash {
    /// Hash of `secret` exactly as presented, including its prefix.
    pub fn of(secret: &str) -> Self {
        Self(crate::digest::sha256(secret.as_bytes()))
    }

    /// Parses a stored hash: exactly 64 lowercase hexadecimal characters.
    pub fn from_hex(input: &str) -> Result<Self, HexDigestError> {
        crate::digest::parse_hex(input).map(Self)
    }

    /// Whether `secret` hashes to this value, compared in constant time.
    pub fn matches(&self, secret: &str) -> bool {
        *self == TokenHash::of(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_tokens_have_the_documented_shape() {
        for kind in TokenKind::ALL {
            let (secret, hash) = Token::generate(kind).unwrap();
            let prefix = format!("bella_{}_", kind.tag());
            assert!(secret.starts_with(&prefix), "{secret}");
            let body = &secret[prefix.len()..];
            assert_eq!(body.len(), 43);
            assert!(
                body.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            );
            assert_eq!(URL_SAFE_NO_PAD.decode(body).unwrap().len(), 32);
            assert_eq!(TokenKind::from_secret(&secret), Ok(kind));
            assert_eq!(hash, TokenHash::of(&secret));
            assert!(hash.matches(&secret));
        }
    }

    #[test]
    fn tags_are_the_four_documented_ones() {
        let tags: Vec<&str> = TokenKind::ALL.iter().map(|k| k.tag()).collect();
        assert_eq!(tags, ["adm", "dev", "view", "mch"]);
        for kind in TokenKind::ALL {
            assert_eq!(TokenKind::from_tag(kind.tag()), Some(kind));
        }
        assert_eq!(TokenKind::from_tag("root"), None);
        assert_eq!(TokenKind::from_tag("ADM"), None);
    }

    #[test]
    fn tokens_are_unique() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let (secret, _) = Token::generate(TokenKind::Developer).unwrap();
            assert!(seen.insert(secret));
        }
    }

    #[test]
    fn hash_is_stable_hex_sha256_of_the_whole_secret() {
        // SHA-256 of the ASCII text below, computed independently.
        let secret = "bella_dev_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(secret.len(), "bella_dev_".len() + 43);
        let expected = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(secret.as_bytes()));
        let hash = TokenHash::of(secret);
        assert_eq!(hash.to_hex(), expected);
        assert_eq!(hash.to_string(), expected);
        assert_eq!(hash, TokenHash::of(secret));
        assert_eq!(TokenHash::from_hex(&expected), Ok(hash));
        assert_eq!(expected.parse::<TokenHash>(), Ok(hash));

        // The prefix is part of what is hashed.
        let other_kind = secret.replace("_dev_", "_adm_");
        assert_ne!(TokenHash::of(&other_kind), hash);
        assert!(!hash.matches(&other_kind));
        assert!(!hash.matches(""));
    }

    #[test]
    fn stored_hashes_have_one_spelling() {
        let hash = TokenHash::of("x");
        assert!(TokenHash::from_hex(&hash.to_hex().to_uppercase()).is_err());
        assert!(TokenHash::from_hex("abc").is_err());
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(json, format!("\"{}\"", hash.to_hex()));
        assert_eq!(serde_json::from_str::<TokenHash>(&json).unwrap(), hash);
    }

    #[test]
    fn kind_parsing_rejects_everything_that_is_not_a_whole_token() {
        let (secret, _) = Token::generate(TokenKind::Viewer).unwrap();
        let body = &secret["bella_view_".len()..];

        assert_eq!(TokenKind::from_secret(""), Err(TokenError::MissingPrefix));
        assert_eq!(
            TokenKind::from_secret(&format!("ghp_{body}")),
            Err(TokenError::MissingPrefix)
        );
        assert_eq!(
            TokenKind::from_secret(&format!("Bella_view_{body}")),
            Err(TokenError::MissingPrefix)
        );
        assert_eq!(
            TokenKind::from_secret("bella_view"),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            TokenKind::from_secret(&format!("bella_root_{body}")),
            Err(TokenError::UnknownKind)
        );
        // Wrong length in either direction.
        assert_eq!(
            TokenKind::from_secret(&secret[..secret.len() - 1]),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            TokenKind::from_secret(&format!("{secret}A")),
            Err(TokenError::Malformed)
        );
        // Right length, wrong alphabet.
        let mut bad = secret.clone();
        bad.replace_range(secret.len() - 5..secret.len() - 4, "+");
        assert_eq!(TokenKind::from_secret(&bad), Err(TokenError::Malformed));
        // Surrounding whitespace is not silently accepted.
        assert_eq!(
            TokenKind::from_secret(&format!("{secret}\n")),
            Err(TokenError::Malformed)
        );
    }

    #[test]
    fn a_body_containing_underscores_still_parses() {
        // 32 bytes of 0xff encode to 42 underscores and a final `8`.
        let body = URL_SAFE_NO_PAD.encode([0xffu8; 32]);
        assert!(body.starts_with("___"));
        let secret = format!("bella_mch_{body}");
        assert_eq!(TokenKind::from_secret(&secret), Ok(TokenKind::Machine));
    }

    #[test]
    fn errors_do_not_echo_the_input() {
        let (secret, _) = Token::generate(TokenKind::Admin).unwrap();
        let body = &secret["bella_adm_".len()..];
        for input in [
            format!("bella_{body}"),
            format!("bella_adm_{body}x"),
            format!("x{secret}"),
        ] {
            let message = TokenKind::from_secret(&input).unwrap_err().to_string();
            assert!(!message.contains(&body[..8]), "{message}");
        }
    }

    #[test]
    fn kinds_map_to_roles() {
        assert_eq!(TokenKind::for_role(Role::Admin), TokenKind::Admin);
        assert_eq!(TokenKind::for_role(Role::Developer), TokenKind::Developer);
        assert_eq!(TokenKind::for_role(Role::Viewer), TokenKind::Viewer);
        assert_eq!(TokenKind::Admin.role(), Some(Role::Admin));
        assert_eq!(TokenKind::Developer.role(), Some(Role::Developer));
        assert_eq!(TokenKind::Viewer.role(), Some(Role::Viewer));
        assert_eq!(TokenKind::Machine.role(), None);
        assert_eq!(
            serde_json::to_string(&TokenKind::Machine).unwrap(),
            "\"machine\""
        );
    }
}
