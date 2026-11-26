//! Files that hold secrets: the fleet private key and client credentials.
//!
//! # Guarantees by platform
//!
//! **Unix.** [`write_private`] creates the file with mode `0600` in the
//! `open` call itself, so there is no moment at which another user could
//! open it, and replaces the destination atomically. [`read_private`]
//! reports whether the file it read was accessible to anyone but its owner.
//!
//! **Windows.** There are no mode bits. The file is written with the
//! access-control list it inherits from its directory, and nothing here
//! tightens or inspects that list. The protection is the per-user profile
//! (`%APPDATA%`, `%LOCALAPPDATA%`), whose default ACL excludes other
//! ordinary users. Keep secrets under the profile, or under a directory an
//! administrator has locked down. [`read_private`] says
//! [`PrivateFileMode::NotChecked`] there instead of claiming a check it did
//! not make.
//!
//! On every platform the owner, administrators, and anything running as the
//! same user can read these files.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::random::{self, RandomError};

/// Largest file [`read_private`] will read. Keys and credential files are
/// a few kilobytes; anything larger is a mistake, not a secret.
pub const MAX_PRIVATE_FILE_BYTES: u64 = 1024 * 1024;

/// How many random temporary names to try before giving up.
const TEMP_NAME_ATTEMPTS: usize = 16;

/// A private file could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum PrivateFileError {
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        /// What was being done, as a verb phrase ("create", "read", …).
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} does not name a file", path.display())]
    NoFileName { path: PathBuf },
    #[error("{} is not a regular file", path.display())]
    NotAFile { path: PathBuf },
    #[error("{} is larger than {limit} bytes and was not read", path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error(transparent)]
    Random(#[from] RandomError),
}

impl PrivateFileError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        PrivateFileError::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }

    /// Whether the failure was that the file does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, PrivateFileError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }
}

/// What [`read_private`] found out about who can access the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateFileMode {
    /// Unix: no permission bits beyond read and write for the owner.
    OwnerOnly,
    /// Unix: the file has permission bits beyond `0600`. Its contents may
    /// have been read by other users and should be treated as exposed.
    TooOpen {
        /// The permission bits found (`mode & 0o7777`).
        mode: u32,
    },
    /// Not Unix: permissions were not inspected. See the module
    /// documentation for what protects the file instead.
    NotChecked,
}

impl PrivateFileMode {
    /// True when the file was found to be accessible beyond its owner.
    /// False for [`PrivateFileMode::NotChecked`], which is the absence of a
    /// check, not a pass.
    pub fn is_too_open(self) -> bool {
        matches!(self, PrivateFileMode::TooOpen { .. })
    }
}

/// Contents of a private file together with the result of the permission
/// check. `Debug` output omits the contents.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateFile {
    pub bytes: Vec<u8>,
    pub mode: PrivateFileMode,
}

impl std::fmt::Debug for PrivateFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateFile")
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("mode", &self.mode)
            .finish()
    }
}

/// Writes `bytes` to `path` so that only the owner can read them.
///
/// * The data goes to a new temporary file in the same directory, is
///   flushed to disk, and is then renamed over `path`. A reader sees the
///   old contents or the new, never a partial file, and a crash leaves at
///   most a stray `.<name>.<random>.tmp` file with the same protection.
/// * On Unix the temporary file is created with mode `0600` and
///   `O_EXCL`, so it is never readable by others and cannot be a symlink
///   planted by someone else. Replacing an existing, more permissive file
///   leaves the new one at `0600`.
/// * Missing parent directories are created, on Unix with mode `0700`.
///   Existing directories are left as they are.
/// * On Windows the file is written normally; see the module documentation.
///
/// The rename is atomic, but whether it survives power loss depends on the
/// filesystem: the directory is synced on Unix on a best-effort basis.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), PrivateFileError> {
    write_atomic(path, bytes, Access::OwnerOnly)
}

/// Writes a file that is not secret (the certificate) with the same
/// all-or-nothing replacement as [`write_private`] and default permissions.
pub(crate) fn write_public(path: &Path, bytes: &[u8]) -> Result<(), PrivateFileError> {
    write_atomic(path, bytes, Access::Default)
}

