//! Writing a manifest's tree into a fresh directory.

use crate::error::MaterializeError;
use crate::plan::{Node, Tree, join_rel};
use crate::platform::{base_len, collision_problems, current_os, name_and_length_problems};
use crate::probe::is_case_sensitive;
use crate::remove::remove_tree;
use bella_core::ids::{Digest, DigestWriter};
use bella_core::snapshot::{Entry, LinkDestination, Manifest, ManifestError};
use bella_core::store::{ObjectStore, StoreError};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
/// Most threads a call will use, whatever the options ask for.
const MAX_THREADS: usize = 64;

/// Limits and parallelism for one [`materialize`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializeOptions {
    /// Largest sum of declared file sizes that will be written. Checked
    /// against the manifest before anything is created; since no file is
    /// ever written past its declared size, it also bounds the bytes on disk.
    pub max_total_bytes: u64,
    /// Largest number of filesystem objects the tree may hold. Files,
    /// symlinks and directories all count, including directories that are
    /// only implied by a path, because each one costs an inode.
    pub max_files: u64,
    /// Threads used to write files, at most 64. 0 and 1 both mean the
    /// calling thread only. The resulting tree, report and error do not
    /// depend on it.
    pub threads: usize,
}

/// What [`materialize`] created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializeReport {
    /// Regular files written.
    pub files: u64,
    /// Symlinks created.
    pub symlinks: u64,
    /// Directories created below the destination, implied ones and mounts
    /// included. The destination itself is not counted.
    pub dirs: u64,
    /// Bytes written to files.
    pub bytes: u64,
    /// Wall-clock time the call took.
    pub duration: Duration,
}

struct PlannedFile<'a> {
    rel: &'a str,
    digest: &'a Digest,
    size: u64,
    executable: bool,
}

struct PlannedLink<'a> {
    rel: &'a str,
    target: &'a str,
    /// Whether the target is a directory inside the manifest. Windows needs
    /// to know when the link is created.
    to_dir: bool,
}

