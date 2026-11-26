//! The destination tree a manifest describes, as one namespace.
//!
//! Every root is placed under its mount and every directory a path implies
//! is made explicit, so the writer, the verifier and the platform checks all
//! look at the same set of paths.

use crate::error::MaterializeError;
use bella_core::ids::Digest;
use bella_core::snapshot::{Entry, Manifest};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};

/// How many links a resolution may pass through before it counts as a loop.
/// Matches the limit Linux applies to a path lookup.
const MAX_LINK_HOPS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node<'a> {
    Dir,
    File {
        digest: &'a Digest,
        size: u64,
        executable: bool,
    },
    Link {
        target: &'a str,
        /// Number of leading path components that form the root's mount.
        /// Resolution may not climb above them.
        floor: usize,
    },
}

/// Where a symlink ends up when it is resolved inside the manifest's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    Dir,
    File,
    /// Dangling, or passes through something that is not a directory.
    Missing,
    /// Leaves the source root the link belongs to.
    Escapes,
    /// Passes through more than [`MAX_LINK_HOPS`] links.
    Loop,
    /// The step budget ran out before the answer was known.
    OutOfBudget,
}

/// Destination-relative path (`/`-separated) to what must exist there.
/// Iteration is in byte order, so a directory always precedes its contents.
#[derive(Debug)]
pub(crate) struct Tree<'a> {
    pub(crate) nodes: BTreeMap<String, Node<'a>>,
}

impl<'a> Tree<'a> {
    /// Build the tree, refusing a manifest in which two entries claim the
    /// same place or a path runs through a file or a symlink.
    ///
    /// `Manifest::validate` already rejects those within one root; this is
    /// the independent check at the point where paths become writes.
    ///
    /// `max_nodes` bounds the entries plus the directories their paths
    /// imply, so a few deep paths cannot ask for millions of directories.
    pub(crate) fn build(manifest: &'a Manifest, max_nodes: u64) -> Result<Self, MaterializeError> {
        let mut nodes: BTreeMap<String, Node<'a>> = BTreeMap::new();
        for root in &manifest.roots {
            let (prefix, floor) = if root.mount == "." {
                (String::new(), 0)
            } else {
                add_ancestors(&mut nodes, &root.mount, max_nodes)?;
                add_node(&mut nodes, root.mount.clone(), Node::Dir, max_nodes)?;
                (format!("{}/", root.mount), root.mount.split('/').count())
            };
            for entry in &root.entries {
                let full = format!("{prefix}{}", entry.path());
                add_ancestors(&mut nodes, &full, max_nodes)?;
                let node = match entry {
                    Entry::File {
                        digest,
                        size,
                        executable,
                        ..
                    } => Node::File {
                        digest,
                        size: *size,
                        executable: *executable,
                    },
                    Entry::Symlink { target, .. } => Node::Link { target, floor },
                    Entry::Dir { .. } => Node::Dir,
                };
                add_node(&mut nodes, full, node, max_nodes)?;
            }
        }
        Ok(Self { nodes })
    }

