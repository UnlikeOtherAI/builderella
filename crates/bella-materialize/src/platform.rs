//! Whether a manifest's tree can exist, exactly as described, on a platform.

use bella_core::platform::Os;
use bella_core::snapshot::{Entry, Manifest, find_collisions, windows_incompatibility};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// One reason a manifest's tree cannot be represented exactly on a platform.
///
/// Paths are relative to the destination, `/`-separated and include the
/// root's mount. Nothing is ever renamed to work around these.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Incompatibility {
    /// Windows cannot hold a file with this name.
    #[error("`{path}` is not a valid Windows name: {reason}")]
    WindowsName { path: String, reason: String },

    /// Windows would read this link target differently from the origin: it
    /// contains `\` or a drive prefix, or names a reserved device.
    #[error("symlink `{path}` -> `{target}` is not portable to Windows: {reason}")]
    WindowsSymlinkTarget {
        path: String,
        target: String,
        reason: String,
    },

    /// One path component is longer than the platform allows.
    #[error("a name in `{path}` is {len} units long, the limit is {limit}")]
    NameTooLong {
        path: String,
        len: usize,
        limit: usize,
    },

    /// The whole path is longer than the platform allows. With
    /// [`check_platform`] `len` is the relative path alone; `materialize`
    /// counts the destination directory as well.
    #[error("`{path}` makes a path of {len} units, the limit is {limit}")]
    PathTooLong {
        path: String,
        len: usize,
        limit: usize,
    },

    /// Two distinct paths would be the same file on the volume.
    #[error("`{first}` and `{second}` are the same path on this volume")]
    Collision { first: String, second: String },
}

/// Path length limits of an operating system, in its native units: UTF-16
/// code units on Windows, bytes elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathLimits {
    /// Longest full path of a file or symlink.
    pub path: usize,
    /// Longest full path of a directory.
    pub dir_path: usize,
    /// Longest single name.
    pub component: usize,
}

impl PathLimits {
    /// The limits `materialize` and [`check_platform`] enforce for `os`.
    ///
    /// Windows uses the classic `MAX_PATH` rules (259 units for a file, 247
    /// for a directory) whether or not long paths are enabled on the host,
    /// because the tools that run in the tree afterwards mostly still do.
    pub const fn for_os(os: Os) -> Self {
        match os {
            Os::Windows => Self {
                path: 259,
                dir_path: 247,
                component: 255,
            },
            Os::Macos => Self {
                path: 1023,
                dir_path: 1023,
                component: 255,
            },
            Os::Linux => Self {
                path: 4095,
                dir_path: 4095,
                component: 255,
            },
        }
    }
}

/// Report everything that stops `manifest` from being materialized exactly on
/// `os`, on a volume that is or is not `case_sensitive`, without touching the
/// disk.
///
/// * Windows: names [`windows_incompatibility`] rejects (plus the reserved
///   device names it does not list), and symlink targets containing `\`, a
///   drive prefix or a device name.
/// * Every OS: names and relative paths over [`PathLimits`]. `materialize`
///   repeats the path check with the real destination, which can only add
///   findings.
/// * A case-insensitive volume: every pair
///   [`find_collisions`](bella_core::snapshot::find_collisions) reports,
///   directories included. macOS volumes ignore Unicode normalization even
///   when they are case-sensitive, so normalization pairs are reported there
///   too.
///
/// `materialize` runs the same checks for the machine it runs on. The
/// manifest should already have passed `Manifest::validate`; an invalid one
/// is examined as it is.
pub fn check_platform(
    manifest: &Manifest,
    os: Os,
    case_sensitive: bool,
) -> Result<(), Vec<Incompatibility>> {
    let mut found = name_and_length_problems(manifest, os, 0);
    found.extend(collision_problems(manifest, os, case_sensitive));
    if found.is_empty() { Ok(()) } else { Err(found) }
}

