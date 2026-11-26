//! `verify_tree` finds every kind of difference; `remove_tree` deletes a
//! workspace without following links.

mod common;

use bella_core::ids::Digest;
use bella_core::snapshot::{MANIFEST_VERSION, Manifest};
use bella_materialize::{EntryKind, TreeMismatch, materialize, remove_tree, verify_tree};
use common::{Snap, materialize_links, options};
use std::fs;
use std::path::Path;

fn sample() -> Snap {
    let mut snap = Snap::default();
    snap.file("a.txt", b"alpha");
    snap.file("b.txt", b"bravo");
    snap.file("c.txt", b"charlie");
    snap.file("sub/d.txt", b"delta");
    snap.file("sub/e.txt", b"echo");
    snap.exec("tool", b"#!/bin/sh\n");
    snap.dir("hollow");
    snap
}

#[test]
fn verify_tree_detects_extra_missing_changed_and_retyped_entries() {
    let snap = sample();
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("tree");
    materialize(&manifest, &snap.store, &dest, &options()).unwrap();
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));

    // One changed byte, same length.
    fs::write(dest.join("a.txt"), b"alphA").unwrap();
    // A different length.
    fs::write(dest.join("b.txt"), b"bravo!").unwrap();
    // A file where a directory belongs, and the reverse.
    fs::remove_dir(dest.join("hollow")).unwrap();
    fs::write(dest.join("hollow"), b"").unwrap();
    fs::remove_file(dest.join("c.txt")).unwrap();
    fs::create_dir(dest.join("c.txt")).unwrap();
    // A removed file.
    fs::remove_file(dest.join("sub").join("e.txt")).unwrap();
    // An extra file and an extra directory with content.
    fs::write(dest.join("sub").join("extra.txt"), b"left over").unwrap();
    fs::create_dir(dest.join("junk")).unwrap();
    fs::write(dest.join("junk").join("x"), b"x").unwrap();
    fs::write(dest.join("junk").join("y"), b"y").unwrap();

    assert_eq!(
        verify_tree(&manifest, &dest),
        Err(vec![
            TreeMismatch::Content {
                path: "a.txt".into(),
                expected: Digest::of_bytes(b"alpha"),
            },
            TreeMismatch::Size {
                path: "b.txt".into(),
                expected: 5,
                actual: 6,
            },
            TreeMismatch::Kind {
                path: "c.txt".into(),
                expected: EntryKind::File,
                actual: EntryKind::Dir,
            },
            TreeMismatch::Kind {
                path: "hollow".into(),
                expected: EntryKind::Dir,
                actual: EntryKind::File,
            },
            TreeMismatch::Extra {
                path: "junk".into()
            },
            TreeMismatch::Missing {
                path: "sub/e.txt".into()
            },
            TreeMismatch::Extra {
                path: "sub/extra.txt".into()
            },
        ])
    );
}

