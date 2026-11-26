//! A seekable buffer that lives in memory while it is small and moves to an
//! anonymous temporary file when it grows.

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Where the bytes of a [`Spool`] currently are.
#[derive(Debug)]
enum Backing {
    Memory(Cursor<Vec<u8>>),
    File(File),
}

/// The content of a finished [`Spool`].
#[derive(Debug)]
pub(crate) enum SpoolContent {
    Memory(Vec<u8>),
    /// The anonymous temporary file, at an unspecified position.
    File(File),
}

/// Holds an object while it is hashed, encoded, uploaded or verified.
///
/// Up to `memory_limit` bytes stay in memory. A write that would go past
/// that moves everything to a temporary file with no name (it disappears
/// when the spool is dropped, or when the process dies) and continues
/// there. Reading, writing and seeking behave as on a file.
#[derive(Debug)]
pub(crate) struct Spool {
    backing: Backing,
    memory_limit: usize,
    dir: Option<PathBuf>,
}

impl Spool {
    /// An empty spool. Temporary files go to `dir`, or to the system's
    /// temporary directory when `dir` is `None`.
    pub(crate) fn new(memory_limit: usize, dir: Option<&Path>) -> Self {
        Self {
            backing: Backing::Memory(Cursor::new(Vec::new())),
            memory_limit,
            dir: dir.map(Path::to_path_buf),
        }
    }

    /// Whether the content has been moved to a file.
    #[cfg(test)]
    pub(crate) fn is_file(&self) -> bool {
        matches!(self.backing, Backing::File(_))
    }

    /// The whole content when it is still in memory.
    #[cfg(test)]
    pub(crate) fn memory(&self) -> Option<&[u8]> {
        match &self.backing {
            Backing::Memory(cursor) => Some(cursor.get_ref()),
            Backing::File(_) => None,
        }
    }

    /// Gives up the spool for its content.
    pub(crate) fn into_content(self) -> SpoolContent {
        match self.backing {
            Backing::Memory(cursor) => SpoolContent::Memory(cursor.into_inner()),
            Backing::File(file) => SpoolContent::File(file),
        }
    }

    /// Total number of bytes held. Leaves the position where it was.
    #[cfg(test)]
    pub(crate) fn len(&mut self) -> io::Result<u64> {
        match &mut self.backing {
            Backing::Memory(cursor) => Ok(cursor.get_ref().len() as u64),
            Backing::File(file) => {
                let position = file.stream_position()?;
                let end = file.seek(SeekFrom::End(0))?;
                file.seek(SeekFrom::Start(position))?;
                Ok(end)
            }
        }
    }

    /// Discards the content and returns to the start, keeping the backing.
    pub(crate) fn reset(&mut self) -> io::Result<()> {
        match &mut self.backing {
            Backing::Memory(cursor) => {
                cursor.get_mut().clear();
                cursor.set_position(0);
            }
            Backing::File(file) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
            }
        }
        Ok(())
    }

    fn roll_over(&mut self) -> io::Result<()> {
        let Backing::Memory(cursor) = &self.backing else {
            return Ok(());
        };
        let mut file = match &self.dir {
            Some(dir) => tempfile::tempfile_in(dir)?,
            None => tempfile::tempfile()?,
        };
        file.write_all(cursor.get_ref())?;
        file.seek(SeekFrom::Start(cursor.position()))?;
        self.backing = Backing::File(file);
        Ok(())
    }
}

impl Write for Spool {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Backing::Memory(cursor) = &self.backing {
            let end = cursor.position().saturating_add(buf.len() as u64);
            if end > self.memory_limit as u64 {
                self.roll_over()?;
            }
        }
        match &mut self.backing {
            Backing::Memory(cursor) => cursor.write(buf),
            Backing::File(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.backing {
            Backing::Memory(_) => Ok(()),
            Backing::File(file) => file.flush(),
        }
    }
}

impl Read for Spool {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.backing {
            Backing::Memory(cursor) => cursor.read(buf),
            Backing::File(file) => file.read(buf),
        }
    }
}

impl Seek for Spool {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match &mut self.backing {
            Backing::Memory(cursor) => cursor.seek(position),
            Backing::File(file) => file.seek(position),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 253) as u8).collect()
    }

    fn read_all(spool: &mut Spool) -> Vec<u8> {
        spool.seek(SeekFrom::Start(0)).unwrap();
        let mut out = Vec::new();
        spool.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn small_content_stays_in_memory() {
        let mut spool = Spool::new(1024, None);
        spool.write_all(&pattern(1024)).unwrap();
        assert!(!spool.is_file());
        assert_eq!(spool.len().unwrap(), 1024);
        assert_eq!(spool.memory().unwrap(), pattern(1024).as_slice());
        assert_eq!(read_all(&mut spool), pattern(1024));
    }

    #[test]
    fn content_past_the_limit_moves_to_a_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = Spool::new(1000, Some(dir.path()));
        let content = pattern(5000);
        for piece in content.chunks(333) {
            spool.write_all(piece).unwrap();
        }
        assert!(spool.is_file());
        assert!(spool.memory().is_none());
        assert_eq!(spool.len().unwrap(), 5000);
        assert_eq!(read_all(&mut spool), content);
        // The temporary file has no name in the directory.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn seeking_back_and_overwriting_works_across_the_roll_over() {
        // The object encoder writes a header, the body, then rewrites part
        // of the header.
        let mut spool = Spool::new(64, None);
        spool.write_all(&[0u8; 16]).unwrap();
        spool.write_all(&pattern(200)).unwrap();
        assert!(spool.is_file());
        let end = spool.stream_position().unwrap();
        assert_eq!(end, 216);
        spool.seek(SeekFrom::Start(8)).unwrap();
        spool.write_all(&[9u8; 8]).unwrap();
        spool.seek(SeekFrom::Start(end)).unwrap();
        spool.write_all(b"tail").unwrap();

        let all = read_all(&mut spool);
        assert_eq!(&all[..8], &[0u8; 8]);
        assert_eq!(&all[8..16], &[9u8; 8]);
        assert_eq!(&all[16..216], pattern(200).as_slice());
        assert_eq!(&all[216..], b"tail");
    }

    #[test]
    fn reset_empties_both_kinds_of_backing() {
        let mut memory = Spool::new(1024, None);
        memory.write_all(b"abc").unwrap();
        memory.reset().unwrap();
        assert_eq!(memory.len().unwrap(), 0);
        memory.write_all(b"xy").unwrap();
        assert_eq!(read_all(&mut memory), b"xy");

        let mut file = Spool::new(2, None);
        file.write_all(b"abcdef").unwrap();
        assert!(file.is_file());
        file.reset().unwrap();
        assert_eq!(file.len().unwrap(), 0);
        file.write_all(b"xy").unwrap();
        assert_eq!(read_all(&mut file), b"xy");
    }

    #[test]
    fn the_content_can_be_taken_out_of_either_backing() {
        let mut memory = Spool::new(1024, None);
        memory.write_all(b"in memory").unwrap();
        match memory.into_content() {
            SpoolContent::Memory(bytes) => assert_eq!(bytes, b"in memory"),
            SpoolContent::File(_) => panic!("expected memory"),
        }

        let mut file = Spool::new(2, None);
        file.write_all(b"shared bytes").unwrap();
        match file.into_content() {
            SpoolContent::File(mut handle) => {
                handle.seek(SeekFrom::Start(0)).unwrap();
                let mut out = String::new();
                handle.read_to_string(&mut out).unwrap();
                assert_eq!(out, "shared bytes");
            }
            SpoolContent::Memory(_) => panic!("expected a file"),
        }
    }
}
