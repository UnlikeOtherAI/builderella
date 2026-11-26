//! Read-through object cache in front of the controller's object store.

use bella_core::ids::Digest;
use bella_core::store::{ObjectStore, StoreError};
use bella_core::AttemptId;
use bella_store::{FsStore, GcReport, Verification};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// A local [`FsStore`] that fetches what it lacks from the remote store and
/// keeps it.
///
/// * `open` serves from the local store when the object is there. Otherwise
///   it copies the object from the remote store into the local one, which
///   verifies the bytes against the digest while storing them, and then
///   serves the local copy. Readers verify again at end of stream.
/// * Objects pinned for an attempt are never collected. An attempt pins its
///   manifest when its offer is held and every file object before the first
///   one is fetched, and unpins when it is released.
pub(crate) struct ObjectCache {
    local: FsStore,
    remote: Arc<dyn ObjectStore>,
    pins: Mutex<BTreeMap<AttemptId, BTreeSet<Digest>>>,
}

impl ObjectCache {
    pub(crate) fn open(root: &Path, remote: Arc<dyn ObjectStore>) -> Result<Self, StoreError> {
        Ok(Self {
            local: FsStore::open(root)?,
            remote,
            pins: Mutex::new(BTreeMap::new()),
        })
    }

    fn pins(&self) -> MutexGuard<'_, BTreeMap<AttemptId, BTreeSet<Digest>>> {
        self.pins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The controller's store, for uploads.
    pub(crate) fn remote(&self) -> &dyn ObjectStore {
        self.remote.as_ref()
    }

    /// Is the object already in the local store?
    pub(crate) fn has_local(&self, digest: &Digest) -> bool {
        self.local.contains(digest).unwrap_or(false)
    }

    /// Protect `digests` from collection for as long as `attempt` is pinned.
    pub(crate) fn pin(&self, attempt: &AttemptId, digests: impl IntoIterator<Item = Digest>) {
        self.pins()
            .entry(attempt.clone())
            .or_default()
            .extend(digests);
    }

    pub(crate) fn unpin(&self, attempt: &AttemptId) {
        self.pins().remove(attempt);
    }

    fn is_pinned(&self, digest: &Digest) -> bool {
        self.pins().values().any(|set| set.contains(digest))
    }

    /// Delete local objects that no pinned attempt needs and that were
    /// fetched at least `retention` ago. Liveness is checked again
    /// immediately before each delete (see [`FsStore::gc`]).
    pub(crate) fn gc(&self, retention: Duration) -> Result<GcReport, StoreError> {
        self.local.gc(&|digest| self.is_pinned(digest), retention)
    }

    /// Drop the local copy of `digest` if it no longer verifies, so the
    /// next `open` fetches it again. Returns `true` when a damaged copy was
    /// removed.
    pub(crate) fn discard_if_corrupt(&self, digest: &Digest) -> bool {
        match self.local.verify(digest) {
            Ok(Verification::Corrupt { reason }) => {
                // Documented layout of the store: objects/<shard>/<digest>.
                let path = self
                    .local
                    .root()
                    .join("objects")
                    .join(digest.shard())
                    .join(digest.as_str());
                tracing::warn!(%digest, %reason, "removing a damaged object from the local cache");
                std::fs::remove_file(path).is_ok()
            }
            _ => false,
        }
    }

    fn fetch(&self, digest: &Digest) -> Result<(), StoreError> {
        let mut reader = self.remote.open(digest)?;
        self.local.put(&mut reader, Some(digest))?;
        Ok(())
    }
}

impl ObjectStore for ObjectCache {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        if self.local.contains(digest)? {
            return Ok(true);
        }
        self.remote.contains(digest)
    }

    /// Uploads go to the controller's store; the cache holds only what was
    /// fetched.
    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        self.remote.put(reader, expected)
    }

    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        if !self.local.contains(digest)? {
            self.fetch(digest)?;
        }
        self.local.open(digest)
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError> {
        match self.local.size(digest)? {
            Some(size) => Ok(Some(size)),
            None => self.remote.size(digest),
        }
    }
}