/// Reads a private file and reports whether its permissions were tighter
/// than or equal to `0600`.
///
/// The permission bits are read from the open file handle, so they describe
/// the file whose contents are returned. A file that is
/// [`PrivateFileMode::TooOpen`] is still returned: the caller decides
/// whether to warn or refuse, and must not ignore the flag.
///
/// Fails for anything that is not a regular file and for files larger than
/// [`MAX_PRIVATE_FILE_BYTES`].
pub fn read_private(path: &Path) -> Result<PrivateFile, PrivateFileError> {
    let file = File::open(path).map_err(|e| PrivateFileError::io("open", path, e))?;
    let metadata = file
        .metadata()
        .map_err(|e| PrivateFileError::io("inspect", path, e))?;
    if !metadata.is_file() {
        return Err(PrivateFileError::NotAFile {
            path: path.to_owned(),
        });
    }
    let too_large = || PrivateFileError::TooLarge {
        path: path.to_owned(),
        limit: MAX_PRIVATE_FILE_BYTES,
    };
    if metadata.len() > MAX_PRIVATE_FILE_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // The size can change between the check and the read; the limit holds
    // either way.
    file.take(MAX_PRIVATE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| PrivateFileError::io("read", path, e))?;
    if bytes.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(too_large());
    }
    Ok(PrivateFile {
        bytes,
        mode: mode_of(&metadata),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    OwnerOnly,
    Default,
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> PrivateFileMode {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & !0o600 == 0 {
        PrivateFileMode::OwnerOnly
    } else {
        PrivateFileMode::TooOpen { mode }
    }
}

#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> PrivateFileMode {
    PrivateFileMode::NotChecked
}

fn write_atomic(path: &Path, bytes: &[u8], access: Access) -> Result<(), PrivateFileError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| PrivateFileError::NoFileName {
            path: path.to_owned(),
        })?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
        _ => PathBuf::from("."),
    };
    create_directory(&directory, access)?;

    let (temp_path, mut file) = create_temp(&directory, file_name, access)?;
    let written = write_and_sync(&mut file, bytes, access);
    drop(file);
    if let Err(source) = written {
        let _ = fs::remove_file(&temp_path);
        return Err(PrivateFileError::io("write", &temp_path, source));
    }
    if let Err(source) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(PrivateFileError::io("replace", path, source));
    }
    sync_directory(&directory);
    Ok(())
}

fn write_and_sync(file: &mut File, bytes: &[u8], access: Access) -> io::Result<()> {
    file.write_all(bytes)?;
    restrict_open_file(file, access)?;
    file.sync_all()
}

/// Creates a uniquely named file in `directory`, failing rather than
/// reusing or following anything that already has that name.
fn create_temp(
    directory: &Path,
    file_name: &OsStr,
    access: Access,
) -> Result<(PathBuf, File), PrivateFileError> {
    let mut last_error = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let mut suffix = [0u8; 8];
        random::fill(&mut suffix)?;
        let mut name = std::ffi::OsString::from(".");
        name.push(file_name);
        name.push(format!(".{}.tmp", hex::encode(suffix)));
        let temp_path = directory.join(name);
        match temp_options(access).open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some((temp_path, error));
            }
            Err(error) => return Err(PrivateFileError::io("create", &temp_path, error)),
        }
    }
    let (temp_path, error) = last_error.unwrap_or_else(|| {
        (
            directory.to_owned(),
            io::Error::other("no temporary name was tried"),
        )
    });
    Err(PrivateFileError::io("create", &temp_path, error))
}

#[cfg(unix)]
fn temp_options(access: Access) -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // The mode is applied by `open` itself; the process umask can only
    // remove bits from it.
    options.mode(match access {
        Access::OwnerOnly => 0o600,
        Access::Default => 0o644,
    });
    options
}

#[cfg(not(unix))]
fn temp_options(_access: Access) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options
}

/// Pins a private file to exactly `0600`. Creation already kept everyone
/// else out; this only restores owner access that an unusual umask removed.
#[cfg(unix)]
fn restrict_open_file(file: &File, access: Access) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match access {
        Access::OwnerOnly => file.set_permissions(fs::Permissions::from_mode(0o600)),
        Access::Default => Ok(()),
    }
}

#[cfg(not(unix))]
fn restrict_open_file(_file: &File, _access: Access) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_directory(directory: &Path, access: Access) -> Result<(), PrivateFileError> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    if access == Access::OwnerOnly {
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .map_err(|e| PrivateFileError::io("create directory", directory, e))
}

#[cfg(not(unix))]
fn create_directory(directory: &Path, _access: Access) -> Result<(), PrivateFileError> {
    fs::create_dir_all(directory)
        .map_err(|e| PrivateFileError::io("create directory", directory, e))
}