#[cfg(unix)]
#[test]
fn a29_verify_tree_detects_a_wrong_mode() {
    use std::os::unix::fs::PermissionsExt;
    let snap = sample();
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("tree");
    materialize(&manifest, &snap.store, &dest, &options()).unwrap();

    fs::set_permissions(dest.join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(dest.join("tool"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(dest.join("b.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        verify_tree(&manifest, &dest),
        Err(vec![
            TreeMismatch::Mode {
                path: "a.txt".into(),
                expected: 0o644,
                actual: 0o755,
            },
            TreeMismatch::Mode {
                path: "b.txt".into(),
                expected: 0o644,
                actual: 0o600,
            },
            TreeMismatch::Mode {
                path: "tool".into(),
                expected: 0o755,
                actual: 0o644,
            },
        ])
    );
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[test]
fn a29_verify_tree_detects_a_wrong_link_target_and_never_follows_links() {
    let mut snap = sample();
    snap.link("current", "a.txt");
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("tree");
    if materialize_links(&manifest, &snap.store, &dest).is_none() {
        return;
    }
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));

    // A link the manifest does not have is extra, whatever it points at.
    let sneaky = dest.join("sub").join("sneaky");
    symlink("d.txt", &sneaky).unwrap();
    assert_eq!(
        verify_tree(&manifest, &dest),
        Err(vec![TreeMismatch::Extra {
            path: "sub/sneaky".into()
        }])
    );
    fs::remove_file(&sneaky).unwrap();
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));

    // Repoint the link.
    fs::remove_file(dest.join("current")).unwrap();
    symlink("b.txt", &dest.join("current")).unwrap();
    // Replace a file with a link to an identical file: the bytes behind the
    // link match, the tree does not.
    fs::write(tmp.path().join("copy-of-c"), b"charlie").unwrap();
    fs::remove_file(dest.join("c.txt")).unwrap();
    symlink("../copy-of-c", &dest.join("c.txt")).unwrap();
    assert_eq!(fs::read(dest.join("c.txt")).unwrap(), b"charlie");

    assert_eq!(
        verify_tree(&manifest, &dest),
        Err(vec![
            TreeMismatch::Kind {
                path: "c.txt".into(),
                expected: EntryKind::File,
                actual: EntryKind::Symlink,
            },
            TreeMismatch::LinkTarget {
                path: "current".into(),
                expected: "a.txt".into(),
                actual: "b.txt".into(),
            },
        ])
    );
}

#[test]
fn verify_tree_reports_a_missing_destination_and_an_invalid_manifest() {
    let snap = sample();
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();

    assert_eq!(
        verify_tree(&manifest, &tmp.path().join("absent")),
        Err(vec![TreeMismatch::Missing { path: ".".into() }])
    );

    let a_file = tmp.path().join("a-file");
    fs::write(&a_file, b"").unwrap();
    assert_eq!(
        verify_tree(&manifest, &a_file),
        Err(vec![TreeMismatch::Kind {
            path: ".".into(),
            expected: EntryKind::Dir,
            actual: EntryKind::File,
        }])
    );

    // An empty directory is missing everything, implied directories included.
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let missing = verify_tree(&manifest, &empty).unwrap_err();
    assert_eq!(missing.len(), 8);
    assert!(
        missing
            .iter()
            .all(|mismatch| matches!(mismatch, TreeMismatch::Missing { .. }))
    );

    let invalid = Manifest {
        version: MANIFEST_VERSION + 1,
        ..manifest
    };
    assert!(matches!(
        verify_tree(&invalid, &empty).unwrap_err()[..],
        [TreeMismatch::InvalidManifest { .. }]
    ));
}

#[test]
fn remove_tree_deletes_read_only_files_and_directories() {
    let snap = sample();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("tree");
    materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap();

    let read_only = |path: &Path| {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).unwrap();
    };
    read_only(&dest.join("a.txt"));
    read_only(&dest.join("sub").join("d.txt"));
    // On Unix a read-only directory is what stops its contents being deleted.
    read_only(&dest.join("sub"));
    read_only(&dest.join("hollow"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir(dest.join("locked")).unwrap();
        fs::write(dest.join("locked").join("inside"), b"x").unwrap();
        fs::set_permissions(dest.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    }

    remove_tree(&dest).unwrap();
    assert!(!dest.exists());
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn remove_tree_tolerates_a_tree_that_is_already_gone() {
    let snap = sample();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("tree");

    // Never existed.
    remove_tree(&dest).unwrap();
    remove_tree(&dest.join("nor").join("this")).unwrap();

    // Partly removed by someone else, then removed twice.
    materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap();
    fs::remove_dir_all(dest.join("sub")).unwrap();
    fs::remove_file(dest.join("a.txt")).unwrap();
    remove_tree(&dest).unwrap();
    remove_tree(&dest).unwrap();
    assert!(!dest.exists());

    // A single file is removed too.
    let file = tmp.path().join("just-a-file");
    fs::write(&file, b"x").unwrap();
    remove_tree(&file).unwrap();
    assert!(!file.exists());
}

/// Two removals of one tree at once: each sees entries vanish under it and
/// both must still succeed.
#[cfg(unix)]
#[test]
fn remove_tree_tolerates_entries_vanishing_during_removal() {
    for _ in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("tree");
        for dir in 0..20 {
            let dir = dest.join(format!("d{dir}")).join("nested");
            fs::create_dir_all(&dir).unwrap();
            for file in 0..20 {
                fs::write(dir.join(format!("f{file}")), b"x").unwrap();
            }
        }
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..3).map(|_| scope.spawn(|| remove_tree(&dest))).collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        for result in results {
            result.unwrap();
        }
        assert!(!dest.exists());
    }
}

#[cfg(unix)]
#[test]
fn remove_tree_does_not_follow_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"precious").unwrap();

    let tree = tmp.path().join("tree");
    fs::create_dir_all(tree.join("deep")).unwrap();
    fs::write(tree.join("deep").join("own.txt"), b"mine").unwrap();
    std::os::unix::fs::symlink(&outside, tree.join("deep").join("to-dir")).unwrap();
    std::os::unix::fs::symlink(outside.join("keep.txt"), tree.join("to-file")).unwrap();
    std::os::unix::fs::symlink("../outside", tree.join("relative")).unwrap();
    std::os::unix::fs::symlink("nowhere", tree.join("dangling")).unwrap();

    remove_tree(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"precious");

    // A link handed in directly is removed as a link.
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    remove_tree(&link).unwrap();
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"precious");
}

/// Junctions need no privilege, so this runs on every Windows host.
#[cfg(windows)]
#[test]
fn remove_tree_does_not_follow_junctions() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"precious").unwrap();

    let tree = tmp.path().join("tree");
    fs::create_dir_all(tree.join("deep")).unwrap();
    fs::write(tree.join("deep").join("own.txt"), b"mine").unwrap();
    let junction = tree.join("deep").join("junction");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "could not create a junction");
    assert_eq!(fs::read(junction.join("keep.txt")).unwrap(), b"precious");

    remove_tree(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"precious");

    // A junction handed in directly is removed as a link.
    let direct = tmp.path().join("direct");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&direct)
        .arg(&outside)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    remove_tree(&direct).unwrap();
    assert!(fs::symlink_metadata(&direct).is_err());
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"precious");
}

/// Symlinks on Windows, when the host allows creating them.
#[cfg(windows)]
#[test]
fn remove_tree_does_not_follow_windows_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"precious").unwrap();
    let tree = tmp.path().join("tree");
    fs::create_dir(&tree).unwrap();
    if let Err(error) = std::os::windows::fs::symlink_dir(&outside, tree.join("to-dir")) {
        eprintln!("skipped: symlink assertions (cannot create a symlink: {error})");
        return;
    }
    std::os::windows::fs::symlink_file(outside.join("keep.txt"), tree.join("to-file")).unwrap();

    remove_tree(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"precious");
}
