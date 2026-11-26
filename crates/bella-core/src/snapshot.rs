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
use std::ops::Bound;
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
    /// Every target stays inside the root on its own, but following `path`
    /// through the other links of root `mount` ends above that root.
    #[error(
        "symlink `{path}` -> `{target}` in root `{mount}` leaves the source root when followed through other links"
    )]
    SymlinkEscapes {
        mount: String,
        path: String,
        target: String,
    },
    /// The links of root `mount` point through each other so heavily that
    /// following them was abandoned at `path`.
    #[error("the symlinks of root `{mount}` are too entangled to follow (stopped at `{path}`)")]
    SymlinksTooComplex { mount: String, path: String },
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
///
/// This looks at one link alone. A target that passes can still lead out of
/// the root through another link (`out -> a/b/up/..` with
/// `a/b/up -> ../..`); [`resolve_symlinks`] follows links through each other
/// and [`Manifest::validate`] runs both.
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

/// How many links following one symlink may pass through before it counts
/// as a loop. Matches the limit Linux applies to a path lookup.
pub const MAX_LINK_HOPS: usize = 40;

/// Steps granted for following all symlinks of a manifest, plus
/// [`SYMLINK_STEPS_PER_ENTRY`] for each of its entries. An ordinary link
/// takes one step per component of its target.
pub const SYMLINK_STEPS_BASE: u64 = 1_000_000;
/// See [`SYMLINK_STEPS_BASE`].
pub const SYMLINK_STEPS_PER_ENTRY: u64 = 64;

/// A component looked up below a path of this many bytes costs one more
/// step, so the budget bounds the bytes compared and not only the lookups.
const STEP_BYTES: u64 = 256;

/// Where a symlink leads when it is followed inside its own root, using only
/// the entries of that root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkDestination {
    /// A directory of the root, explicit or implied by a path, or the root
    /// itself.
    Dir,
    /// A regular file of the root.
    File,
    /// Nothing in the root: the link dangles, or its path runs through a
    /// file.
    Missing,
    /// More than [`MAX_LINK_HOPS`] links were passed through.
    Loop,
}

/// Why the symlinks of a root were refused by [`resolve_symlinks`]. `path` is
/// the offending link, relative to its root.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SymlinkFault {
    /// Followed through the other links of the root, the link ends above
    /// the root.
    #[error(
        "symlink `{path}` -> `{target}` leaves the source root when followed through other links"
    )]
    Escapes { path: String, target: String },
    /// The step budget ran out before it was known where `path` leads.
    #[error("the symlinks are too entangled to follow (stopped at `{path}`)")]
    TooComplex { path: String },
}

/// Follow every symlink of `root` the way a path lookup would, through the
/// other links of the same root, and report where each one leads.
///
/// The result has one item per symlink entry, in entry order. The first link
/// that ends above the root is an error naming it; a dangling link and a
/// loop are not errors, because such links can exist on disk and lead
/// nowhere outside.
///
/// Every path component looked at takes at least one step from `budget`,
/// which callers share across the roots of a manifest so that a small
/// manifest of links pointing through each other cannot buy unbounded work.
/// When it runs out the answer is [`SymlinkFault::TooComplex`].
///
/// Only the entries are consulted, never the disk. Targets are followed as
/// written: the per-link rules (relative, no NUL) are [`validate_symlink`]'s.
/// With duplicate paths the later entry is the one looked up.
pub fn resolve_symlinks(
    root: &RootManifest,
    budget: &mut u64,
) -> Result<Vec<LinkDestination>, SymlinkFault> {
    if !root
        .entries
        .iter()
        .any(|entry| matches!(entry, Entry::Symlink { .. }))
    {
        return Ok(Vec::new());
    }
    let index: BTreeMap<&str, &Entry> = root
        .entries
        .iter()
        .map(|entry| (entry.path(), entry))
        .collect();
    let mut destinations = Vec::new();
    for entry in &root.entries {
        let Entry::Symlink { path, target } = entry else {
            continue;
        };
        match follow(&index, path, target, budget) {
            Followed::To(destination) => destinations.push(destination),
            Followed::Escapes => {
                return Err(SymlinkFault::Escapes {
                    path: path.clone(),
                    target: target.clone(),
                });
            }
            Followed::OutOfBudget => return Err(SymlinkFault::TooComplex { path: path.clone() }),
        }
    }
    Ok(destinations)
}

