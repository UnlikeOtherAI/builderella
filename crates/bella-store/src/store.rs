//! [`FsStore`]: the filesystem object store.

use crate::format::{self, BodyReader, HEADER_LEN, Header};
use bella_core::ids::DigestWriter;
use bella_core::store::{ObjectStore, StoreError};
use bella_core::{Clock, Digest, SystemClock, defaults};
use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OBJECTS_DIR: &str = "objects";
const TMP_DIR: &str = "tmp";
const LOCK_FILE: &str = "lock";
const LAYOUT_FILE: &str = "VERSION";
const LAYOUT_VERSION: &str = "1";
const TEMP_SUFFIX: &str = ".tmp";
const TEMP_NAME_ATTEMPTS: usize = 64;
const COPY_CHUNK: usize = 64 * 1024;

/// Settings for [`FsStore::open_with`].
#[derive(Clone)]
pub struct StoreOptions {
    /// Largest uncompressed object `put` and `put_compressed` accept.
    /// Defaults to [`defaults::MAX_OBJECT_BYTES`].
    pub max_object_bytes: u64,
    /// Clock used to stamp objects when they are published or re-put, and
    /// to measure their age during garbage collection. Temporary files are
    /// timestamped by the operating system as they are written, so their
    /// age is always measured against the system clock.
    pub clock: Arc<dyn Clock>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_object_bytes: defaults::MAX_OBJECT_BYTES,
            clock: Arc::new(SystemClock),
        }
    }
}

impl fmt::Debug for StoreOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreOptions")
            .field("max_object_bytes", &self.max_object_bytes)
            .finish_non_exhaustive()
    }
}

/// Object count and size on disk, from [`FsStore::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Number of published objects.
    pub objects: u64,
    /// Bytes those objects occupy as stored (compressed, headers included).
    pub stored_bytes: u64,
}

/// What one [`FsStore::gc`] pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Objects deleted: not live and at least `grace` old.
    pub objects_removed: u64,
    /// Objects kept because `is_live` returned true, on either check.
    pub objects_live: u64,
    /// Objects that are not live but were published or refreshed less than
    /// `grace` ago.
    pub objects_in_grace: u64,
    /// Temporary files deleted.
    pub temps_removed: u64,
    /// Temporary files younger than `grace`, left alone.
    pub temps_in_grace: u64,
    /// Stored bytes released by the deleted objects and temporary files.
    pub bytes_freed: u64,
    /// Entries that could not be examined or deleted. Each is logged; the
    /// pass continues past them.
    pub failed: u64,
}

/// Result of [`FsStore::verify`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verification {
    /// The object decodes to `len` bytes that hash to its digest.
    Valid { len: u64 },
    /// No object with that digest is stored.
    Missing,
    /// The stored file is damaged: bad header, bad or truncated frame,
    /// wrong length, trailing data, or content that does not match the
    /// digest.
    Corrupt { reason: String },
}

/// Content-addressed object store in a directory.
///
/// See the [crate documentation](crate) for the layout, the guarantees and
/// the platform differences. All methods take `&self`; one `FsStore` can be
/// shared between threads, and several `FsStore` values, in one process or
/// many, can use the same root at the same time.
pub struct FsStore {
    root: PathBuf,
    objects: PathBuf,
    tmp: PathBuf,
    lock_path: PathBuf,
    max_object_bytes: u64,
    clock: Arc<dyn Clock>,
    nonce: u64,
    temp_seq: AtomicU64,
    temp_collisions: AtomicU64,
}

impl fmt::Debug for FsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsStore")
            .field("root", &self.root)
            .field("max_object_bytes", &self.max_object_bytes)
            .finish_non_exhaustive()
    }
}