/// The operating system this binary runs on. Unix systems other than macOS
/// follow the Linux rules.
pub(crate) fn current_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// Length of `text` in the units `os` counts paths in.
fn units(os: Os, text: &str) -> usize {
    match os {
        Os::Windows => text.encode_utf16().count(),
        Os::Linux | Os::Macos => text.len(),
    }
}

/// Length of an absolute destination directory plus one separator, in the
/// units of the current platform.
pub(crate) fn base_len(dest: &Path) -> usize {
    #[cfg(windows)]
    let len = {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dest.as_os_str().encode_wide().collect();
        // A verbatim prefix opts the caller's own calls out of `MAX_PATH`;
        // the tree is still judged by the plain path other programs see.
        let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
        if wide.starts_with(&verbatim) {
            wide.len() - verbatim.len()
        } else {
            wide.len()
        }
    };
    #[cfg(not(windows))]
    let len = dest.as_os_str().as_encoded_bytes().len();
    len + 1
}

/// Name, link-target and length problems. `base` is the length of the
/// destination directory including its trailing separator, or 0 to judge
/// the relative paths alone.
pub(crate) fn name_and_length_problems(
    manifest: &Manifest,
    os: Os,
    base: usize,
) -> Vec<Incompatibility> {
    let limits = PathLimits::for_os(os);
    let windows = os == Os::Windows;
    let mut found = Vec::new();
    let mut long_dirs: BTreeSet<String> = BTreeSet::new();

    let check_names = |found: &mut Vec<Incompatibility>, full: &str, own: &str| {
        if windows && let Some(reason) = windows_name_problem(own) {
            found.push(Incompatibility::WindowsName {
                path: full.to_string(),
                reason,
            });
        }
        if let Some(len) = own
            .split('/')
            .map(|component| units(os, component))
            .find(|len| *len > limits.component)
        {
            found.push(Incompatibility::NameTooLong {
                path: full.to_string(),
                len,
                limit: limits.component,
            });
        }
    };
    let mut check_dir_len = |found: &mut Vec<Incompatibility>, dir: &str| {
        let len = base + units(os, dir);
        if len > limits.dir_path && long_dirs.insert(dir.to_string()) {
            found.push(Incompatibility::PathTooLong {
                path: dir.to_string(),
                len,
                limit: limits.dir_path,
            });
        }
    };

    for root in &manifest.roots {
        let prefix = if root.mount == "." {
            String::new()
        } else {
            check_names(&mut found, &root.mount, &root.mount);
            check_dir_len(&mut found, &root.mount);
            format!("{}/", root.mount)
        };
        for entry in &root.entries {
            let full = format!("{prefix}{}", entry.path());
            check_names(&mut found, &full, entry.path());
            if windows
                && let Entry::Symlink { target, .. } = entry
                && let Some(reason) = windows_target_problem(target)
            {
                found.push(Incompatibility::WindowsSymlinkTarget {
                    path: full.clone(),
                    target: target.clone(),
                    reason,
                });
            }
            if matches!(entry, Entry::Dir { .. }) {
                check_dir_len(&mut found, &full);
            } else {
                let len = base + units(os, &full);
                if len > limits.path {
                    found.push(Incompatibility::PathTooLong {
                        path: full.clone(),
                        len,
                        limit: limits.path,
                    });
                }
                // The deepest directory a path implies is its parent; every
                // other implied directory is shorter.
                if let Some((parent, _)) = full.rsplit_once('/') {
                    check_dir_len(&mut found, parent);
                }
            }
        }
    }
    found
}

