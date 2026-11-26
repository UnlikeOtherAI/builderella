//! Comparing a directory with the manifest it should hold.

use crate::plan::{Node, Tree};
use crate::platform::{Incompatibility, current_os, name_and_length_problems};
use bella_core::ids::{Digest, DigestWriter};
use bella_core::snapshot::Manifest;
use std::collections::BTreeSet;
use std::fs::{self, File, FileType};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// What kind of filesystem object a path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// A device, socket, pipe or anything else a manifest cannot describe.
    Other,
}

/// One difference between a directory and a manifest.
///
/// Paths are relative to the destination, `/`-separated and include the
/// root's mount.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TreeMismatch {
    /// The manifest does not pass validation, or holds a path too long to
    /// exist on this platform, so there is nothing to compare against.
    /// Reported alone.
    #[error("invalid manifest: {reason}")]
    InvalidManifest { reason: String },

    /// The manifest has this path and the directory does not. `.` is the
    /// destination itself.
    #[error("`{path}` is missing")]
    Missing { path: String },

    /// The directory has this path and the manifest does not. A directory
    /// that should not exist is reported once, not once per file inside it.
    #[error("`{path}` is not in the manifest")]
    Extra { path: String },

    /// The path exists as a different kind of object. A symlink where a
    /// directory or file belongs is reported here and never followed.
    #[error("`{path}` is a {actual:?}, the manifest has a {expected:?}")]
    Kind {
        path: String,
        expected: EntryKind,
        actual: EntryKind,
    },

    /// The file's length differs.
    #[error("`{path}` has {actual} bytes, the manifest declares {expected}")]
    Size {
        path: String,
        expected: u64,
        actual: u64,
    },

    /// The file has the right length and different bytes.
    #[error("`{path}` does not match its digest {expected}")]
    Content { path: String, expected: Digest },

    /// Unix only: the permission bits are not the ones `materialize` sets
    /// (`0o755` for an executable, `0o644` otherwise).
    #[error("`{path}` has mode {actual:o}, expected {expected:o}")]
    Mode {
        path: String,
        expected: u32,
        actual: u32,
    },

    /// The symlink points somewhere else.
    #[error("symlink `{path}` points to `{actual}`, the manifest says `{expected}`")]
    LinkTarget {
        path: String,
        expected: String,
        actual: String,
    },

    /// The path could not be read, so it could not be compared.
    #[error("cannot read `{path}`: {error}")]
    Unreadable { path: String, error: String },
}

impl TreeMismatch {
    fn path(&self) -> &str {
        match self {
            TreeMismatch::InvalidManifest { .. } => "",
            TreeMismatch::Missing { path }
            | TreeMismatch::Extra { path }
            | TreeMismatch::Kind { path, .. }
            | TreeMismatch::Size { path, .. }
            | TreeMismatch::Content { path, .. }
            | TreeMismatch::Mode { path, .. }
            | TreeMismatch::LinkTarget { path, .. }
            | TreeMismatch::Unreadable { path, .. } => path,
        }
    }
}

