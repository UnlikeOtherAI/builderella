//! A few megabytes set aside for the day the disk fills up.
//!
//! A worker on a full volume still has to journal that it is stopping an
//! attempt, that cleanup started and that the resources were released.
//! SQLite cannot append to its write-ahead log with no free space, so the
//! journal keeps a file of ballast next to the database. Deleting it gives
//! those last records room.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Size of the reserve the journal keeps next to its database.
pub const RESERVE_BYTES: u64 = 4 * 1024 * 1024;

/// The ballast file of one journal.
///
/// Intended use by the worker: when a journal write fails and
/// [`JournalError::is_disk_full`](crate::JournalError::is_disk_full) is true,
/// call [`release`](ReserveFile::release), journal the stop and cleanup of
/// whatever is filling the disk, remove those workspaces, and call
/// [`ensure`](ReserveFile::ensure) again once space is back. While the
/// reserve is absent the worker should accept no new work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveFile {
    path: PathBuf,
    size: u64,
}

impl ReserveFile {
    /// A reserve of `size` bytes at `path`. Nothing is written until
    /// [`ensure`](ReserveFile::ensure).
    pub fn new(path: impl Into<PathBuf>, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
        }
    }

    /// The reserve that belongs to the database at `database`: the same
    /// file name with `.reserve` appended, in the same directory and so on
    /// the same volume.
    pub fn beside(database: &Path, size: u64) -> Self {
        let mut name = database
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_default();
        name.push(".reserve");
        Self::new(database.with_file_name(name), size)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Is the reserve on disk at its full size?
    pub fn is_present(&self) -> bool {
        fs::metadata(&self.path).is_ok_and(|meta| meta.is_file() && meta.len() == self.size)
    }

    /// Makes sure the reserve exists at its full size and is on disk.
    ///
    /// Returns `true` when it had to be written. The content is
    /// incompressible, so a compressing or deduplicating filesystem cannot
    /// make the reservation smaller than it looks. A failed write removes
    /// the partial file, so a reserve that is present is always complete.
    pub fn ensure(&self) -> io::Result<bool> {
        if self.is_present() {
            return Ok(false);
        }
        let written = self.write();
        if written.is_err() {
            // The partial file would only take the space the caller is
            // short of; the original error is what matters.
            let _ = fs::remove_file(&self.path);
        }
        written.map(|()| true)
    }

    fn write(&self) -> io::Result<()> {
        let mut file = File::create(&self.path)?;
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut block = vec![0_u8; 64 * 1024];
        let mut remaining = self.size;
        while remaining > 0 {
            for chunk in block.chunks_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
            }
            let take = usize::try_from(remaining).map_or(block.len(), |left| left.min(block.len()));
            file.write_all(&block[..take])?;
            remaining -= take as u64;
        }
        file.sync_all()
    }

    /// Deletes the reserve, freeing its space. Returns `false` when it was
    /// already gone.
    pub fn release(&self) -> io::Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserve_sits_next_to_the_database() {
        let reserve = ReserveFile::beside(Path::new("state/journal.sqlite"), 16);
        assert_eq!(reserve.path(), Path::new("state/journal.sqlite.reserve"));
    }

    #[test]
    fn ensure_release_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let reserve = ReserveFile::new(dir.path().join("j.reserve"), 100_000);
        assert!(!reserve.is_present());
        assert!(reserve.ensure().unwrap());
        assert!(reserve.is_present());
        assert_eq!(fs::metadata(reserve.path()).unwrap().len(), 100_000);
        assert!(!reserve.ensure().unwrap(), "a full reserve is left alone");

        assert!(reserve.release().unwrap());
        assert!(!reserve.is_present());
        assert!(!reserve.release().unwrap(), "releasing twice is harmless");

        assert!(reserve.ensure().unwrap());
        assert!(reserve.is_present());
    }

    #[test]
    fn a_short_reserve_is_rewritten_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let reserve = ReserveFile::new(dir.path().join("j.reserve"), 70_000);
        fs::write(reserve.path(), b"cut short").unwrap();
        assert!(!reserve.is_present());
        assert!(reserve.ensure().unwrap());
        assert_eq!(fs::metadata(reserve.path()).unwrap().len(), 70_000);
    }

    #[test]
    fn the_content_does_not_compress_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let reserve = ReserveFile::new(dir.path().join("j.reserve"), 64 * 1024);
        reserve.ensure().unwrap();
        let bytes = fs::read(reserve.path()).unwrap();
        let mut seen = [false; 256];
        for byte in &bytes {
            seen[usize::from(*byte)] = true;
        }
        assert!(seen.iter().all(|present| *present));
        assert_ne!(bytes[..4096], bytes[4096..8192]);
    }
}
