//! Deleting a workspace tree.

use std::fs::{self, FileType};
use std::io;
use std::path::{Path, PathBuf};

/// Delete `path` and everything below it.
///
/// * Symlinks and Windows junctions are removed as links; nothing they point
///   at is touched or even listed.
/// * Read-only files and directories are deleted too: the read-only
///   attribute is cleared on Windows, and on Unix a directory the owner
///   cannot search or write is made accessible before it is emptied.
/// * A path that does not exist, or that disappears while the tree is being
///   removed, is not an error.
///
/// Any other failure is returned, and the tree may then be partly removed.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let file_type = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // `is_dir` is false for every kind of link, so only real directories
    // are ever descended into.
    if !file_type.is_dir() {
        return remove_leaf(path, &file_type);
    }

    // (directory, whether its contents have already been dealt with)
    let mut stack: Vec<(PathBuf, bool)> = vec![(path.to_path_buf(), false)];
    while let Some((dir, emptied)) = stack.pop() {
        if emptied {
            remove_dir(&dir)?;
            continue;
        }
        make_accessible(&dir);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if gone(&error, &dir) => continue,
            Err(error) => return Err(error),
        };
        stack.push((dir.clone(), true));
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                // The directory itself went away mid-listing; there is
                // nothing left to list.
                Err(error) if gone(&error, &dir) => break,
                Err(error) => return Err(error),
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if gone(&error, &entry.path()) => continue,
                Err(error) => return Err(error),
            };
            if file_type.is_dir() {
                stack.push((entry.path(), false));
            } else {
                remove_leaf(&entry.path(), &file_type)?;
            }
        }
    }
    Ok(())
}

/// Whether `error` only says that `path` is no longer there. Not every
/// system reports that as "not found": listing a directory that another
/// process removed in the meantime fails with "invalid argument" on some
/// macOS versions. So when the kind is something else, look again.
fn gone(error: &io::Error, path: &Path) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || matches!(
            fs::symlink_metadata(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
}

fn tolerate_missing(result: io::Result<()>, path: &Path) -> io::Result<()> {
    match result {
        Err(error) if gone(&error, path) => Ok(()),
        other => other,
    }
}

/// Remove an empty directory.
fn remove_dir(dir: &Path) -> io::Result<()> {
    match fs::remove_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            clear_read_only(dir);
            tolerate_missing(fs::remove_dir(dir), dir)
        }
        other => tolerate_missing(other, dir),
    }
}

/// Remove anything that is not a real directory: a file, a link of any kind
/// or a special file.
fn remove_leaf(path: &Path, file_type: &FileType) -> io::Result<()> {
    if is_directory_link(file_type) {
        return remove_dir(path);
    }
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            if !file_type.is_symlink() {
                clear_read_only(path);
            }
            tolerate_missing(fs::remove_file(path), path)
        }
        other => tolerate_missing(other, path),
    }
}

/// Directory symlinks and junctions on Windows are removed like directories.
#[cfg(windows)]
fn is_directory_link(file_type: &FileType) -> bool {
    use std::os::windows::fs::FileTypeExt;
    file_type.is_symlink_dir()
}

#[cfg(not(windows))]
fn is_directory_link(_file_type: &FileType) -> bool {
    false
}

/// Let the owner list and change a directory before it is emptied. Failure
/// is left for the operation that follows to report.
#[cfg(unix)]
fn make_accessible(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::symlink_metadata(dir) {
        let mode = metadata.permissions().mode();
        if metadata.is_dir() && mode & 0o700 != 0o700 {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
        }
    }
}

#[cfg(not(unix))]
fn make_accessible(_dir: &Path) {}

/// Clear the Windows read-only attribute of a file or directory that is not
/// a link. Failure is left for the retry that follows to report.
#[cfg(windows)]
fn clear_read_only(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_symlink()
    {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            // On Windows this clears one attribute bit; it grants nothing.
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

/// On Unix, deleting an entry needs write access to its directory, which
/// [`make_accessible`] has already arranged; the entry's own mode is
/// irrelevant.
#[cfg(not(windows))]
fn clear_read_only(_path: &Path) {}
