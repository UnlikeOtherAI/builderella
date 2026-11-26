//! Stable identifiers. Paths, hostnames and account names are labels and
//! never stand in for these.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::io;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("expected an id starting with `{expected}_`, got `{got}`")]
    WrongPrefix { expected: &'static str, got: String },
    #[error("malformed id `{0}`")]
    Malformed(String),
    #[error("malformed sha-256 digest `{0}`")]
    BadDigest(String),
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// A new time-ordered identifier.
            pub fn generate() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::now_v7().simple()))
            }

            /// A stable identifier derived from `seed`: the same seed always
            /// gives the same id.
            pub fn derive(seed: &[u8]) -> Self {
                let hash = hex::encode(Sha256::digest(seed));
                Self(format!("{}_{}", $prefix, &hash[..32]))
            }

            pub fn parse(s: &str) -> Result<Self, IdError> {
                let rest = s
                    .strip_prefix(concat!($prefix, "_"))
                    .ok_or_else(|| IdError::WrongPrefix { expected: $prefix, got: s.to_string() })?;
                let ok = rest.len() == 32
                    && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if ok { Ok(Self(s.to_string())) } else { Err(IdError::Malformed(s.to_string())) }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::parse(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                Self::parse(&s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

id_type!(
    /// Administrative boundary of one fleet.
    FleetId,
    "flt"
);
id_type!(
    /// One worker installation. Stable across reboots; never derived from a hostname.
    MachineId,
    "mch"
);
id_type!(
    /// One boot of a machine. Process identities from another boot are invalid.
    BootId,
    "boot"
);
id_type!(
    /// A human, automation or service identity.
    PrincipalId,
    "prn"
);
id_type!(
    /// A logical project, independent of any checkout path.
    ProjectId,
    "prj"
);
id_type!(
    /// One coding session, used for attribution and fair sharing.
    SessionId,
    "ses"
);
id_type!(
    /// User intent for one snapshot and one frozen specification.
    JobId,
    "job"
);
id_type!(
    /// One execution of a job.
    AttemptId,
    "att"
);
id_type!(
    /// A running app/services/debug context that outlives its build steps.
    EnvironmentId,
    "env"
);
id_type!(
    /// A quantitative or exclusive resource allocation.
    ReservationId,
    "rsv"
);
id_type!(
    /// One output with provenance.
    ArtifactId,
    "art"
);
id_type!(
    /// One controller-to-worker command; duplicates are acknowledged idempotently.
    CommandId,
    "cmd"
);
id_type!(
    /// A durable request to create cloud capacity.
    CloudIntentId,
    "cld"
);

/// SHA-256 content digest, lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let ok = s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if ok {
            Ok(Self(s.to_string()))
        } else {
            Err(IdError::BadDigest(s.to_string()))
        }
    }

    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// First two hex characters, used as a fan-out directory name.
    pub fn shard(&self) -> &str {
        &self.0[..2]
    }

    /// Abbreviated form for display only.
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Digest {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Digest {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        Self::parse(&s)
    }
}

impl From<Digest> for String {
    fn from(d: Digest) -> String {
        d.0
    }
}

/// Incremental SHA-256. Hash the bytes that are actually retained or sent,
/// never a second read of a mutable file.
#[derive(Default)]
pub struct DigestWriter {
    hasher: Sha256,
    len: u64,
}

impl DigestWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finish(self) -> (Digest, u64) {
        (Digest(hex::encode(self.hasher.finalize())), self.len)
    }
}

impl io::Write for DigestWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Identity of an immutable source snapshot: the digest of its canonical manifest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub Digest);

impl SnapshotId {
    pub fn digest(&self) -> &Digest {
        &self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_foreign_prefixes() {
        let job = JobId::generate();
        assert_eq!(JobId::parse(job.as_str()).unwrap(), job);
        assert!(matches!(
            AttemptId::parse(job.as_str()),
            Err(IdError::WrongPrefix { .. })
        ));
        assert!(JobId::parse("job_xyz").is_err());
        let json = serde_json::to_string(&job).unwrap();
        assert_eq!(serde_json::from_str::<JobId>(&json).unwrap(), job);
        assert!(serde_json::from_str::<JobId>("\"job_nope\"").is_err());
        assert_eq!(BootId::derive(b"x"), BootId::derive(b"x"));
        assert_ne!(BootId::derive(b"x"), BootId::derive(b"y"));
        assert!(BootId::parse(BootId::derive(b"x").as_str()).is_ok());
    }

    #[test]
    fn digest_matches_known_vector() {
        let d = Digest::of_bytes(b"abc");
        assert_eq!(
            d.as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut w = DigestWriter::new();
        w.update(b"a");
        w.update(b"bc");
        assert_eq!(w.finish(), (d, 3));
        assert!(Digest::parse("ABC").is_err());
    }
}