impl FsStore {
    /// Opens the store at `root` with default options, creating the
    /// directory and its layout if needed.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(root, StoreOptions::default())
    }

    /// Opens the store at `root`, creating `objects/` (with its 256 fan-out
    /// directories), `tmp/`, the lock file and the layout version file if
    /// they are missing.
    ///
    /// Fails with [`StoreError::Unavailable`] when the directory was written
    /// by a different layout version, and with an I/O error when the
    /// filesystem cannot take the advisory file lock the store relies on.
    pub fn open_with(root: impl AsRef<Path>, options: StoreOptions) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        let objects = root.join(OBJECTS_DIR);
        let tmp = root.join(TMP_DIR);
        fs::create_dir_all(&objects)?;
        fs::create_dir_all(&tmp)?;
        // Creating the fan-out directories up front means publishing an
        // object only ever adds one entry to one existing directory.
        for shard in 0..=u8::MAX {
            match fs::create_dir(objects.join(format!("{shard:02x}"))) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let lock_path = root.join(LOCK_FILE);
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        let store = Self {
            nonce: RandomState::new().hash_one((std::process::id(), SystemTime::now())),
            root,
            objects,
            tmp,
            lock_path,
            max_object_bytes: options.max_object_bytes,
            clock: options.clock,
            temp_seq: AtomicU64::new(0),
            temp_collisions: AtomicU64::new(0),
        };
        // Fails here, not on the first put, if the filesystem has no locks.
        drop(store.lock_shared()?);
        store.check_layout()?;
        sync_dir(&store.objects)?;
        sync_dir(&store.root)?;
        Ok(store)
    }

    /// The directory this store lives in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Largest uncompressed object this store accepts.
    pub fn max_object_bytes(&self) -> u64 {
        self.max_object_bytes
    }

    /// Stores an object that is already in the [stored format](crate::format)
    /// and returns its uncompressed length.
    ///
    /// The stream is written as received while it is decoded and hashed; it
    /// is published only if it is a well-formed stored object whose content
    /// hashes to `expected`. A content mismatch is
    /// [`StoreError::DigestMismatch`], a recorded length over the limit is
    /// [`StoreError::TooLarge`], and a malformed, truncated or over-long
    /// stream is an `InvalidData` I/O error. Nothing is published and no
    /// temporary file is left in any of those cases.
    pub fn put_compressed(
        &self,
        reader: &mut dyn Read,
        expected: &Digest,
    ) -> Result<u64, StoreError> {
        let header = Header::read_from(reader)?;
        if header.len > self.max_object_bytes {
            return Err(StoreError::TooLarge {
                limit: self.max_object_bytes,
            });
        }
        let mut temp = self.create_temp()?;
        temp.file_mut()?.write_all(&header.encode())?;

        let mut hasher = DigestWriter::new();
        {
            let tee = Tee {
                src: reader,
                sink: temp.file_mut()?,
                budget: format::max_stored_len(header.len) - HEADER_LEN as u64,
            };
            let mut body = BodyReader::new(tee, header.len)?;
            let mut buf = vec![0u8; COPY_CHUNK];
            loop {
                match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => hasher.update(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let (actual, len) = hasher.finish();
        if actual != *expected {
            return Err(StoreError::DigestMismatch {
                expected: expected.clone(),
                actual,
            });
        }
        self.publish(temp, expected, len)?;
        Ok(len)
    }

    /// Opens the stored bytes of an object, in the
    /// [stored format](crate::format), together with their length.
    ///
    /// Nothing is decoded or checked here: the bytes are meant to be sent to
    /// a receiver that verifies them, such as another store's
    /// [`put_compressed`](Self::put_compressed) or [`format::decode`].
    pub fn open_compressed(&self, digest: &Digest) -> Result<(File, u64), StoreError> {
        let file = self.open_object(digest)?;
        let len = file.metadata()?.len();
        Ok((file, len))
    }

    /// Reads one object completely and reports whether it is intact.
    ///
    /// Damage is reported as [`Verification::Corrupt`]; an `Err` means the
    /// check itself could not be carried out.
    pub fn verify(&self, digest: &Digest) -> Result<Verification, StoreError> {
        let mut reader = match self.open(digest) {
            Ok(reader) => reader,
            Err(StoreError::NotFound(_)) => return Ok(Verification::Missing),
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                return Ok(Verification::Corrupt {
                    reason: e.to_string(),
                });
            }
            Err(e) => return Err(e),
        };
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut len = 0u64;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return Ok(Verification::Valid { len }),
                Ok(n) => len += n as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    return Ok(Verification::Corrupt {
                        reason: e.to_string(),
                    });
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Iterates over the digests of all published objects, in no particular
    /// order. Temporary files and files that are not named like an object
    /// are never listed. Objects published or deleted while iterating may or
    /// may not appear.
    pub fn list(&self) -> Result<ObjectIter, StoreError> {
        Ok(ObjectIter(self.walk()?))
    }

    /// Counts the published objects and the bytes they occupy on disk.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let mut stats = StoreStats::default();
        for entry in self.walk()? {
            let (_, path) = entry?;
            if let Some(meta) = stat(&path)? {
                stats.objects += 1;
                stats.stored_bytes += meta.len();
            }
        }
        Ok(stats)
    }

    /// Deletes objects that nothing references and temporary files that
    /// were left behind.
    ///
    /// An object is deleted only if `is_live` returns false for it and it
    /// was published, or last re-put, at least `grace` ago. `is_live` is
    /// asked again immediately before each delete, together with a fresh
    /// look at the object's age, while holding the store's publish lock
    /// exclusively. A `put` that returns therefore leaves its object safe
    /// from every collector on this root until `grace` after the moment it
    /// published or refreshed the object, which is just before it returned.
    /// That is the time the caller has to make `is_live` return true.
    ///
    /// Temporary files (`tmp/*.tmp`) not written to for `grace`, by the
    /// system clock, are deleted. `grace` must be longer than the longest
    /// stall of an upload in progress, or that upload fails when it tries
    /// to publish.
    ///
    /// `is_live` may be called twice per object and must not call back into
    /// this store's `put`, `put_compressed` or `gc`.
    ///
    /// A failure on one entry is counted in [`GcReport::failed`] and logged;
    /// an `Err` is returned only when the store itself cannot be read.
    pub fn gc(
        &self,
        is_live: &dyn Fn(&Digest) -> bool,
        grace: Duration,
    ) -> Result<GcReport, StoreError> {
        let mut report = GcReport::default();
        let lock = File::open(&self.lock_path)?;

        for entry in self.walk()? {
            let outcome = entry
                .and_then(|(digest, path)| self.collect(&lock, &digest, &path, is_live, grace));
            match outcome {
                Ok(Collected::Live) => report.objects_live += 1,
                Ok(Collected::InGrace) => report.objects_in_grace += 1,
                Ok(Collected::Removed(bytes)) => {
                    report.objects_removed += 1;
                    report.bytes_freed += bytes;
                }
                Ok(Collected::Gone) => {}
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(%error, "object store gc could not process an object");
                }
            }
        }

        for entry in fs::read_dir(&self.tmp)? {
            match entry.and_then(|entry| self.collect_temp(&entry.path(), grace)) {
                Ok(Collected::Removed(bytes)) => {
                    report.temps_removed += 1;
                    report.bytes_freed += bytes;
                }
                Ok(Collected::InGrace) => report.temps_in_grace += 1,
                Ok(Collected::Live | Collected::Gone) => {}
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(%error, "object store gc could not process a temporary file");
                }
            }
        }
        Ok(report)
    }

    fn collect(
        &self,
        lock: &File,
        digest: &Digest,
        path: &Path,
        is_live: &dyn Fn(&Digest) -> bool,
        grace: Duration,
    ) -> io::Result<Collected> {
        if is_live(digest) {
            return Ok(Collected::Live);
        }
        let Some(meta) = stat(path)? else {
            return Ok(Collected::Gone);
        };
        if !self.object_is_older_than(&meta, grace)? {
            return Ok(Collected::InGrace);
        }

        // Publishing and refreshing happen under the shared lock, so from
        // here until the delete no put can make this object young again.
        let _held = ExclusiveLock::acquire(lock)?;
        let Some(meta) = stat(path)? else {
            return Ok(Collected::Gone);
        };
        if !self.object_is_older_than(&meta, grace)? {
            return Ok(Collected::InGrace);
        }
        if is_live(digest) {
            return Ok(Collected::Live);
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(Collected::Removed(meta.len())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Collected::Gone),
            Err(e) => Err(e),
        }
    }

    fn collect_temp(&self, path: &Path, grace: Duration) -> io::Result<Collected> {
        let is_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(TEMP_SUFFIX));
        if !is_temp {
            return Ok(Collected::Gone);
        }
        let Some(meta) = stat(path)? else {
            return Ok(Collected::Gone);
        };
        if !meta.is_file() {
            return Ok(Collected::Gone);
        }
        // The operating system stamped this file, so its age is measured
        // on the operating system's clock.
        if !is_older_than(&meta, SystemTime::now(), grace)? {
            return Ok(Collected::InGrace);
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(Collected::Removed(meta.len())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Collected::Gone),
            Err(e) => Err(e),
        }
    }

    /// Whether an object was published or refreshed at least `grace` ago
    /// by this store's clock, which is the clock that stamped it.
    fn object_is_older_than(&self, meta: &fs::Metadata, grace: Duration) -> io::Result<bool> {
        is_older_than(meta, self.now(), grace)
    }

    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.clock.now().0)
    }

    pub(crate) fn object_path(&self, digest: &Digest) -> PathBuf {
        self.objects.join(digest.shard()).join(digest.as_str())
    }

    fn open_object(&self, digest: &Digest) -> Result<File, StoreError> {
        match File::open(self.object_path(digest)) {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(StoreError::NotFound(digest.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn walk(&self) -> io::Result<Walk> {
        Ok(Walk {
            shards: fs::read_dir(&self.objects)?,
            current: None,
        })
    }

    pub(crate) fn lock_shared(&self) -> io::Result<SharedLock> {
        let file = File::open(&self.lock_path)?;
        retry_interrupted(|| file.lock_shared())?;
        Ok(SharedLock(file))
    }

    fn check_layout(&self) -> Result<(), StoreError> {
        let path = self.root.join(LAYOUT_FILE);
        let found = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut temp = self.create_temp()?;
                let file = temp.file_mut()?;
                file.write_all(format!("{LAYOUT_VERSION}\n").as_bytes())?;
                file.sync_all()?;
                temp.close();
                match fs::rename(temp.path(), &path) {
                    Ok(()) => temp.disarm(),
                    // Another opener may have written it first.
                    Err(e) if !path.exists() => return Err(e.into()),
                    Err(_) => {}
                }
                fs::read_to_string(&path)?
            }
            Err(e) => return Err(e.into()),
        };
        let found = found.trim();
        if found != LAYOUT_VERSION {
            return Err(StoreError::Unavailable(format!(
                "object store at {} has layout version `{found}`, this build reads version {LAYOUT_VERSION}",
                self.root.display()
            )));
        }
        Ok(())
    }

    pub(crate) fn temp_name(&self) -> String {
        let seq = self.temp_seq.fetch_add(1, Ordering::Relaxed);
        format!(
            "{:08x}-{:016x}-{seq:08x}{TEMP_SUFFIX}",
            std::process::id(),
            self.nonce
        )
    }

    /// Creates a new, empty temporary file that no other put can be using:
    /// the name is unique to this process, this store value and this call,
    /// and the file is created with `create_new`, so a leftover with the
    /// same name is skipped instead of being reused.
    pub(crate) fn create_temp(&self) -> io::Result<TempFile> {
        let mut last = None;
        for _ in 0..TEMP_NAME_ATTEMPTS {
            let path = self.tmp.join(self.temp_name());
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(TempFile {
                        path,
                        file: Some(file),
                        armed: true,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    self.temp_collisions.fetch_add(1, Ordering::Relaxed);
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no temporary file name available")))
    }

    #[cfg(test)]
    pub(crate) fn temp_collisions(&self) -> u64 {
        self.temp_collisions.load(Ordering::Relaxed)
    }

    /// Makes a complete temporary file the object `digest`, or discards it
    /// if that object is already published.
    fn publish(&self, mut temp: TempFile, digest: &Digest, len: u64) -> Result<(), StoreError> {
        let dst = self.object_path(digest);
        temp.file_mut()?.sync_all()?;

        let renamed = {
            let _lock = self.lock_shared()?;
            if self.refresh_existing(&dst, len)? {
                false
            } else {
                // The modification time is the publication time by the
                // store's own clock; gc measures age from it. It is set
                // under the lock so no collector can see an older one.
                temp.file_mut()?.set_modified(self.now())?;
                temp.close();
                self.rename_into_place(temp.path(), &dst, len)?
            }
        };
        if renamed {
            temp.disarm();
            if let Some(shard) = dst.parent() {
                sync_dir(shard)?;
            }
        }
        Ok(())
    }

    /// If `dst` already holds an object of this length, restarts its grace
    /// period and returns true. A file whose header is unreadable or records
    /// another length is damaged and is left for the caller to replace.
    fn refresh_existing(&self, dst: &Path, len: u64) -> io::Result<bool> {
        let mut file = match OpenOptions::new().read(true).write(true).open(dst) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        match Header::read_from(&mut file) {
            Ok(header) if header.len == len => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(false),
            Err(e) => return Err(e),
        }
        file.set_modified(self.now())?;
        Ok(true)
    }

    /// Renames `temp` to `dst`. Returns false when the rename was refused
    /// but `dst` already holds the object, which happens on Windows when
    /// another writer published it first and a reader has it open.
    pub(crate) fn rename_into_place(&self, temp: &Path, dst: &Path, len: u64) -> io::Result<bool> {
        let mut recreated_shard = false;
        let mut busy_retries = 0u32;
        loop {
            let error = match fs::rename(temp, dst) {
                Ok(()) => return Ok(true),
                Err(error) => error,
            };
            if header_matches(dst, len) {
                return Ok(false);
            }
            let shard = dst.parent().filter(|shard| !shard.exists());
            if let Some(shard) = shard
                && error.kind() == io::ErrorKind::NotFound
                && !recreated_shard
            {
                fs::create_dir_all(shard)?;
                sync_dir(&self.objects)?;
                recreated_shard = true;
                continue;
            }
            // A scanner or indexer can hold a freshly written file for a
            // moment on Windows; that clears by itself.
            if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied && busy_retries < 5
            {
                busy_retries += 1;
                std::thread::sleep(Duration::from_millis(20 * u64::from(busy_retries)));
                continue;
            }
            return Err(error);
        }
    }
}

impl ObjectStore for FsStore {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        Ok(stat(&self.object_path(digest))?.is_some_and(|meta| meta.is_file()))
    }

    /// Streams `reader` into a new temporary file, compressing and hashing
    /// the same bytes, then publishes it atomically. Content longer than the
    /// store's limit is [`StoreError::TooLarge`]. On any error nothing is
    /// published and the temporary file is removed.
    ///
    /// If the object is already stored, the new copy is discarded and the
    /// existing one has its grace period restarted. The existing file is
    /// judged by its header only: one with an unreadable header or another
    /// recorded length is replaced, but damage inside the frame is found by
    /// reading the object or by [`FsStore::verify`], not here.
    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        let mut temp = self.create_temp()?;
        let encoded = format::encode(reader, temp.file_mut()?, self.max_object_bytes)?;
        if let Some(expected) = expected
            && *expected != encoded.digest
        {
            return Err(StoreError::DigestMismatch {
                expected: expected.clone(),
                actual: encoded.digest,
            });
        }
        self.publish(temp, &encoded.digest, encoded.len)?;
        Ok((encoded.digest, encoded.len))
    }

    /// The reader yields exactly the recorded number of bytes and fails with
    /// `InvalidData` if the stored file is damaged in any way; see
    /// [`format::decode`]. A damaged header fails here, before any read.
    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        let file = self.open_object(digest)?;
        let (reader, _) = format::decode(file, digest)?;
        Ok(Box::new(reader))
    }

    /// Reads the length from the object's header: one open and one 16-byte
    /// read, whatever the object's size. The body is not checked.
    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError> {
        let mut file = match self.open_object(digest) {
            Ok(file) => file,
            Err(StoreError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(Header::read_from(&mut file)?.len))
    }
}

/// Iterator returned by [`FsStore::list`].
pub struct ObjectIter(Walk);

impl Iterator for ObjectIter {
    type Item = Result<Digest, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .map(|entry| entry.map(|(digest, _)| digest).map_err(StoreError::from))
    }
}

enum Collected {
    Live,
    InGrace,
    Removed(u64),
    Gone,
}

/// Walks `objects/<shard>/<digest>`, skipping everything that is not a
/// regular file named by a digest inside its own shard.
struct Walk {
    shards: fs::ReadDir,
    current: Option<([u8; 2], fs::ReadDir)>,
}

impl Walk {
    fn next_shard(&mut self) -> Option<io::Result<()>> {
        loop {
            let entry = match self.shards.next()? {
                Ok(entry) => entry,
                Err(e) => return Some(Err(e)),
            };
            let name = entry.file_name();
            let Some(&[a, b]) = name.to_str().map(str::as_bytes) else {
                continue;
            };
            let is_hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
            if !is_hex(a) || !is_hex(b) {
                continue;
            }
            let path = entry.path();
            match stat(&path) {
                Ok(Some(meta)) if meta.is_dir() => {}
                Ok(_) => continue,
                Err(e) => return Some(Err(e)),
            }
            match fs::read_dir(&path) {
                Ok(dir) => {
                    self.current = Some(([a, b], dir));
                    return Some(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

impl Iterator for Walk {
    type Item = io::Result<(Digest, PathBuf)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let Some((shard, mut dir)) = self.current.take() else {
                match self.next_shard()? {
                    Ok(()) => continue,
                    Err(e) => return Some(Err(e)),
                }
            };
            let Some(entry) = dir.next() else {
                continue;
            };
            self.current = Some((shard, dir));
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => return Some(Err(e)),
            };
            let name = entry.file_name();
            let Some(digest) = name.to_str().and_then(|name| Digest::parse(name).ok()) else {
                continue;
            };
            if digest.shard().as_bytes() != shard {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_file() => return Some(Ok((digest, entry.path()))),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// A file under `tmp/` that is deleted on drop unless it was published.
pub(crate) struct TempFile {
    path: PathBuf,
    file: Option<File>,
    armed: bool,
}

impl TempFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn file_mut(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("temporary file is already closed"))
    }

    /// Closes the handle; the file stays until dropped or disarmed.
    pub(crate) fn close(&mut self) {
        self.file = None;
    }

    /// The file now belongs to someone else and must not be deleted.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        self.file = None;
        if self.armed {
            // If this fails the file is a stale temporary for gc to remove.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Copies everything read from `src` into `sink`, and fails once more than
/// `budget` bytes have passed through.
struct Tee<'a> {
    src: &'a mut dyn Read,
    sink: &'a mut File,
    budget: u64,
}

impl Read for Tee<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.src.read(buf)?;
        if n as u64 > self.budget {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed object is larger than its recorded length allows",
            ));
        }
        self.budget -= n as u64;
        self.sink.write_all(&buf[..n])?;
        Ok(n)
    }
}

/// Shared hold on the publish lock, released on drop.
pub(crate) struct SharedLock(File);

impl Drop for SharedLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Exclusive hold on the publish lock, released on drop.
struct ExclusiveLock<'a>(&'a File);

impl<'a> ExclusiveLock<'a> {
    fn acquire(file: &'a File) -> io::Result<Self> {
        retry_interrupted(|| file.lock())?;
        Ok(Self(file))
    }
}

impl Drop for ExclusiveLock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn retry_interrupted(mut op: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        match op() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Metadata of `path`, or `None` if nothing is there. Asks the file itself
/// instead of trusting a directory listing, whose timestamps can lag on
/// Windows while a file is still open for writing.
fn stat(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the file was last modified at least `grace` before `now`. A
/// modification time after `now` counts as age zero.
fn is_older_than(meta: &fs::Metadata, now: SystemTime, grace: Duration) -> io::Result<bool> {
    let age = now.duration_since(meta.modified()?).unwrap_or_default();
    Ok(age >= grace)
}

fn header_matches(path: &Path, len: u64) -> bool {
    File::open(path)
        .and_then(|mut file| Header::read_from(&mut file))
        .is_ok_and(|header| header.len == len)
}

/// Flushes a directory's entries to disk so a completed rename survives a
/// power loss.
#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Windows offers no way to flush a directory through the standard library;
/// the rename is made durable by the filesystem's own metadata journal.
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}
