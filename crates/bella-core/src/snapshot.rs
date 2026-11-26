//! Immutable source snapshots.
//!
//! A job executes the bytes selected at submission. The manifest describes
//! the complete desired tree, so a file deleted at the origin cannot survive
//! from an older cached checkout. The snapshot id is the digest of the
//! canonical manifest: identical trees share an id regardless of branch,
//! machine or capture time, which live in [`SnapshotMeta`] instead.

use crate::ids::{Digest, SnapshotId};
use crate::time::UnixMillis;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

pub const MANIFEST_VERSION: u32 = 1;

/// One filesystem object. Paths are relative to their root, use `/` as the
/// separator and are valid UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    File {
        path: String,
        digest: Digest,
        size: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        executable: bool,
    },
    /// Preserved as a link; never silently dereferenced.
    Symlink { path: String, target: String },
    /// Only recorded for directories that would otherwise be absent.
    Dir { path: String },
}

impl Entry {
    pub fn path(&self) -> &str {
        match self {
            Entry::File { path, .. } | Entry::Symlink { path, .. } | Entry::Dir { path } => path,
        }
    }
}

/// One source root of a source set, placed at a logical mount path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootManifest {
    /// Where this root is materialized, relative to the source set root.
    /// A single-root snapshot uses `.`.
    pub mount: String,
    /// Sorted by path bytes, unique.
    pub entries: Vec<Entry>,
}

/// The identity-bearing description of a source set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Sorted by mount, unique.
    pub roots: Vec<RootManifest>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("unsupported manifest version {0}")]
    Version(u32),
    #[error("manifest has no source roots")]
    NoRoots,
    #[error("invalid mount `{mount}`: {reason}")]
    Mount { mount: String, reason: String },
    #[error("invalid path `{path}`: {reason}")]
    Path { path: String, reason: String },
    #[error("entries are not sorted and unique at `{0}`")]
    Unsorted(String),
    #[error("`{file}` is a file or link but `{child}` is inside it")]
    FileUsedAsDirectory { file: String, child: String },
    #[error("symlink `{path}` -> `{target}`: {reason}")]
    Symlink {
        path: String,
        target: String,
        reason: String,
    },
}

/// Check one relative path: non-empty, `/`-separated, no empty, `.` or `..`
/// component, no NUL, not absolute.
pub fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if path.contains('\0') {
        return Err("contains NUL".into());
    }
    if path.starts_with('/') {
        return Err("absolute path".into());
    }
    for component in path.split('/') {
        match component {
            "" => return Err("empty path component".into()),
            "." | ".." => return Err(format!("`{component}` component")),
            _ => {}
        }
    }
    Ok(())
}

/// Check a symlink target against the link's own path. The target must be
/// relative and, resolved lexically, stay inside the root.
pub fn validate_symlink(link_path: &str, target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("empty target".into());
    }
    if target.contains('\0') {
        return Err("contains NUL".into());
    }
    if target.starts_with('/')
        || target.starts_with('\\')
        || target.as_bytes().get(1) == Some(&b':')
    {
        return Err("absolute target".into());
    }
    // Depth of the directory that contains the link.
    let mut depth = link_path.split('/').count() as i64 - 1;
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err("target escapes the source root".into());
                }
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

/// Why a path cannot exist on Windows, if it cannot. Source is never renamed
/// to make it fit; an incompatible snapshot excludes Windows executors.
pub fn windows_incompatibility(path: &str) -> Option<String> {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    for component in path.split('/') {
        let illegal = |c: &char| {
            matches!(*c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || (*c as u32) < 32
        };
        if let Some(bad) = component.chars().find(illegal) {
            return Some(format!("`{component}` contains `{}`", bad.escape_default()));
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Some(format!("`{component}` ends with a dot or space"));
        }
        let stem = component.split('.').next().unwrap_or("");
        if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
            return Some(format!("`{component}` is a reserved device name"));
        }
    }
    None
}

/// Key under which two paths collide on a case-insensitive or
/// normalization-insensitive filesystem (default macOS and Windows volumes).
pub fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

/// Pairs of distinct paths that would be the same file on a
/// case-insensitive or Unicode-normalizing filesystem.
pub fn find_collisions<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut seen: BTreeMap<String, &'a str> = BTreeMap::new();
    let mut collisions = Vec::new();
    for path in paths {
        if let Some(first) = seen.insert(collision_key(path), path)
            && first != path
        {
            collisions.push((first.to_string(), path.to_string()));
            seen.insert(collision_key(path), first);
        }
    }
    collisions
}

