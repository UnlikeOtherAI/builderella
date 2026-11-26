//! Observe a path and read a file exactly once.

use crate::enumerate::Candidate;
use bella_core::ids::{Digest, DigestWriter};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What identifies one version of a file without reading it. Two equal
/// signatures around a read mean the bytes read are one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sig {
    pub len: u64,
    modified: Option<SystemTime>,
    /// Device, inode, mode, status-change time: a rename over the path, a
    /// `chmod` or a rewrite that restores the mtime all show up here.
    #[cfg(unix)]
    inode: (u64, u64, u32, i64, i64),
    #[cfg(windows)]
    created: Option<SystemTime>,
}

impl Sig {
    fn of(meta: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            inode: (
                meta.dev(),
                meta.ino(),
                meta.mode(),
                meta.ctime(),
                meta.ctime_nsec(),
            ),
            #[cfg(windows)]
            created: meta.created().ok(),
        }
    }

    /// The executable bit, where the filesystem records one.
    pub(crate) fn executable(&self) -> Option<bool> {
        #[cfg(unix)]
        {
            Some(self.inode.2 & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

/// What is at a candidate's path right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observed {
    /// Nothing, or the path lies behind a symlinked or missing directory.
    Missing,
    File(Sig),
    /// The link's raw target.
    Symlink(PathBuf),
    Dir,
    /// A socket, FIFO or device.
    Other,
}

/// Remembers which directories of a root are real directories. A path
/// below a symlinked directory is not part of the tree: reading through
/// the link would capture files from wherever it points.
#[derive(Default)]
pub(crate) struct DirCache(HashMap<String, bool>);

impl DirCache {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    fn is_real_dir(&mut self, root: &Path, rel: &str) -> io::Result<bool> {
        if let Some(known) = self.0.get(rel) {
            return Ok(*known);
        }
        let parent_is_real = match rel.rsplit_once('/') {
            Some((parent, _)) => self.is_real_dir(root, parent)?,
            None => true,
        };
        let real = parent_is_real
            && match fs::symlink_metadata(root.join(rel)) {
                Ok(meta) => meta.file_type().is_dir(),
                Err(e) if is_absent(&e) => false,
                Err(e) => return Err(e),
            };
        self.0.insert(rel.to_string(), real);
        Ok(real)
    }
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Look at a candidate without following links.
pub(crate) fn observe(
    root: &Path,
    candidate: &Candidate,
    dirs: &mut DirCache,
) -> io::Result<Observed> {
    if let Some((parent, _)) = candidate.rel.rsplit_once('/')
        && !dirs.is_real_dir(root, parent)?
    {
        return Ok(Observed::Missing);
    }
    let meta = match fs::symlink_metadata(&candidate.abs) {
        Ok(meta) => meta,
        Err(e) if is_absent(&e) => return Ok(Observed::Missing),
        Err(e) => return Err(e),
    };
    let kind = meta.file_type();
    Ok(if kind.is_symlink() {
        Observed::Symlink(fs::read_link(&candidate.abs)?)
    } else if kind.is_dir() {
        Observed::Dir
    } else if kind.is_file() {
        Observed::File(Sig::of(&meta))
    } else {
        Observed::Other
    })
}

enum Spool {
    Memory(Vec<u8>),
    /// An unnamed temporary file, removed by the operating system when it
    /// is closed. It has no name for a concurrent capture to collide with.
    Disk(File),
}

/// The bytes of one read of a file, with their digest. These bytes, not a
/// second read of the file, are what gets stored.
pub(crate) struct Spooled {
    pub digest: Digest,
    pub len: u64,
    data: Spool,
}

pub(crate) enum SpoolError {
    /// The file disappeared before it could be opened.
    Vanished,
    /// More than the limit was read; the value is how much had been read.
    TooLarge(u64),
    Io(io::Error),
}

impl From<io::Error> for SpoolError {
    fn from(error: io::Error) -> Self {
        SpoolError::Io(error)
    }
}

const READ_BUFFER_BYTES: usize = 64 * 1024;

impl Spooled {
    /// Read `path` once, hashing while spooling. Up to `memory` bytes stay
    /// in memory; larger files go to a temporary file. `opened` runs after
    /// the file is open and before the first byte is read.
    pub(crate) fn read(
        path: &Path,
        limit: u64,
        memory: usize,
        opened: impl FnOnce(),
    ) -> Result<Self, SpoolError> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if is_absent(&e) => return Err(SpoolError::Vanished),
            Err(e) => return Err(e.into()),
        };
        opened();
        let mut hasher = DigestWriter::new();
        let mut held = Vec::new();
        let mut disk: Option<File> = None;
        let mut buffer = vec![0u8; READ_BUFFER_BYTES];
        loop {
            let n = match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            let chunk = &buffer[..n];
            hasher.update(chunk);
            if hasher.len() > limit {
                return Err(SpoolError::TooLarge(hasher.len()));
            }
            match disk.as_mut() {
                Some(disk) => disk.write_all(chunk)?,
                None if held.len() + n > memory => {
                    let mut file = tempfile::tempfile()?;
                    file.write_all(&held)?;
                    file.write_all(chunk)?;
                    held = Vec::new();
                    disk = Some(file);
                }
                None => held.extend_from_slice(chunk),
            }
        }
        let (digest, len) = hasher.finish();
        Ok(Self {
            digest,
            len,
            data: match disk {
                Some(file) => Spool::Disk(file),
                None => Spool::Memory(held),
            },
        })
    }

    /// Run `consume` over the spooled bytes from the start.
    pub(crate) fn with_reader<T>(
        &mut self,
        consume: impl FnOnce(&mut dyn Read) -> T,
    ) -> io::Result<T> {
        match &mut self.data {
            Spool::Memory(bytes) => Ok(consume(&mut bytes.as_slice())),
            Spool::Disk(file) => {
                file.seek(SeekFrom::Start(0))?;
                Ok(consume(file))
            }
        }
    }

    /// The spooled bytes, for the small files whose content decides how
    /// they are recorded.
    pub(crate) fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.len as usize);
        self.with_reader(|reader| reader.read_to_end(&mut out))??;
        Ok(out)
    }
}