enum Followed {
    To(LinkDestination),
    Escapes,
    OutOfBudget,
}

/// What a root holds at one path.
enum Found<'a> {
    Dir,
    File,
    Link(&'a str),
}

/// Look `path` up among the entries. A path that is no entry but has
/// entries below it is a directory. `path` is borrowed mutably only to
/// append a separator for that search; it is unchanged on return.
fn find<'a>(index: &BTreeMap<&'a str, &'a Entry>, path: &mut String) -> Option<Found<'a>> {
    match index.get(path.as_str()) {
        Some(Entry::Dir { .. }) => return Some(Found::Dir),
        Some(Entry::File { .. }) => return Some(Found::File),
        Some(Entry::Symlink { target, .. }) => return Some(Found::Link(target)),
        None => {}
    }
    path.push('/');
    let implied = index
        .range::<str, _>((Bound::Included(path.as_str()), Bound::Unbounded))
        .next()
        .is_some_and(|(below, _)| below.starts_with(path.as_str()));
    path.pop();
    implied.then_some(Found::Dir)
}

fn follow(index: &BTreeMap<&str, &Entry>, link: &str, target: &str, budget: &mut u64) -> Followed {
    // The directory being looked in, as a path of the root; empty for the
    // root itself.
    let mut current = match link.rfind('/') {
        Some(end) => link[..end].to_string(),
        None => String::new(),
    };
    // Components still to walk; a link met on the way puts its own target
    // on top.
    let mut pending = vec![target.split('/')];
    let mut hops = 0;
    loop {
        let Some(components) = pending.last_mut() else {
            return Followed::To(LinkDestination::Dir);
        };
        let Some(component) = components.next() else {
            pending.pop();
            continue;
        };
        let cost = 1 + (current.len() + component.len()) as u64 / STEP_BYTES;
        if *budget < cost {
            *budget = 0;
            return Followed::OutOfBudget;
        }
        *budget -= cost;
        match component {
            "" | "." => {}
            ".." => {
                if current.is_empty() {
                    return Followed::Escapes;
                }
                current.truncate(current.rfind('/').unwrap_or(0));
            }
            name => {
                let parent_len = current.len();
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(name);
                match find(index, &mut current) {
                    None => return Followed::To(LinkDestination::Missing),
                    Some(Found::Dir) => {}
                    Some(Found::File) => {
                        let ends_here = pending
                            .iter()
                            .all(|rest| rest.clone().all(|part| matches!(part, "" | ".")));
                        return Followed::To(if ends_here {
                            LinkDestination::File
                        } else {
                            LinkDestination::Missing
                        });
                    }
                    Some(Found::Link(target)) => {
                        hops += 1;
                        if hops > MAX_LINK_HOPS {
                            return Followed::To(LinkDestination::Loop);
                        }
                        current.truncate(parent_len);
                        pending.push(target.split('/'));
                    }
                }
            }
        }
    }
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

    /// The step budget [`validate`](Self::validate) grants for following this
    /// manifest's symlinks: [`SYMLINK_STEPS_BASE`] plus
    /// [`SYMLINK_STEPS_PER_ENTRY`] per entry.
    pub fn symlink_step_budget(&self) -> u64 {
        let entries = self.roots.iter().fold(0u64, |sum, root| {
            sum.saturating_add(root.entries.len() as u64)
        });
        SYMLINK_STEPS_BASE.saturating_add(SYMLINK_STEPS_PER_ENTRY.saturating_mul(entries))
    }

    /// Validation. Run it on every manifest that crosses a trust boundary,
    /// before anything is materialized.
    ///
    /// Checks the structure (version, mounts, paths, order, no path through a
    /// file or link), each symlink target on its own ([`validate_symlink`]),
    /// and then follows the links of every root through each other
    /// ([`resolve_symlinks`]) so that a chain of individually harmless links
    /// cannot lead above its root.
    pub fn validate(&self) -> Result<(), ManifestError> {
        self.validate_links().map(|_| ())
    }

    /// [`validate`](Self::validate), returning what following the symlinks
    /// found: for each root, in order, where each of its symlink entries
    /// leads, in entry order.
    pub fn validate_links(&self) -> Result<Vec<Vec<LinkDestination>>, ManifestError> {
        if self.version != MANIFEST_VERSION {
            return Err(ManifestError::Version(self.version));
        }
        if self.roots.is_empty() {
            return Err(ManifestError::NoRoots);
        }
        let single = self.roots.len() == 1;
        let mut budget = self.symlink_step_budget();
        let mut destinations = Vec::with_capacity(self.roots.len());
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
            let followed = resolve_symlinks(root, &mut budget).map_err(|fault| match fault {
                SymlinkFault::Escapes { path, target } => ManifestError::SymlinkEscapes {
                    mount: root.mount.clone(),
                    path,
                    target,
                },
                SymlinkFault::TooComplex { path } => ManifestError::SymlinksTooComplex {
                    mount: root.mount.clone(),
                    path,
                },
            })?;
            destinations.push(followed);
        }
        Ok(destinations)
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

    fn link(path: &str, target: &str) -> Entry {
        Entry::Symlink {
            path: path.into(),
            target: target.into(),
        }
    }

    /// One root, entries as given (not sorted, not validated).
    fn root(entries: Vec<Entry>) -> RootManifest {
        RootManifest {
            mount: ".".into(),
            entries,
        }
    }

    /// Where each named link of `root` leads.
    fn followed(root: &RootManifest) -> BTreeMap<String, LinkDestination> {
        let destinations = resolve_symlinks(root, &mut 10_000).unwrap();
        root.entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Symlink { .. }))
            .map(|entry| entry.path().to_string())
            .zip(destinations)
            .collect()
    }

    #[test]
    fn links_are_followed_inside_the_root() {
        let root = root(vec![
            file("a/b/file"),
            link("a/b/up2", "../.."),
            link("to_file", "a/b/file"),
            link("to_dir", "a/b"),
            link("to_dir_slash", "a/b/"),
            link("via_link", "to_dir/file"),
            link("dangling", "nope/x"),
            link("through_file", "a/b/file/x"),
            link("above_file", "a/b/file/.."),
            link("loop_a", "loop_b"),
            link("loop_b", "loop_a"),
            link("root", "a/.."),
            link("self_dir", "."),
            Entry::Dir {
                path: "empty".into(),
            },
            link("to_empty", "empty"),
            // `a.b` sorts between `a` and `a/…`; it must not hide that `a`
            // is a directory.
            file("a.b"),
            link("to_a", "a"),
        ]);
        let found = followed(&root);
        let expect = [
            ("to_file", LinkDestination::File),
            ("to_dir", LinkDestination::Dir),
            ("to_dir_slash", LinkDestination::Dir),
            ("via_link", LinkDestination::File),
            ("dangling", LinkDestination::Missing),
            ("through_file", LinkDestination::Missing),
            ("above_file", LinkDestination::Missing),
            ("a/b/up2", LinkDestination::Dir),
            ("loop_a", LinkDestination::Loop),
            ("loop_b", LinkDestination::Loop),
            ("root", LinkDestination::Dir),
            ("self_dir", LinkDestination::Dir),
            ("to_empty", LinkDestination::Dir),
            ("to_a", LinkDestination::Dir),
        ];
        for (path, destination) in expect {
            assert_eq!(found[path], destination, "{path}");
        }
        assert_eq!(found.len(), expect.len());
    }

    #[test]
    fn a_chain_of_links_that_ends_above_the_root_is_named() {
        // Each target stays inside the root on its own; `out` goes through
        // `a/b/up` (the root) and then one level higher.
        let entries = vec![
            file("a/b/f"),
            link("a/b/up", "../.."),
            link("out", "a/b/up/.."),
        ];
        for entry in &entries {
            if let Entry::Symlink { path, target } = entry {
                assert!(validate_symlink(path, target).is_ok(), "{path}");
            }
        }
        assert_eq!(
            resolve_symlinks(&root(entries.clone()), &mut 10_000),
            Err(SymlinkFault::Escapes {
                path: "out".into(),
                target: "a/b/up/..".into()
            })
        );
        let error = manifest(entries).validate().unwrap_err();
        assert_eq!(
            error,
            ManifestError::SymlinkEscapes {
                mount: ".".into(),
                path: "out".into(),
                target: "a/b/up/..".into()
            }
        );
        assert!(error.to_string().contains("`out`"), "{error}");

        // Without the second link the same tree is fine.
        let fine = manifest(vec![file("a/b/f"), link("a/b/up", "../..")]);
        assert_eq!(fine.validate_links(), Ok(vec![vec![LinkDestination::Dir]]));
    }

    #[test]
    fn a_link_is_followed_inside_its_own_root_only() {
        // `app/up` reaches the top of `app` and may not go on to `lib`.
        let two = Manifest::new(vec![
            RootManifest {
                mount: "app".into(),
                entries: vec![
                    file("sub/x"),
                    link("sub/top", ".."),
                    link("up", "sub/top/../lib/y"),
                ],
            },
            RootManifest {
                mount: "lib".into(),
                entries: vec![file("y")],
            },
        ]);
        assert_eq!(
            two.validate(),
            Err(ManifestError::SymlinkEscapes {
                mount: "app".into(),
                path: "up".into(),
                target: "sub/top/../lib/y".into()
            })
        );
        // The resolver reaches the lexical answer on its own as well.
        assert_eq!(
            resolve_symlinks(
                &root(vec![file("sub/x"), link("up", "sub/../..")]),
                &mut 100
            ),
            Err(SymlinkFault::Escapes {
                path: "up".into(),
                target: "sub/../..".into()
            })
        );
        // Destinations come back per root, in root and entry order.
        let ok = Manifest::new(vec![
            RootManifest {
                mount: "app".into(),
                entries: vec![file("x"), link("to_x", "x")],
            },
            RootManifest {
                mount: "lib".into(),
                entries: vec![file("d/y"), link("to_d", "d"), link("gone", "x")],
            },
        ]);
        assert_eq!(
            ok.validate_links(),
            Ok(vec![
                vec![LinkDestination::File],
                // `gone` sorts before `to_d`; `x` exists only in `app`.
                vec![LinkDestination::Missing, LinkDestination::Dir],
            ])
        );
    }

    #[test]
    fn following_links_stops_when_the_budget_is_spent() {
        // 30 links, each going through the previous one twice.
        let mut entries = vec![file("base/f"), link("l00", "base")];
        for index in 1..30 {
            entries.push(link(
                &format!("l{index:02}"),
                &format!("l{:02}/../l{:02}", index - 1, index - 1),
            ));
        }
        let mut budget = 50;
        assert_eq!(
            resolve_symlinks(&root(entries.clone()), &mut budget),
            Err(SymlinkFault::TooComplex { path: "l04".into() })
        );
        assert_eq!(budget, 0);
        // Each link costs twice the one before plus three steps: 1, 5, 13,
        // 29, then 61. The first three alone fit.
        let mut budget = 50;
        entries.truncate(4);
        assert_eq!(
            resolve_symlinks(&root(entries), &mut budget),
            Ok(vec![LinkDestination::Dir; 3])
        );
        assert_eq!(budget, 50 - 19);
    }

    #[test]
    fn long_paths_cost_more_steps() {
        // One lookup below a directory path of over 1000 bytes.
        let deep = vec!["d".repeat(100); 12].join("/");
        let entries = vec![file(&format!("{deep}/f")), link(&format!("{deep}/l"), "f")];
        let mut budget = 100;
        resolve_symlinks(&root(entries), &mut budget).unwrap();
        assert!(budget < 99, "{budget}");
        // The same lookup at the top of the root is one step.
        let mut budget = 100;
        resolve_symlinks(&root(vec![file("f"), link("l", "f")]), &mut budget).unwrap();
        assert_eq!(budget, 99);
    }

    #[test]
    fn entangled_links_fail_validation_within_the_manifest_budget() {
        // `l0` wanders in and out of `b`; every other link goes through
        // `l0` until the hop limit, which costs thousands of steps per link.
        let mut entries = vec![file("b/f"), link("l0", &format!("{}b", "b/../".repeat(50)))];
        let through_l0 = format!("{}l0", "l0/../".repeat(42));
        for index in 1..=400 {
            entries.push(link(&format!("l{index}"), &through_l0));
        }
        let m = manifest(entries);
        assert_eq!(m.symlink_step_budget(), 1_000_000 + 64 * 402);
        assert!(matches!(
            m.validate(),
            Err(ManifestError::SymlinksTooComplex { mount, path })
                if mount == "." && path.starts_with('l')
        ));
        // A manifest without links spends nothing, whatever its size.
        let mut budget = 0;
        assert_eq!(
            resolve_symlinks(&root(vec![file("a"), file("b/c")]), &mut budget),
            Ok(Vec::new())
        );
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