    /// Resolve the symlink at `link` the way a path lookup would, using only
    /// the manifest's own entries.
    ///
    /// Every path component looked at takes one step from `budget`, which
    /// callers share across all links of a manifest so that a small manifest
    /// of links pointing through each other cannot buy unbounded work.
    pub(crate) fn resolve_link(&self, link: &str, budget: &mut u64) -> Resolved {
        let Some(Node::Link { target, floor }) = self.nodes.get(link) else {
            return Resolved::Missing;
        };
        let floor = *floor;
        // The directory being looked in, as a key of `nodes`; empty for the
        // destination itself. `depth` is its number of components.
        let mut current = match link.rfind('/') {
            Some(index) => link[..index].to_string(),
            None => String::new(),
        };
        let mut depth = current.split('/').filter(|part| !part.is_empty()).count();
        // Components still to walk; a link met on the way puts its own
        // target on top.
        let mut pending = vec![target.split('/')];
        let mut hops = 0;
        loop {
            let Some(components) = pending.last_mut() else {
                return Resolved::Dir;
            };
            let Some(component) = components.next() else {
                pending.pop();
                continue;
            };
            if *budget == 0 {
                return Resolved::OutOfBudget;
            }
            *budget -= 1;
            match component {
                "" | "." => {}
                ".." => {
                    if depth <= floor {
                        return Resolved::Escapes;
                    }
                    current.truncate(current.rfind('/').unwrap_or(0));
                    depth -= 1;
                }
                name => {
                    let parent_len = current.len();
                    if !current.is_empty() {
                        current.push('/');
                    }
                    current.push_str(name);
                    match self.nodes.get(current.as_str()) {
                        None => return Resolved::Missing,
                        Some(Node::Dir) => depth += 1,
                        Some(Node::File { .. }) => {
                            let ends_here = pending
                                .iter()
                                .all(|rest| rest.clone().all(|part| matches!(part, "" | ".")));
                            return if ends_here {
                                Resolved::File
                            } else {
                                Resolved::Missing
                            };
                        }
                        Some(Node::Link { target, .. }) => {
                            hops += 1;
                            if hops > MAX_LINK_HOPS {
                                return Resolved::Loop;
                            }
                            current.truncate(parent_len);
                            pending.push(target.split('/'));
                        }
                    }
                }
            }
        }
    }
}

fn check_room(nodes: &BTreeMap<String, Node<'_>>, max_nodes: u64) -> Result<(), MaterializeError> {
    if nodes.len() as u64 >= max_nodes {
        return Err(MaterializeError::TooManyEntries {
            entries: max_nodes.saturating_add(1),
            limit: max_nodes,
        });
    }
    Ok(())
}

/// Make every proper ancestor of `path` a directory, or fail when one of
/// them is already a file or a link.
fn add_ancestors<'a>(
    nodes: &mut BTreeMap<String, Node<'a>>,
    path: &str,
    max_nodes: u64,
) -> Result<(), MaterializeError> {
    for (index, _) in path.match_indices('/') {
        let ancestor = &path[..index];
        match nodes.get(ancestor) {
            Some(Node::Dir) => {}
            Some(Node::Link { .. }) => {
                return Err(MaterializeError::LinkInPath {
                    path: path.to_string(),
                    link: ancestor.to_string(),
                });
            }
            Some(Node::File { .. }) => {
                return Err(MaterializeError::Conflict {
                    path: path.to_string(),
                    with: ancestor.to_string(),
                });
            }
            None => {
                check_room(nodes, max_nodes)?;
                nodes.insert(ancestor.to_string(), Node::Dir);
            }
        }
    }
    Ok(())
}

fn add_node<'a>(
    nodes: &mut BTreeMap<String, Node<'a>>,
    path: String,
    node: Node<'a>,
    max_nodes: u64,
) -> Result<(), MaterializeError> {
    match nodes.get(&path) {
        None => {
            check_room(nodes, max_nodes)?;
            nodes.insert(path, node);
            Ok(())
        }
        Some(Node::Dir) if node == Node::Dir => Ok(()),
        Some(existing) => {
            // A directory that is already there was implied by a path below
            // it; name that path so the error points at the real clash.
            let below = format!("{path}/");
            let child = nodes
                .range::<str, _>((Bound::Included(below.as_str()), Bound::Unbounded))
                .next()
                .map(|(key, _)| key.as_str())
                .filter(|key| key.starts_with(&below));
            match (existing, node, child) {
                (Node::Dir, Node::Link { .. }, Some(child)) => Err(MaterializeError::LinkInPath {
                    path: child.to_string(),
                    link: path,
                }),
                (Node::Dir, _, Some(child)) => Err(MaterializeError::Conflict {
                    path: child.to_string(),
                    with: path,
                }),
                _ => Err(MaterializeError::Conflict {
                    with: path.clone(),
                    path,
                }),
            }
        }
    }
}