/// Build the tree `manifest` describes in `dest`, reading file contents from
/// `store`.
///
/// `dest` must not exist, or be an empty real directory; missing parents are
/// created and stay if the call fails. Each root is placed at `dest/<mount>`
/// (`.` is `dest` itself).
///
/// Guarantees:
///
/// * The manifest is validated first; an invalid one writes nothing. The
///   limits in `options` are checked next, against what the manifest
///   declares.
/// * Nothing is created outside `dest`. Directories are made first, files
///   are created with `create_new` (never truncating or following an
///   existing entry) and symlinks come last. A manifest in which a path
///   passes through one of its own symlinks, or whose links resolve outside
///   their root when followed through each other, is refused.
/// * A file's object is read no further than one byte past its declared
///   size and is hashed as it is written. A missing object, a digest
///   mismatch and an object longer or shorter than declared are errors.
/// * Names the platform cannot hold, paths over [`PathLimits`], and paths
///   that collide on the volume `dest` is on are refused; nothing is
///   renamed. See [`check_platform`].
/// * On Unix files get mode `0o755` when executable and `0o644` otherwise,
///   and directories `0o755`, whatever the umask. Windows ignores the
///   executable bit.
/// * Symlinks are real symlinks holding the manifest's target. Where they
///   cannot be created the call fails with
///   [`MaterializeError::SymlinksUnsupported`].
/// * Every file is a separate copy of its object.
/// * On any error everything this call created is removed again: `dest` is
///   absent if it was absent, and empty if it was empty. The error names the
///   path that caused it. With several bad files, the one that sorts first
///   is reported, whatever the thread count.
///
/// File contents are not flushed to stable storage; a workspace does not
/// outlive a crash of its host.
///
/// [`PathLimits`]: crate::PathLimits
/// [`check_platform`]: crate::check_platform
pub fn materialize(
    manifest: &Manifest,
    store: &dyn ObjectStore,
    dest: &Path,
    options: &MaterializeOptions,
) -> Result<MaterializeReport, MaterializeError> {
    let started = Instant::now();
    let followed = manifest.validate_links().map_err(invalid_manifest)?;
    check_limits(manifest, options)?;

    // Names and lengths first: this pass is linear in the manifest and
    // bounds every path before anything is built from them.
    let os = current_os();
    let absolute = std::path::absolute(dest).map_err(|source| MaterializeError::Io {
        op: "resolve",
        path: dest.to_path_buf(),
        source,
    })?;
    let problems = name_and_length_problems(manifest, os, base_len(&absolute));
    if !problems.is_empty() {
        return Err(MaterializeError::Incompatible(problems));
    }

    let tree = Tree::build(manifest, options.max_files)?;
    let directory_links = directory_links(manifest, &followed);
    let mut dirs: Vec<&str> = Vec::new();
    let mut files: Vec<PlannedFile<'_>> = Vec::new();
    let mut links: Vec<PlannedLink<'_>> = Vec::new();
    for (rel, node) in &tree.nodes {
        // Every path the tree needs must be made of plain names here.
        if join_rel(dest, rel).is_none() {
            return Err(MaterializeError::UnsafePath { path: rel.clone() });
        }
        match *node {
            Node::Dir => dirs.push(rel),
            Node::File {
                digest,
                size,
                executable,
            } => files.push(PlannedFile {
                rel,
                digest,
                size,
                executable,
            }),
            Node::Link { target } => links.push(PlannedLink {
                rel,
                target,
                to_dir: directory_links.contains(rel.as_str()),
            }),
        }
    }

    let collisions = collision_problems(manifest, os, true);
    if !collisions.is_empty() {
        return Err(MaterializeError::Incompatible(collisions));
    }

    let created_dest = prepare_destination(dest)?;
    let written = write_tree(manifest, store, dest, options, &dirs, &files, &links);
    match written {
        Ok(()) => Ok(MaterializeReport {
            files: files.len() as u64,
            symlinks: links.len() as u64,
            dirs: dirs.len() as u64,
            bytes: files.iter().map(|file| file.size).sum(),
            duration: started.elapsed(),
        }),
        Err(error) => match remove_created(dest, created_dest) {
            Ok(()) => Err(error),
            Err(source) => Err(MaterializeError::CleanupFailed {
                error: Box::new(error),
                dest: dest.to_path_buf(),
                source,
            }),
        },
    }
}

/// Turn a validation failure into this crate's error. The two findings
/// about links followed through each other keep their own variants, with the
/// link's path made relative to the destination.
fn invalid_manifest(error: ManifestError) -> MaterializeError {
    let under = |mount: &str, path: String| {
        if mount == "." {
            path
        } else {
            format!("{mount}/{path}")
        }
    };
    match error {
        ManifestError::SymlinkEscapes {
            mount,
            path,
            target,
        } => MaterializeError::SymlinkEscapes {
            path: under(&mount, path),
            target,
        },
        ManifestError::SymlinksTooComplex { mount, path } => MaterializeError::SymlinksTooComplex {
            path: under(&mount, path),
        },
        other => MaterializeError::InvalidManifest(other),
    }
}

/// Destination-relative paths of the symlinks that lead to a directory of
/// the manifest. `followed` is what `Manifest::validate_links` returned for
/// `manifest`.
fn directory_links(manifest: &Manifest, followed: &[Vec<LinkDestination>]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (root, destinations) in manifest.roots.iter().zip(followed) {
        let links = root
            .entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Symlink { .. }));
        for (entry, destination) in links.zip(destinations) {
            if *destination == LinkDestination::Dir {
                found.insert(if root.mount == "." {
                    entry.path().to_string()
                } else {
                    format!("{}/{}", root.mount, entry.path())
                });
            }
        }
    }
    found
}

