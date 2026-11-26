//! Helpers shared by the integration tests.
#![allow(dead_code)]

use bella_core::ids::Digest;
use bella_core::snapshot::{Entry, Manifest, RootManifest};
use bella_core::store::{MemStore, ObjectStore, StoreError, put_bytes};
use bella_materialize::{MaterializeError, MaterializeOptions, MaterializeReport, materialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn options() -> MaterializeOptions {
    MaterializeOptions {
        max_total_bytes: 1 << 30,
        max_files: 100_000,
        threads: 4,
    }
}

/// A hand-built single-root snapshot and the store holding its objects.
#[derive(Default)]
pub struct Snap {
    pub store: MemStore,
    pub entries: Vec<Entry>,
}

impl Snap {
    pub fn file(&mut self, path: &str, bytes: &[u8]) -> Digest {
        self.add_file(path, bytes, false)
    }

    pub fn exec(&mut self, path: &str, bytes: &[u8]) -> Digest {
        self.add_file(path, bytes, true)
    }

    fn add_file(&mut self, path: &str, bytes: &[u8], executable: bool) -> Digest {
        let digest = put_bytes(&self.store, bytes).unwrap();
        self.entries.push(Entry::File {
            path: path.into(),
            digest: digest.clone(),
            size: bytes.len() as u64,
            executable,
        });
        digest
    }

    pub fn link(&mut self, path: &str, target: &str) {
        self.entries.push(Entry::Symlink {
            path: path.into(),
            target: target.into(),
        });
    }

    pub fn dir(&mut self, path: &str) {
        self.entries.push(Entry::Dir { path: path.into() });
    }

    pub fn manifest(&self) -> Manifest {
        single_root(self.entries.clone())
    }

    pub fn without_links(&self) -> Manifest {
        single_root(
            self.entries
                .iter()
                .filter(|entry| !matches!(entry, Entry::Symlink { .. }))
                .cloned()
                .collect(),
        )
    }
}

pub fn single_root(entries: Vec<Entry>) -> Manifest {
    Manifest::new(vec![RootManifest {
        mount: ".".into(),
        entries,
    }])
}

/// Materialize a manifest that contains symlinks.
///
/// Returns `None`, after printing why, when Windows refuses to create
/// symlinks for lack of privilege. In that case it also checks that the
/// failed attempt left nothing behind.
pub fn materialize_links(
    manifest: &Manifest,
    store: &dyn ObjectStore,
    dest: &Path,
) -> Option<MaterializeReport> {
    match materialize(manifest, store, dest, &options()) {
        Ok(report) => Some(report),
        Err(MaterializeError::SymlinksUnsupported { path, source }) if cfg!(windows) => {
            eprintln!("skipped: symlink assertions (cannot create `{path}`: {source})");
            assert!(!dest.exists(), "a refused symlink left a partial tree");
            None
        }
        Err(error) => panic!("materialize failed: {error}"),
    }
}

/// Every path below `root`, `/`-separated, without following links.
pub fn list_tree(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let rel = format!("{prefix}{name}");
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &format!("{rel}/"), out);
            }
            out.insert(rel);
        }
    }
    let mut out = BTreeSet::new();
    walk(root, "", &mut out);
    out
}

pub fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).unwrap().next().is_none()
}

/// Deterministic bytes that do not repeat with a short period.
pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 251) as u8).collect()
}

/// A store whose objects misbehave: each one yields `good` bytes of `0xAA`
/// and then either never ends or fails.
pub struct HostileStore {
    pub good: u64,
    pub then_fail: bool,
    /// Bytes handed out across all readers.
    pub served: Arc<AtomicU64>,
}

struct HostileReader {
    remaining_good: u64,
    then_fail: bool,
    served: Arc<AtomicU64>,
}

impl Read for HostileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = if self.then_fail {
            if self.remaining_good == 0 {
                return Err(io::Error::other("connection reset"));
            }
            (buf.len() as u64).min(self.remaining_good) as usize
        } else {
            buf.len()
        };
        buf[..len].fill(0xAA);
        self.remaining_good = self.remaining_good.saturating_sub(len as u64);
        self.served.fetch_add(len as u64, Ordering::SeqCst);
        Ok(len)
    }
}

impl ObjectStore for HostileStore {
    fn contains(&self, _digest: &Digest) -> Result<bool, StoreError> {
        Ok(true)
    }

    fn put(
        &self,
        _reader: &mut dyn Read,
        _expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        Err(StoreError::Unavailable("read-only test store".into()))
    }

    fn open(&self, _digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        Ok(Box::new(HostileReader {
            remaining_good: self.good,
            then_fail: self.then_fail,
            served: Arc::clone(&self.served),
        }))
    }

    fn size(&self, _digest: &Digest) -> Result<Option<u64>, StoreError> {
        Ok(Some(self.good))
    }
}