/// Join a destination-relative manifest path onto `dest`.
///
/// Returns `None` unless every component is exactly one ordinary file name
/// for the current platform, so a component can never reset the path to a
/// drive or a root, or step out with `..`.
pub(crate) fn join_rel(dest: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    for component in rel.split('/') {
        let mut parts = Path::new(component).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(name)), None) if name == OsStr::new(component) => {
                out.push(component);
            }
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::snapshot::{MANIFEST_VERSION, RootManifest};

    fn file(path: &str) -> Entry {
        Entry::File {
            path: path.into(),
            digest: Digest::of_bytes(path.as_bytes()),
            size: 1,
            executable: false,
        }
    }

    fn link(path: &str, target: &str) -> Entry {
        Entry::Symlink {
            path: path.into(),
            target: target.into(),
        }
    }

    /// Built without `Manifest::new` or `validate`, to reach the defences a
    /// validated manifest never triggers.
    fn raw(roots: Vec<(&str, Vec<Entry>)>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            roots: roots
                .into_iter()
                .map(|(mount, entries)| RootManifest {
                    mount: mount.into(),
                    entries,
                })
                .collect(),
        }
    }

    fn resolve(tree: &Tree<'_>, link: &str) -> Resolved {
        tree.resolve_link(link, &mut 10_000)
    }

    #[test]
    fn a36_file_below_a_symlink_entry_is_refused_in_either_order() {
        let link_first = raw(vec![(".", vec![link("out", "elsewhere"), file("out/x")])]);
        assert!(matches!(
            Tree::build(&link_first, u64::MAX),
            Err(MaterializeError::LinkInPath { path, link }) if path == "out/x" && link == "out"
        ));
        let file_first = raw(vec![(".", vec![file("out/x"), link("out", "elsewhere")])]);
        assert!(matches!(
            Tree::build(&file_first, u64::MAX),
            Err(MaterializeError::LinkInPath { path, link }) if path == "out/x" && link == "out"
        ));
    }

    #[test]
    fn a36_symlink_across_roots_cannot_carry_a_mount() {
        // One root holds a link named like the directory another root mounts in.
        let m = raw(vec![
            ("libs", vec![link("shared", "../outside")]),
            ("libs/shared", vec![file("x")]),
        ]);
        assert!(matches!(
            Tree::build(&m, u64::MAX),
            Err(MaterializeError::LinkInPath { .. } | MaterializeError::Conflict { .. })
        ));
    }

    #[test]
    fn file_used_as_directory_and_duplicates_conflict() {
        let m = raw(vec![(".", vec![file("a"), file("a/b")])]);
        assert!(matches!(
            Tree::build(&m, u64::MAX),
            Err(MaterializeError::Conflict { path, with }) if path == "a/b" && with == "a"
        ));
        let m = raw(vec![(".", vec![file("a/b"), file("a")])]);
        assert!(matches!(
            Tree::build(&m, u64::MAX),
            Err(MaterializeError::Conflict { path, with }) if path == "a/b" && with == "a"
        ));
        let m = raw(vec![(".", vec![file("a"), file("a")])]);
        assert!(matches!(
            Tree::build(&m, u64::MAX),
            Err(MaterializeError::Conflict { .. })
        ));
    }

    #[test]
    fn implied_directories_and_mounts_become_nodes() {
        let m = raw(vec![
            ("app", vec![file("src/main.rs")]),
            (
                "libs/shared",
                vec![Entry::Dir {
                    path: "empty".into(),
                }],
            ),
        ]);
        let tree = Tree::build(&m, u64::MAX).unwrap();
        let dirs: Vec<&str> = tree
            .nodes
            .iter()
            .filter(|(_, n)| **n == Node::Dir)
            .map(|(p, _)| p.as_str())
            .collect();
        assert_eq!(
            dirs,
            ["app", "app/src", "libs", "libs/shared", "libs/shared/empty"]
        );
        assert!(matches!(
            tree.nodes.get("app/src/main.rs"),
            Some(Node::File { .. })
        ));
    }

    #[test]
    fn links_resolve_inside_the_manifest_tree() {
        let m = raw(vec![(
            ".",
            vec![
                file("a/b/file"),
                link("a/b/up2", "../.."),
                link("to_file", "a/b/file"),
                link("to_dir", "a/b"),
                link("to_dir_slash", "a/b/"),
                link("via_link", "to_dir/file"),
                link("dangling", "nope/x"),
                link("through_file", "a/b/file/x"),
                link("escape_chain", "a/b/up2/.."),
                link("loop_a", "loop_b"),
                link("loop_b", "loop_a"),
                link("root", "a/.."),
            ],
        )]);
        let tree = Tree::build(&m, u64::MAX).unwrap();
        assert_eq!(resolve(&tree, "to_file"), Resolved::File);
        assert_eq!(resolve(&tree, "to_dir"), Resolved::Dir);
        assert_eq!(resolve(&tree, "to_dir_slash"), Resolved::Dir);
        assert_eq!(resolve(&tree, "via_link"), Resolved::File);
        assert_eq!(resolve(&tree, "dangling"), Resolved::Missing);
        assert_eq!(resolve(&tree, "through_file"), Resolved::Missing);
        assert_eq!(resolve(&tree, "a/b/up2"), Resolved::Dir);
        assert_eq!(resolve(&tree, "escape_chain"), Resolved::Escapes);
        assert_eq!(resolve(&tree, "loop_a"), Resolved::Loop);
        assert_eq!(resolve(&tree, "root"), Resolved::Dir);
    }

    #[test]
    fn a_link_cannot_climb_above_its_own_mount() {
        let m = raw(vec![
            ("app", vec![file("x"), link("up", "sub/../..")]),
            ("lib", vec![file("y")]),
        ]);
        // `Manifest::validate` rejects this target lexically; the resolver
        // must reach the same answer on its own.
        let tree = Tree::build(&m, u64::MAX).unwrap();
        assert_eq!(resolve(&tree, "app/up"), Resolved::Missing);
        let m = raw(vec![
            ("app", vec![file("sub/x"), link("up", "sub/../..")]),
            ("lib", vec![file("y")]),
        ]);
        let tree = Tree::build(&m, u64::MAX).unwrap();
        assert_eq!(resolve(&tree, "app/up"), Resolved::Escapes);
    }

    #[test]
    fn a36_implied_directories_count_against_the_node_limit() {
        let m = raw(vec![(".", vec![file("a/b/c/d/e/f")])]);
        assert!(Tree::build(&m, 6).is_ok());
        assert!(matches!(
            Tree::build(&m, 5),
            Err(MaterializeError::TooManyEntries { limit: 5, .. })
        ));
        let m = raw(vec![("m/n", vec![file("f")])]);
        assert!(Tree::build(&m, 3).is_ok());
        assert!(matches!(
            Tree::build(&m, 2),
            Err(MaterializeError::TooManyEntries { limit: 2, .. })
        ));
    }

    #[test]
    fn a36_link_resolution_stops_when_its_budget_is_spent() {
        // 30 links, each going through the previous one twice.
        let mut entries = vec![file("base/f")];
        entries.push(link("l00", "base"));
        for index in 1..30 {
            entries.push(link(
                &format!("l{index:02}"),
                &format!("l{:02}/../l{:02}", index - 1, index - 1),
            ));
        }
        let m = raw(vec![(".", entries)]);
        let tree = Tree::build(&m, u64::MAX).unwrap();
        let mut budget = 50;
        assert_eq!(tree.resolve_link("l29", &mut budget), Resolved::OutOfBudget);
        assert_eq!(budget, 0);
        let mut budget = 50;
        assert_eq!(tree.resolve_link("l02", &mut budget), Resolved::Dir);
        assert!(budget < 50);
    }

    #[test]
    fn join_rel_accepts_only_plain_components() {
        let dest = Path::new("dest");
        assert_eq!(
            join_rel(dest, "a/b c/d\ne"),
            Some(dest.join("a").join("b c").join("d\ne"))
        );
        for bad in ["..", "a/../b", "/abs", "a//b", "a/./b", ""] {
            assert_eq!(join_rel(dest, bad), None, "{bad:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn join_rel_refuses_windows_separators_and_drives() {
        let dest = Path::new("dest");
        for bad in ["a\\b", "..\\x", "C:", "C:x", "\\\\server\\share", "a/C:\\x"] {
            assert_eq!(join_rel(dest, bad), None, "{bad:?}");
        }
    }
}