/// Check that `dest` holds exactly the tree `manifest` describes: every
/// entry present with the right kind, size, bytes, mode (Unix) and link
/// target, every implied directory present, and nothing else.
///
/// Symlinks are compared as links and never followed. Names are compared
/// exactly, so a volume that altered a name on the way in is reported. On
/// Windows the executable bit is not checked and `\` in a stored link target
/// is read as `/`.
///
/// Mismatches are returned sorted by path. Meant for manifests that
/// [`materialize`](crate::materialize) accepted: memory grows with the number
/// of entries and the directories their paths imply.
pub fn verify_tree(manifest: &Manifest, dest: &Path) -> Result<(), Vec<TreeMismatch>> {
    let invalid = |reason: String| vec![TreeMismatch::InvalidManifest { reason }];
    manifest
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    // A path too long to exist here cannot be in the tree, and refusing it
    // keeps the work below proportional to the manifest.
    if let Some(too_long) = name_and_length_problems(manifest, current_os(), 0)
        .into_iter()
        .find(|problem| {
            matches!(
                problem,
                Incompatibility::PathTooLong { .. } | Incompatibility::NameTooLong { .. }
            )
        })
    {
        return Err(invalid(too_long.to_string()));
    }
    let tree = Tree::build(manifest, u64::MAX).map_err(|error| invalid(error.to_string()))?;

    match fs::symlink_metadata(dest) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(metadata) => {
            return Err(vec![TreeMismatch::Kind {
                path: ".".to_string(),
                expected: EntryKind::Dir,
                actual: kind_of(&metadata.file_type()),
            }]);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(vec![TreeMismatch::Missing {
                path: ".".to_string(),
            }]);
        }
        Err(error) => {
            return Err(vec![TreeMismatch::Unreadable {
                path: ".".to_string(),
                error: error.to_string(),
            }]);
        }
    }

    let mut found: Vec<TreeMismatch> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    // (directory on disk, its destination-relative path with a trailing `/`)
    let mut pending: Vec<(PathBuf, String)> = vec![(dest.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let here = || {
            if prefix.is_empty() {
                ".".to_string()
            } else {
                prefix.trim_end_matches('/').to_string()
            }
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                found.push(TreeMismatch::Unreadable {
                    path: here(),
                    error: error.to_string(),
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    found.push(TreeMismatch::Unreadable {
                        path: here(),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                found.push(TreeMismatch::Extra {
                    path: format!("{prefix}{}", name.to_string_lossy()),
                });
                continue;
            };
            let rel = format!("{prefix}{name}");
            let Some((expected_path, node)) = tree.nodes.get_key_value(rel.as_str()) else {
                found.push(TreeMismatch::Extra { path: rel });
                continue;
            };
            seen.insert(expected_path.as_str());
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    found.push(TreeMismatch::Unreadable {
                        path: rel,
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            let actual = kind_of(&file_type);
            let expected = match node {
                Node::Dir => EntryKind::Dir,
                Node::File { .. } => EntryKind::File,
                Node::Link { .. } => EntryKind::Symlink,
            };
            if actual != expected {
                found.push(TreeMismatch::Kind {
                    path: rel,
                    expected,
                    actual,
                });
                continue;
            }
            match *node {
                Node::Dir => pending.push((entry.path(), format!("{rel}/"))),
                Node::File {
                    digest,
                    size,
                    executable,
                } => compare_file(&entry.path(), rel, digest, size, executable, &mut found),
                Node::Link { target, .. } => match fs::read_link(entry.path()) {
                    Ok(actual) => {
                        let actual = link_text(&actual);
                        if actual != target {
                            found.push(TreeMismatch::LinkTarget {
                                path: rel,
                                expected: target.to_string(),
                                actual,
                            });
                        }
                    }
                    Err(error) => found.push(TreeMismatch::Unreadable {
                        path: rel,
                        error: error.to_string(),
                    }),
                },
            }
        }
    }

    for path in tree.nodes.keys() {
        if !seen.contains(path.as_str()) {
            found.push(TreeMismatch::Missing { path: path.clone() });
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    // Stable, so the mismatches of one path keep the order they were found in.
    found.sort_by(|a, b| a.path().as_bytes().cmp(b.path().as_bytes()));
    Err(found)
}

fn kind_of(file_type: &FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn compare_file(
    path: &Path,
    rel: String,
    digest: &Digest,
    size: u64,
    executable: bool,
    found: &mut Vec<TreeMismatch>,
) {
    let unreadable = |error: io::Error| TreeMismatch::Unreadable {
        path: rel.clone(),
        error: error.to_string(),
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            found.push(unreadable(error));
            return;
        }
    };
    if metadata.len() != size {
        found.push(TreeMismatch::Size {
            path: rel.clone(),
            expected: size,
            actual: metadata.len(),
        });
    } else {
        match hash_file(path) {
            Ok(actual) if actual == *digest => {}
            Ok(_) => found.push(TreeMismatch::Content {
                path: rel.clone(),
                expected: digest.clone(),
            }),
            Err(error) => found.push(unreadable(error)),
        }
    }
    if let Some((expected, actual)) = mode_mismatch(&metadata, executable) {
        found.push(TreeMismatch::Mode {
            path: rel,
            expected,
            actual,
        });
    }
}

fn hash_file(path: &Path) -> io::Result<Digest> {
    let mut file = File::open(path)?;
    let mut hasher = DigestWriter::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(hasher.finish().0)
}

/// `(expected, actual)` permission bits when they differ.
#[cfg(unix)]
fn mode_mismatch(metadata: &fs::Metadata, executable: bool) -> Option<(u32, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let expected = if executable { 0o755 } else { 0o644 };
    let actual = metadata.permissions().mode() & 0o7777;
    (actual != expected).then_some((expected, actual))
}

#[cfg(not(unix))]
fn mode_mismatch(_metadata: &fs::Metadata, _executable: bool) -> Option<(u32, u32)> {
    None
}

/// A link target as the manifest would spell it.
fn link_text(target: &Path) -> String {
    let text = target.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}