impl RootManifest {
    /// Sort entries into canonical order.
    pub fn normalize(&mut self) {
        self.entries
            .sort_by(|a, b| a.path().as_bytes().cmp(b.path().as_bytes()));
    }
}

impl Manifest {
    pub fn new(mut roots: Vec<RootManifest>) -> Self {
        for root in &mut roots {
            root.normalize();
        }
        roots.sort_by(|a, b| a.mount.as_bytes().cmp(b.mount.as_bytes()));
        Self {
            version: MANIFEST_VERSION,
            roots,
        }
    }

    /// The exact bytes whose digest is the snapshot id.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a manifest always serializes")
    }

    pub fn id(&self) -> SnapshotId {
        SnapshotId(Digest::of_bytes(&self.canonical_bytes()))
    }

    pub fn file_count(&self) -> u64 {
        self.roots
            .iter()
            .flat_map(|r| &r.entries)
            .filter(|e| matches!(e, Entry::File { .. }))
            .count() as u64
    }

    pub fn total_bytes(&self) -> u64 {
        self.roots
            .iter()
            .flat_map(|r| &r.entries)
            .map(|e| match e {
                Entry::File { size, .. } => *size,
                _ => 0,
            })
            .sum()
    }

    /// Every distinct content digest the manifest references.
    pub fn digests(&self) -> BTreeSet<Digest> {
        self.roots
            .iter()
            .flat_map(|r| &r.entries)
            .filter_map(|e| match e {
                Entry::File { digest, .. } => Some(digest.clone()),
                _ => None,
            })
            .collect()
    }

    /// Structural validation. Run it on every manifest that crosses a trust
    /// boundary, before anything is materialized.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.version != MANIFEST_VERSION {
            return Err(ManifestError::Version(self.version));
        }
        if self.roots.is_empty() {
            return Err(ManifestError::NoRoots);
        }
        let single = self.roots.len() == 1;
        for (i, root) in self.roots.iter().enumerate() {
            let mount_err = |reason: String| ManifestError::Mount {
                mount: root.mount.clone(),
                reason,
            };
            if root.mount == "." {
                if !single {
                    return Err(mount_err(
                        "`.` is only valid for a single-root snapshot".into(),
                    ));
                }
            } else {
                validate_rel_path(&root.mount).map_err(mount_err)?;
            }
            if let Some(prev) = i.checked_sub(1).map(|p| &self.roots[p])
                && prev.mount.as_bytes() >= root.mount.as_bytes()
            {
                return Err(mount_err("mounts are not sorted and unique".into()));
            }
            for other in &self.roots {
                if other.mount != root.mount && root.mount.starts_with(&format!("{}/", other.mount))
                {
                    return Err(mount_err(format!("nested inside mount `{}`", other.mount)));
                }
            }
            root.validate_entries()?;
        }
        Ok(())
    }
}

impl RootManifest {
    fn validate_entries(&self) -> Result<(), ManifestError> {
        let mut previous: Option<&str> = None;
        // Paths that are files or links and therefore cannot contain children.
        let mut leaves: BTreeSet<&str> = BTreeSet::new();
        for entry in &self.entries {
            let path = entry.path();
            validate_rel_path(path).map_err(|reason| ManifestError::Path {
                path: path.to_string(),
                reason,
            })?;
            if previous.is_some_and(|p| p.as_bytes() >= path.as_bytes()) {
                return Err(ManifestError::Unsorted(path.to_string()));
            }
            previous = Some(path);
            match entry {
                Entry::Symlink { target, .. } => {
                    validate_symlink(path, target).map_err(|reason| ManifestError::Symlink {
                        path: path.to_string(),
                        target: target.clone(),
                        reason,
                    })?;
                    leaves.insert(path);
                }
                Entry::File { .. } => {
                    leaves.insert(path);
                }
                Entry::Dir { .. } => {}
            }
        }
        for entry in &self.entries {
            let path = entry.path();
            let mut end = 0;
            while let Some(offset) = path[end..].find('/') {
                end += offset;
                if leaves.contains(&path[..end]) {
                    return Err(ManifestError::FileUsedAsDirectory {
                        file: path[..end].to_string(),
                        child: path.to_string(),
                    });
                }
                end += 1;
            }
        }
        Ok(())
    }
}

/// How coherent a capture is. Immutable does not mean coherent: a retained
/// archive can still contain half of a multi-file edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    /// No mutation was detected during capture. Explicitly weaker.
    BestEffort,
    /// Taken from a filesystem snapshot.
    FilesystemSnapshot,
    /// Participating writers acknowledged a capture barrier.
    Coordinated,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The working tree differed from `head` at capture.
    pub dirty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootProvenance {
    pub mount: String,
    /// Origin directory name. A label; never used to locate anything.
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitProvenance>,
}