/// Asks the filesystem to persist the rename. Failure is not an error: the
/// new file is complete and in place, and some filesystems do not support
/// syncing a directory at all.
#[cfg(unix)]
fn sync_directory(directory: &Path) {
    if let Ok(handle) = File::open(directory) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn written_bytes_are_read_back_and_no_temporary_file_remains() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"first").unwrap();
        assert_eq!(read_private(&path).unwrap().bytes, b"first");
        write_private(&path, b"second, longer").unwrap();
        assert_eq!(read_private(&path).unwrap().bytes, b"second, longer");
        write_private(&path, b"").unwrap();
        assert_eq!(read_private(&path).unwrap().bytes, b"");
        assert_eq!(entries(dir.path()), ["secret.key"]);
    }

    #[test]
    fn missing_parent_directories_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("credentials.json");
        write_private(&path, b"{}").unwrap();
        assert_eq!(read_private(&path).unwrap().bytes, b"{}");
    }

    #[test]
    fn missing_files_and_non_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let missing = read_private(&dir.path().join("nope")).unwrap_err();
        assert!(missing.is_not_found(), "{missing}");
        assert!(missing.to_string().contains("nope"));

        let not_a_file = read_private(dir.path()).unwrap_err();
        assert!(!not_a_file.is_not_found());

        assert!(matches!(
            write_private(Path::new("/"), b"x"),
            Err(PrivateFileError::NoFileName { .. })
        ));
    }

    #[test]
    fn oversized_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        fs::write(&path, vec![0u8; MAX_PRIVATE_FILE_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            read_private(&path),
            Err(PrivateFileError::TooLarge { limit, .. }) if limit == MAX_PRIVATE_FILE_BYTES
        ));
        fs::write(&path, vec![0u8; MAX_PRIVATE_FILE_BYTES as usize]).unwrap();
        assert_eq!(
            read_private(&path).unwrap().bytes.len() as u64,
            MAX_PRIVATE_FILE_BYTES
        );
    }

    #[test]
    fn debug_output_omits_the_contents() {
        let file = PrivateFile {
            bytes: b"hunter2".to_vec(),
            mode: PrivateFileMode::NotChecked,
        };
        let debug = format!("{file:?}");
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("7 bytes"));
        assert!(!PrivateFileMode::NotChecked.is_too_open());
        assert!(!PrivateFileMode::OwnerOnly.is_too_open());
        assert!(PrivateFileMode::TooOpen { mode: 0o644 }.is_too_open());
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o7777
        }

        #[test]
        fn private_files_are_created_owner_only() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret.key");
            write_private(&path, b"secret").unwrap();
            assert_eq!(mode(&path), 0o600);
            let read = read_private(&path).unwrap();
            assert_eq!(read.mode, PrivateFileMode::OwnerOnly);
            assert_eq!(read.bytes, b"secret");
        }

        #[test]
        fn replacing_a_permissive_file_tightens_it() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret.key");
            fs::write(&path, b"old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            write_private(&path, b"new").unwrap();
            assert_eq!(mode(&path), 0o600);
            assert_eq!(read_private(&path).unwrap().bytes, b"new");
        }

        #[test]
        fn permissive_files_are_flagged_when_read() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret.key");
            write_private(&path, b"secret").unwrap();
            for (bits, too_open) in [
                (0o600, false),
                (0o400, false),
                (0o640, true),
                (0o604, true),
                (0o644, true),
                (0o660, true),
                (0o700, true),
            ] {
                fs::set_permissions(&path, fs::Permissions::from_mode(bits)).unwrap();
                let read = read_private(&path).unwrap();
                assert_eq!(read.bytes, b"secret");
                assert_eq!(read.mode.is_too_open(), too_open, "{bits:o}");
                if too_open {
                    assert_eq!(read.mode, PrivateFileMode::TooOpen { mode: bits });
                } else {
                    assert_eq!(read.mode, PrivateFileMode::OwnerOnly);
                }
            }
        }

        #[test]
        fn created_directories_are_owner_only_and_existing_ones_are_untouched() {
            let dir = tempfile::tempdir().unwrap();
            let existing = dir.path().join("existing");
            fs::create_dir(&existing).unwrap();
            fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();

            let path = existing.join("new").join("secret.key");
            write_private(&path, b"secret").unwrap();
            assert_eq!(mode(&existing), 0o755);
            assert_eq!(mode(&existing.join("new")), 0o700);
            assert_eq!(mode(&path), 0o600);
        }

        #[test]
        fn public_files_are_not_restricted() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("controller.crt");
            write_public(&path, b"certificate").unwrap();
            // Group and other bits depend on the umask; the owner can
            // always read and write, and nothing is executable.
            let bits = mode(&path);
            assert_eq!(bits & 0o600, 0o600);
            assert_eq!(bits & 0o111, 0);
            assert_eq!(fs::read(&path).unwrap(), b"certificate");
        }

        #[test]
        fn a_symlink_at_the_destination_is_replaced_not_followed() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("elsewhere");
            fs::write(&target, b"untouched").unwrap();
            let path = dir.path().join("secret.key");
            std::os::unix::fs::symlink(&target, &path).unwrap();

            write_private(&path, b"secret").unwrap();
            assert_eq!(fs::read(&target).unwrap(), b"untouched");
            assert!(
                !fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(&path).unwrap(), b"secret");
            assert_eq!(mode(&path), 0o600);
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn permissions_are_reported_as_not_checked_off_unix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"secret").unwrap();
        let read = read_private(&path).unwrap();
        assert_eq!(read.mode, PrivateFileMode::NotChecked);
        assert_eq!(read.bytes, b"secret");
    }
}