/// Pairs of paths, implied directories and mounts included, that are one
/// path on the volume.
pub(crate) fn collision_problems(
    manifest: &Manifest,
    os: Os,
    case_sensitive: bool,
) -> Vec<Incompatibility> {
    if case_sensitive && os != Os::Macos {
        return Vec::new();
    }
    let mut paths: BTreeSet<String> = BTreeSet::new();
    let mut add = |path: &str| {
        for (index, _) in path.match_indices('/') {
            if !paths.contains(&path[..index]) {
                paths.insert(path[..index].to_string());
            }
        }
        if !paths.contains(path) {
            paths.insert(path.to_string());
        }
    };
    for root in &manifest.roots {
        if root.mount == "." {
            for entry in &root.entries {
                add(entry.path());
            }
        } else {
            add(&root.mount);
            for entry in &root.entries {
                add(&format!("{}/{}", root.mount, entry.path()));
            }
        }
    }
    let pairs = if case_sensitive {
        normalization_collisions(paths.iter().map(String::as_str))
    } else {
        find_collisions(paths.iter().map(String::as_str))
    };
    pairs
        .into_iter()
        .map(|(first, second)| Incompatibility::Collision { first, second })
        .collect()
}

/// Pairs of distinct paths that differ only in Unicode normalization.
fn normalization_collisions<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut seen: BTreeMap<String, &'a str> = BTreeMap::new();
    let mut collisions = Vec::new();
    for path in paths {
        let key: String = path.nfc().collect();
        match seen.get(&key) {
            Some(first) if *first != path => {
                collisions.push((first.to_string(), path.to_string()));
            }
            Some(_) => {}
            None => {
                seen.insert(key, path);
            }
        }
    }
    collisions
}

/// Why `path` cannot be a Windows path, if it cannot.
fn windows_name_problem(path: &str) -> Option<String> {
    windows_incompatibility(path).or_else(|| path.split('/').find_map(reserved_device_name))
}

/// Device names Windows reserves in every directory that
/// `windows_incompatibility` does not list: the console pseudo-files, port
/// number 0, superscript port digits, and any reserved name followed by
/// spaces before the extension.
fn reserved_device_name(component: &str) -> Option<String> {
    let stem = component.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        other => ["COM", "LPT"].iter().any(|port| {
            other.strip_prefix(port).is_some_and(|rest| {
                let mut chars = rest.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
                )
            })
        }),
    };
    reserved.then(|| format!("`{component}` is a reserved device name"))
}

/// Why a symlink target would not mean on Windows what it meant at the
/// origin, if it would not.
fn windows_target_problem(target: &str) -> Option<String> {
    if target.contains('\\') {
        return Some("the target contains `\\`".to_string());
    }
    if target.as_bytes().get(1) == Some(&b':') {
        return Some("the target has a drive prefix".to_string());
    }
    target
        .split('/')
        .filter(|component| !matches!(*component, "" | "." | ".."))
        .find_map(windows_name_problem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_beyond_the_core_list() {
        for name in [
            "CONIN$",
            "conout$",
            "COM0",
            "lpt0.txt",
            "COM\u{b9}",
            "LPT\u{b3}.log",
            "aux .c",
            "NUL  ",
        ] {
            assert!(windows_name_problem(name).is_some(), "{name:?}");
        }
        for name in ["COM10", "console", "com", "auxiliary.c", "lpt", "COMA"] {
            assert!(windows_name_problem(name).is_none(), "{name:?}");
        }
    }

    #[test]
    fn link_targets_windows_would_misread() {
        for target in ["a\\b", "..\\..\\x", "C:x", "sub/C:/x", "NUL", "../aux.txt"] {
            assert!(windows_target_problem(target).is_some(), "{target:?}");
        }
        for target in ["a/b", "../x y/z", "./a/", "ünï/côdé"] {
            assert!(windows_target_problem(target).is_none(), "{target:?}");
        }
    }

    #[test]
    fn normalization_pairs_are_found_without_case_folding() {
        let found = normalization_collisions(["README", "readme", "caf\u{e9}", "cafe\u{301}"]);
        assert_eq!(
            found,
            [("caf\u{e9}".to_string(), "cafe\u{301}".to_string())]
        );
    }
}