fn check_limits(manifest: &Manifest, options: &MaterializeOptions) -> Result<(), MaterializeError> {
    let entries = manifest.roots.iter().fold(0u64, |sum, root| {
        sum.saturating_add(root.entries.len() as u64)
    });
    if entries > options.max_files {
        return Err(MaterializeError::TooManyEntries {
            entries,
            limit: options.max_files,
        });
    }
    // Sizes come from the manifest, so the sum can be made to overflow.
    let bytes = manifest
        .roots
        .iter()
        .flat_map(|root| &root.entries)
        .fold(0u64, |sum, entry| match entry {
            Entry::File { size, .. } => sum.saturating_add(*size),
            _ => sum,
        });
    if bytes > options.max_total_bytes {
        return Err(MaterializeError::TooManyBytes {
            bytes,
            limit: options.max_total_bytes,
        });
    }
    Ok(())
}

/// Make sure `dest` is an empty real directory. Returns whether this call
/// created it.
fn prepare_destination(dest: &Path) -> Result<bool, MaterializeError> {
    let io_error = |op: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| MaterializeError::Io { op, path, source }
    };
    match fs::symlink_metadata(dest) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(MaterializeError::DestinationNotDirectory {
                    dest: dest.to_path_buf(),
                });
            }
            let mut entries = fs::read_dir(dest).map_err(io_error("list", dest))?;
            match entries.next() {
                None => Ok(false),
                Some(Ok(_)) => Err(MaterializeError::DestinationNotEmpty {
                    dest: dest.to_path_buf(),
                }),
                Some(Err(source)) => Err(io_error("list", dest)(source)),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dest.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(io_error("create directory", parent))?;
            }
            // Not `create_dir_all`: if something appeared here in the
            // meantime it is not ours to fill or to remove.
            fs::create_dir(dest).map_err(io_error("create directory", dest))?;
            Ok(true)
        }
        Err(source) => Err(io_error("inspect", dest)(source)),
    }
}

/// Undo a failed materialization: remove `dest` if this call created it,
/// otherwise empty it again.
fn remove_created(dest: &Path, created_dest: bool) -> io::Result<()> {
    if created_dest {
        return remove_tree(dest);
    }
    for entry in fs::read_dir(dest)? {
        remove_tree(&entry?.path())?;
    }
    Ok(())
}