/// Facts about a capture that are not part of the snapshot's identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub consistency: Consistency,
    pub captured_at: UnixMillis,
    #[serde(default)]
    pub roots: Vec<RootProvenance>,
    /// First reason the tree cannot be materialized on Windows, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_incompatible: Option<String>,
    /// The tree needs a case-sensitive filesystem.
    #[serde(default)]
    pub case_sensitive_only: bool,
    /// The tree contains symlinks (needs symlink support on the executor).
    #[serde(default)]
    pub has_symlinks: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> Entry {
        Entry::File {
            path: path.into(),
            digest: Digest::of_bytes(path.as_bytes()),
            size: 1,
            executable: false,
        }
    }

    fn manifest(entries: Vec<Entry>) -> Manifest {
        Manifest::new(vec![RootManifest {
            mount: ".".into(),
            entries,
        }])
    }

    #[test]
    fn id_ignores_input_order_but_not_content() {
        let a = manifest(vec![file("b"), file("a")]);
        let b = manifest(vec![file("a"), file("b")]);
        assert_eq!(a.id(), b.id());
        let c = manifest(vec![file("a")]);
        assert_ne!(a.id(), c.id());
    }

    #[test]
    fn executable_bit_and_link_changes_change_the_id() {
        let plain = manifest(vec![file("run.sh")]);
        let exec = manifest(vec![Entry::File {
            path: "run.sh".into(),
            digest: Digest::of_bytes(b"run.sh"),
            size: 1,
            executable: true,
        }]);
        assert_ne!(plain.id(), exec.id());
        let link_a = manifest(vec![Entry::Symlink {
            path: "l".into(),
            target: "a".into(),
        }]);
        let link_b = manifest(vec![Entry::Symlink {
            path: "l".into(),
            target: "b".into(),
        }]);
        assert_ne!(link_a.id(), link_b.id());
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        for bad in ["../x", "a/../b", "/etc/passwd", "a//b", "a/./b", "", "a\0b"] {
            assert!(
                manifest(vec![file(bad)]).validate().is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(
            manifest(vec![file("dir/ünï côdé/new\nline")])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn escaping_symlinks_are_rejected() {
        assert!(validate_symlink("a/b/link", "../../ok").is_ok());
        assert!(validate_symlink("a/b/link", "../../../escape").is_err());
        assert!(validate_symlink("link", "/etc/passwd").is_err());
        assert!(validate_symlink("link", "C:\\Windows").is_err());
        assert!(validate_symlink("link", "sub/../../escape").is_err());
    }

    #[test]
    fn a_file_cannot_also_be_a_directory() {
        let m = manifest(vec![file("a"), file("a/b")]);
        assert!(matches!(
            m.validate(),
            Err(ManifestError::FileUsedAsDirectory { .. })
        ));
    }

    #[test]
    fn unsorted_or_duplicate_entries_are_rejected() {
        let mut m = manifest(vec![file("a"), file("b")]);
        m.roots[0].entries.swap(0, 1);
        assert!(matches!(m.validate(), Err(ManifestError::Unsorted(_))));
        let dup = Manifest {
            version: MANIFEST_VERSION,
            roots: vec![RootManifest {
                mount: ".".into(),
                entries: vec![file("a"), file("a")],
            }],
        };
        assert!(dup.validate().is_err());
    }

    #[test]
    fn windows_rules() {
        assert!(windows_incompatibility("src/aux.c").is_some());
        assert!(windows_incompatibility("a/b:c").is_some());
        assert!(windows_incompatibility("trailing./x").is_some());
        assert!(windows_incompatibility("src/auxiliary.c").is_none());
    }

    #[test]
    fn case_and_normalization_collisions() {
        let found = find_collisions([
            "README.md",
            "readme.md",
            "caf\u{e9}",
            "cafe\u{301}",
            "other",
        ]);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn multi_root_mounts() {
        let two = Manifest::new(vec![
            RootManifest {
                mount: "libs/shared".into(),
                entries: vec![file("a")],
            },
            RootManifest {
                mount: "app".into(),
                entries: vec![file("a")],
            },
        ]);
        assert!(two.validate().is_ok());
        let nested = Manifest::new(vec![
            RootManifest {
                mount: "app".into(),
                entries: vec![],
            },
            RootManifest {
                mount: "app/lib".into(),
                entries: vec![],
            },
        ]);
        assert!(nested.validate().is_err());
    }
}
