//! The single-instance lock of a data directory.

use crate::LOCK_FILE;
use crate::core::WorkerError;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

/// Exclusive ownership of a data directory for the life of this value.
///
/// The lock is an advisory file lock on `worker.lock` (`flock` on Unix,
/// `LockFileEx` on Windows). The operating system drops it when the process
/// ends, however it ends, so a crashed daemon never blocks its successor.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Take the lock, or fail with [`WorkerError::AlreadyRunning`] when
    /// another daemon holds it.
    pub(crate) fn acquire(data_dir: &Path) -> Result<Self, WorkerError> {
        let path = data_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| WorkerError::io("open the lock file", &path, source))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(WorkerError::AlreadyRunning { path }),
            Err(TryLockError::Error(source)) => {
                Err(WorkerError::io("lock the lock file", &path, source))
            }
        }
    }
}
