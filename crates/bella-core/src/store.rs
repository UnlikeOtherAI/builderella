//! Content-addressed object storage contract.
//!
//! Objects are immutable and named by the SHA-256 of their uncompressed
//! bytes. Compression is a transport and at-rest detail, never part of
//! identity. Implementations: `bella-store` (filesystem), the HTTP store in
//! `bella-client`, and [`MemStore`] for tests.

use crate::ids::{Digest, DigestWriter};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("object {0} not found")]
    NotFound(Digest),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("object exceeds the {limit}-byte limit")]
    TooLarge { limit: u64 },
    #[error("object store unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub trait ObjectStore: Send + Sync {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError>;

    /// Store everything `reader` yields and return the digest and length of
    /// the bytes actually retained. With `expected`, a mismatch publishes
    /// nothing. Publication is atomic: a reader never sees a partial object.
    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError>;

    /// Open an object for reading. The returned reader fails with
    /// `InvalidData` at end of stream if the content does not match `digest`.
    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError>;

    /// Uncompressed size, or `None` when absent.
    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError>;
}

pub fn put_bytes(store: &dyn ObjectStore, bytes: &[u8]) -> Result<Digest, StoreError> {
    let mut reader = bytes;
    Ok(store.put(&mut reader, None)?.0)
}

pub fn get_bytes(store: &dyn ObjectStore, digest: &Digest) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    store.open(digest)?.read_to_end(&mut out)?;
    Ok(out)
}

/// Wraps a reader and verifies the digest of what was read once the inner
/// reader reports end of stream.
pub struct VerifyingReader<R> {
    inner: R,
    expected: Digest,
    hasher: Option<DigestWriter>,
}

impl<R: Read> VerifyingReader<R> {
    pub fn new(inner: R, expected: Digest) -> Self {
        Self {
            inner,
            expected,
            hasher: Some(DigestWriter::new()),
        }
    }
}

impl<R: Read> Read for VerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = self.inner.read(buf)?;
        if n > 0 {
            if let Some(hasher) = self.hasher.as_mut() {
                hasher.update(&buf[..n]);
            }
            return Ok(n);
        }
        if let Some(hasher) = self.hasher.take() {
            let (actual, _) = hasher.finish();
            if actual != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("digest mismatch: expected {}, got {actual}", self.expected),
                ));
            }
        }
        Ok(0)
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemStore {
    objects: Mutex<BTreeMap<Digest, Vec<u8>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Overwrite an object's bytes without changing its name, to simulate
    /// corruption in tests.
    pub fn corrupt(&self, digest: &Digest, bytes: Vec<u8>) {
        self.objects.lock().unwrap().insert(digest.clone(), bytes);
    }

    pub fn remove(&self, digest: &Digest) {
        self.objects.lock().unwrap().remove(digest);
    }
}

impl ObjectStore for MemStore {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        Ok(self.objects.lock().unwrap().contains_key(digest))
    }

    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let actual = Digest::of_bytes(&bytes);
        if let Some(expected) = expected
            && *expected != actual
        {
            return Err(StoreError::DigestMismatch {
                expected: expected.clone(),
                actual,
            });
        }
        let len = bytes.len() as u64;
        self.objects.lock().unwrap().insert(actual.clone(), bytes);
        Ok((actual, len))
    }

    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        let bytes = self
            .objects
            .lock()
            .unwrap()
            .get(digest)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(digest.clone()))?;
        Ok(Box::new(VerifyingReader::new(
            io::Cursor::new(bytes),
            digest.clone(),
        )))
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .get(digest)
            .map(|b| b.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_expected_digest() {
        let store = MemStore::new();
        let digest = put_bytes(&store, b"hello").unwrap();
        assert_eq!(get_bytes(&store, &digest).unwrap(), b"hello");
        let wrong = Digest::of_bytes(b"other");
        let mut reader: &[u8] = b"hello";
        assert!(matches!(
            store.put(&mut reader, Some(&wrong)),
            Err(StoreError::DigestMismatch { .. })
        ));
        assert!(!store.contains(&wrong).unwrap());
    }

    #[test]
    fn corrupt_content_is_never_returned_as_valid() {
        let store = MemStore::new();
        let digest = put_bytes(&store, b"hello").unwrap();
        store.corrupt(&digest, b"tampered".to_vec());
        assert!(get_bytes(&store, &digest).is_err());
    }
}