fn write_tree(
    manifest: &Manifest,
    store: &dyn ObjectStore,
    dest: &Path,
    options: &MaterializeOptions,
    dirs: &[&str],
    files: &[PlannedFile<'_>],
    links: &[PlannedLink<'_>],
) -> Result<(), MaterializeError> {
    let case_sensitive = is_case_sensitive(dest).map_err(|source| MaterializeError::Io {
        op: "probe case sensitivity of",
        path: dest.to_path_buf(),
        source,
    })?;
    if !case_sensitive {
        let collisions = collision_problems(manifest, current_os(), false);
        if !collisions.is_empty() {
            return Err(MaterializeError::Incompatible(collisions));
        }
    }

    // Parents sort before their children, and `create_dir` fails on anything
    // that already exists, so two names the volume folds together can never
    // silently share a directory.
    for rel in dirs {
        let path = target_path(dest, rel)?;
        create_directory(&path).map_err(|source| MaterializeError::Io {
            op: "create directory",
            path,
            source,
        })?;
    }

    write_files(store, dest, files, options.threads)?;

    // Links last: until here the tree holds only directories and files this
    // call made, so no earlier write could have gone through a link.
    for link in links {
        let path = target_path(dest, link.rel)?;
        if let Err(source) = create_symlink(link.target, &path, link.to_dir) {
            return Err(if symlinks_unavailable(&source) {
                MaterializeError::SymlinksUnsupported {
                    path: link.rel.to_string(),
                    source,
                }
            } else {
                MaterializeError::Io {
                    op: "create symlink",
                    path,
                    source,
                }
            });
        }
    }
    Ok(())
}

fn target_path(dest: &Path, rel: &str) -> Result<std::path::PathBuf, MaterializeError> {
    join_rel(dest, rel).ok_or_else(|| MaterializeError::UnsafePath {
        path: rel.to_string(),
    })
}

#[cfg(unix)]
fn create_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new().mode(0o755).create(path)?;
    // The mode given at creation is filtered by the umask; set it outright.
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn create_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn create_symlink(target: &str, link: &Path, _to_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &str, link: &Path, to_dir: bool) -> io::Result<()> {
    // Targets containing `\` were refused earlier, so this mapping loses
    // nothing and `verify_tree` can reverse it.
    let native = target.replace('/', "\\");
    if to_dir {
        std::os::windows::fs::symlink_dir(native, link)
    } else {
        std::os::windows::fs::symlink_file(native, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &str, _link: &Path, _to_dir: bool) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Whether a failed symlink creation means "this machine or volume does not
/// do symlinks" rather than an ordinary I/O failure.
fn symlinks_unavailable(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    if cfg!(windows) {
        // ERROR_PRIVILEGE_NOT_HELD, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED
        matches!(error.raw_os_error(), Some(1314 | 1 | 50))
    } else {
        // EPERM: what Linux reports for a filesystem without symlinks.
        error.raw_os_error() == Some(1)
    }
}

enum WriteFailure {
    /// An earlier file already failed; this one was abandoned.
    Abandoned,
    Failed(MaterializeError),
}

/// Write every file, on up to `threads` threads. When files fail, the error
/// of the first one in list order is returned.
fn write_files(
    store: &dyn ObjectStore,
    dest: &Path,
    files: &[PlannedFile<'_>],
    threads: usize,
) -> Result<(), MaterializeError> {
    // Index of the earliest file known to have failed.
    let first_failed = AtomicUsize::new(usize::MAX);
    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<(usize, MaterializeError)>> = Mutex::new(Vec::new());

    // Indices are handed out in order, so when file `i` fails every earlier
    // file has already been claimed. Earlier files run to completion and
    // later ones stop, which makes the lowest failing index the same on
    // every run.
    let work = || {
        let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
        loop {
            let index = next.fetch_add(1, Ordering::SeqCst);
            if index >= files.len() || index > first_failed.load(Ordering::SeqCst) {
                break;
            }
            let abandoned = || first_failed.load(Ordering::SeqCst) < index;
            match write_file(store, dest, &files[index], &mut buffer, &abandoned) {
                Ok(()) | Err(WriteFailure::Abandoned) => {}
                Err(WriteFailure::Failed(error)) => {
                    first_failed.fetch_min(index, Ordering::SeqCst);
                    failures
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((index, error));
                }
            }
        }
    };

    let workers = threads.clamp(1, files.len().clamp(1, MAX_THREADS));
    if workers == 1 {
        work();
    } else {
        std::thread::scope(|scope| {
            for _ in 1..workers {
                scope.spawn(work);
            }
            work();
        });
    }

    let failures = failures
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    match failures.into_iter().min_by_key(|(index, _)| *index) {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

fn write_file(
    store: &dyn ObjectStore,
    dest: &Path,
    planned: &PlannedFile<'_>,
    buffer: &mut [u8],
    abandoned: &dyn Fn() -> bool,
) -> Result<(), WriteFailure> {
    let fail = |error: MaterializeError| WriteFailure::Failed(error);
    let path = target_path(dest, planned.rel).map_err(fail)?;
    let io_error = |op: &'static str| {
        let path = path.clone();
        move |source| WriteFailure::Failed(MaterializeError::Io { op, path, source })
    };
    let store_error = |source: StoreError| {
        WriteFailure::Failed(MaterializeError::Store {
            path: planned.rel.to_string(),
            digest: planned.digest.clone(),
            source,
        })
    };
    let digest_mismatch = || {
        WriteFailure::Failed(MaterializeError::DigestMismatch {
            path: planned.rel.to_string(),
            digest: planned.digest.clone(),
        })
    };

    // Open the object before creating the file, so a missing object leaves
    // nothing to clean up for this path.
    let mut reader = match store.open(planned.digest) {
        Ok(reader) => reader,
        Err(StoreError::NotFound(_)) => {
            return Err(fail(MaterializeError::MissingObject {
                path: planned.rel.to_string(),
                digest: planned.digest.clone(),
            }));
        }
        Err(StoreError::DigestMismatch { .. }) => return Err(digest_mismatch()),
        Err(source) => return Err(store_error(source)),
    };

    let mut file = create_file(&path, planned.executable).map_err(io_error("create"))?;
    let mut hasher = DigestWriter::new();
    let mut written = 0u64;
    loop {
        if abandoned() {
            return Err(WriteFailure::Abandoned);
        }
        // Ask for at most one byte beyond the declared size: enough to tell
        // that the object is longer, never enough to fill the disk.
        let room = planned.size - written;
        let want = usize::try_from(room.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = match reader.read(&mut buffer[..want]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // The store's own verification, reported at end of stream.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Err(digest_mismatch());
            }
            Err(error) => return Err(store_error(StoreError::Io(error))),
        };
        if read == 0 {
            break;
        }
        if read as u64 > room {
            return Err(fail(MaterializeError::ObjectTooLong {
                path: planned.rel.to_string(),
                digest: planned.digest.clone(),
                expected: planned.size,
            }));
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read]).map_err(io_error("write"))?;
        written += read as u64;
    }
    if written < planned.size {
        return Err(fail(MaterializeError::ObjectTooShort {
            path: planned.rel.to_string(),
            digest: planned.digest.clone(),
            expected: planned.size,
            actual: written,
        }));
    }
    let (actual, _) = hasher.finish();
    if actual != *planned.digest {
        return Err(digest_mismatch());
    }
    finish_file(&file, planned.executable).map_err(io_error("set the mode of"))?;
    Ok(())
}

#[cfg(unix)]
fn file_mode(executable: bool) -> u32 {
    if executable { 0o755 } else { 0o644 }
}

#[cfg(unix)]
fn create_file(path: &Path, executable: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(file_mode(executable))
        .open(path)
}

#[cfg(not(unix))]
fn create_file(path: &Path, _executable: bool) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Give the file its final mode. The mode passed at creation is filtered by
/// the umask, so it is set again on the open handle.
#[cfg(unix)]
fn finish_file(file: &File, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(file_mode(executable)))
}

#[cfg(not(unix))]
fn finish_file(_file: &File, _executable: bool) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a29_only_a_missing_capability_counts_as_symlinks_unsupported() {
        assert!(symlinks_unavailable(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        for ordinary in [
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::NotFound,
            io::ErrorKind::StorageFull,
        ] {
            assert!(!symlinks_unavailable(&io::Error::from(ordinary)));
        }
        if cfg!(windows) {
            // ERROR_PRIVILEGE_NOT_HELD: no symlink privilege and no Developer Mode.
            assert!(symlinks_unavailable(&io::Error::from_raw_os_error(1314)));
            // ERROR_ALREADY_EXISTS and ERROR_DISK_FULL are ordinary failures.
            assert!(!symlinks_unavailable(&io::Error::from_raw_os_error(183)));
            assert!(!symlinks_unavailable(&io::Error::from_raw_os_error(112)));
        } else {
            // EPERM is the filesystem saying it has no symlinks; EACCES and
            // EEXIST are ordinary failures.
            assert!(symlinks_unavailable(&io::Error::from_raw_os_error(1)));
            assert!(!symlinks_unavailable(&io::Error::from_raw_os_error(13)));
            assert!(!symlinks_unavailable(&io::Error::from_raw_os_error(17)));
        }
    }
}
