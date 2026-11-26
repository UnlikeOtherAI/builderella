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
    },
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
            let prefix = if root.mount == "." {
                String::new()
            } else {
                add_ancestors(&mut nodes, &root.mount, max_nodes)?;
                add_node(&mut nodes, root.mount.clone(), Node::Dir, max_nodes)?;
                format!("{}/", root.mount)
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
                    Entry::Symlink { target, .. } => Node::Link { target },
                    Entry::Dir { .. } => Node::Dir,
                };
                add_node(&mut nodes, full, node, max_nodes)?;
            }
        }
        Ok(Self { nodes })
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
