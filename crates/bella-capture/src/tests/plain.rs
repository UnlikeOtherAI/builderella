//! Directories without git, symlinks, odd names and the tree facts.

use super::*;
use crate::engine::tree_facts;
use bella_core::ids::Digest;
use bella_core::snapshot::{Manifest, RootManifest};

#[test]
fn plain_directory_is_walked_without_git() {
    let Some(tmp) = plain_dir() else { return };
    let root = tmp.path().join("project");
    write(&root.join("a.txt"), b"a");
    write(&root.join("sub/b.txt"), b"b");
    write(&root.join("sub/deep/c.txt"), b"c");
    write(&root.join("sub/skip/d.txt"), b"d");
    // Leftovers of a repository below the root are never captured.
    write(&root.join("sub/.git/config"), b"[core]");
    fs::create_dir_all(root.join("empty/inner")).unwrap();

    let store = MemStore::new();
    let policy = CapturePolicy {
        exclude: vec!["skip".into()],
        ..CapturePolicy::default()
    };
    let result = snap(&root, &policy, &store).unwrap();
    // Empty directories are not recorded.
    assert_eq!(paths(&result), ["a.txt", "sub/b.txt", "sub/deep/c.txt"]);
    assert_eq!(content(&store, &result, "sub/deep/c.txt"), b"c");
    assert_eq!(result.meta.roots[0].git, None);
    assert_eq!(result.meta.roots[0].label, "project");
    assert_eq!(result.stats.files, 3);
    assert_eq!(result.stats.bytes, 3);
    assert_eq!(result.stats.passes, 1);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    result.manifest.validate().unwrap();
}

#[test]
fn identical_content_is_stored_once() {
    let Some(tmp) = plain_dir() else { return };
    write(&tmp.path().join("one.txt"), b"same bytes");
    write(&tmp.path().join("two.txt"), b"same bytes");
    write(&tmp.path().join("three.txt"), b"other bytes");
    let store = MemStore::new();
    let first = snap(tmp.path(), &CapturePolicy::default(), &store).unwrap();
    assert_eq!(store.len(), 2);
    assert_eq!(
        (first.stats.objects_stored, first.stats.objects_reused),
        (2, 1)
    );
    let second = snap(tmp.path(), &CapturePolicy::default(), &store).unwrap();
    assert_eq!(
        (second.stats.objects_stored, second.stats.objects_reused),
        (0, 3)
    );
    assert_eq!(first.snapshot_id, second.snapshot_id);
    assert_eq!(first.manifest, second.manifest);
}

/// A link that leaves the root, or names an absolute path, would mean
/// something else on the machine that runs the build. The capture fails
/// and names the link.
#[test]
fn escaping_or_absolute_symlink_fails_naming_the_link() {
    let Some(tmp) = plain_dir() else { return };
    let root = tmp.path().join("root");
    write(&root.join("a.txt"), b"a");
    fs::create_dir_all(root.join("sub")).unwrap();

    if !symlink("../a.txt", &root.join("sub/ok"), false) {
        return;
    }
    let result = snap_default(&root);
    assert_eq!(
        entry(&result, "sub/ok"),
        Some(&Entry::Symlink {
            path: "sub/ok".into(),
            target: "../a.txt".into()
        })
    );
    assert!(result.meta.has_symlinks);

    assert!(symlink(
        "../../outside.txt",
        &root.join("sub/escape"),
        false
    ));
    let err = snap(&root, &CapturePolicy::default(), &MemStore::new()).unwrap_err();
    assert!(
        matches!(&err, CaptureError::Symlink { path, .. } if path == "sub/escape"),
        "{err}"
    );
    assert!(err.to_string().contains("sub/escape"), "{err}");
    fs::remove_file(root.join("sub/escape")).unwrap();

    let absolute = if cfg!(windows) {
        "C:/Windows/win.ini"
    } else {
        "/etc/hosts"
    };
    assert!(symlink(absolute, &root.join("abs"), false));
    let err = snap(&root, &CapturePolicy::default(), &MemStore::new()).unwrap_err();
    assert!(
        matches!(&err, CaptureError::Symlink { path, reason, .. } if path == "abs" && reason.contains("absolute")),
        "{err}"
    );
}

/// Each link stays inside the root on its own, but `out` goes through
/// `a/b/up` (the root) and one level higher. A worker would refuse to
/// materialize that tree, so the capture fails first and names the link.
#[test]
fn a29_symlink_chain_that_leaves_the_root_fails_naming_the_link() {
    let Some(tmp) = plain_dir() else { return };
    let root = tmp.path().join("root");
    write(&root.join("a/b/f.txt"), b"f");
    if !symlink("../..", &root.join("a/b/up"), true) {
        return;
    }
    // On its own the first link is fine.
    let result = snap_default(&root);
    assert_eq!(paths(&result), ["a/b/f.txt", "a/b/up"]);

    assert!(symlink("a/b/up/..", &root.join("out"), true));
    let store = MemStore::new();
    let err = snap(&root, &CapturePolicy::default(), &store).unwrap_err();
    assert!(
        matches!(
            &err,
            CaptureError::Symlink { path, target, reason }
                if path == "out" && target == "a/b/up/.." && reason.contains("other links")
        ),
        "{err}"
    );
    assert!(err.to_string().contains("out"), "{err}");

    // Under a mount the link is named the way the capture shows paths.
    let mounted = [RootSpec::mounted(&root, "app")];
    let err = capture(&mounted, &CapturePolicy::default(), &store, None).unwrap_err();
    assert!(
        matches!(&err, CaptureError::Symlink { path, .. } if path == "app/out"),
        "{err}"
    );
}

/// A link to a directory is one entry. What it points at is captured where
/// it really is, never a second time through the link.
#[test]
fn symlink_to_a_directory_is_not_followed() {
    let Some(tmp) = plain_dir() else { return };
    let root = tmp.path().join("root");
    write(&root.join("real/inner.txt"), b"inner");
    if !symlink("real", &root.join("alias"), true) {
        return;
    }
    let store = MemStore::new();
    let result = snap(&root, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(paths(&result), ["alias", "real/inner.txt"]);
    assert_eq!(
        entry(&result, "alias"),
        Some(&Entry::Symlink {
            path: "alias".into(),
            target: "real".into()
        })
    );
    assert_eq!(store.len(), 1);
}

/// A name that is not valid UTF-8 cannot be written in a manifest. The
/// capture fails and shows the name as well as it can; it does not skip
/// the file.
#[test]
fn name_that_is_not_utf8_fails_with_the_path() {
    let Some(tmp) = plain_dir() else { return };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"bad-\xff-name.txt".to_vec())
    };
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = "bad-".encode_utf16().collect();
        wide.push(0xD800); // an unpaired surrogate
        wide.extend("-name.txt".encode_utf16());
        std::ffi::OsString::from_wide(&wide)
    };
    fs::create_dir(tmp.path().join("dir")).unwrap();
    write(&tmp.path().join("fine.txt"), b"fine");
    if let Err(e) = fs::write(tmp.path().join("dir").join(&name), b"x") {
        eprintln!("skipped: this filesystem refuses names that are not valid Unicode ({e})");
        return;
    }
    let store = MemStore::new();
    let err = snap(tmp.path(), &CapturePolicy::default(), &store).unwrap_err();
    match &err {
        CaptureError::NonUtf8Path { path } => {
            assert!(path.starts_with("dir/bad-"), "{path}");
            assert!(path.ends_with("-name.txt"), "{path}");
            assert!(path.contains('\u{fffd}'), "{path}");
        }
        other => panic!("unexpected error: {other}"),
    }
}

/// The same failure when git reports the name.
#[cfg(unix)]
#[test]
fn name_that_is_not_utf8_fails_in_a_git_tree() {
    use std::os::unix::ffi::OsStringExt;
    let repo = Repo::new();
    repo.write("fine.txt", b"fine");
    repo.commit_all("init");
    let name = std::ffi::OsString::from_vec(b"bad-\xff-name.txt".to_vec());
    if let Err(e) = fs::write(repo.dir.join(&name), b"x") {
        eprintln!("skipped: this filesystem refuses names that are not valid Unicode ({e})");
        return;
    }
    let err = snap(&repo.dir, &CapturePolicy::default(), &MemStore::new()).unwrap_err();
    assert!(
        matches!(&err, CaptureError::NonUtf8Path { path } if path.starts_with("bad-")),
        "{err}"
    );
}

/// Sockets, FIFOs and devices cannot be stored. They are left out with a
/// warning instead of being opened.
#[cfg(unix)]
#[test]
fn special_files_are_skipped_with_a_warning() {
    let Some(tmp) = plain_dir() else { return };
    write(&tmp.path().join("a.txt"), b"a");
    let fifo = tmp.path().join("pipe");
    let made = Command::new("mkfifo").arg(&fifo).status();
    if !made.is_ok_and(|status| status.success()) {
        eprintln!("skipped: mkfifo is not available");
        return;
    }
    let result = snap_default(tmp.path());
    assert_eq!(paths(&result), ["a.txt"]);
    assert!(
        result.warnings.iter().any(|w| w.contains("`pipe`")),
        "{:?}",
        result.warnings
    );
}

#[test]
fn roots_and_mounts_are_validated_before_anything_is_read() {
    let Some(tmp) = plain_dir() else { return };
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    write(&a.join("f"), b"a");
    write(&b.join("f"), b"b");
    let store = MemStore::new();
    let policy = CapturePolicy::default();
    let run = |roots: &[RootSpec]| capture(roots, &policy, &store, None);

    assert!(matches!(run(&[]), Err(CaptureError::NoRoots)));
    for mounts in [
        ("x", "x"),
        (".", "y"),
        ("x", "x/y"),
        ("../up", "y"),
        ("", "y"),
    ] {
        let err = run(&[
            RootSpec::mounted(&a, mounts.0),
            RootSpec::mounted(&b, mounts.1),
        ])
        .unwrap_err();
        assert!(
            matches!(err, CaptureError::Manifest(_)),
            "{mounts:?}: {err}"
        );
    }
    let err = run(&[RootSpec::single(a.join("f"))]).unwrap_err();
    assert!(matches!(err, CaptureError::InvalidRoot { .. }), "{err}");
    let err = run(&[RootSpec::single(tmp.path().join("missing"))]).unwrap_err();
    assert!(matches!(err, CaptureError::InvalidRoot { .. }), "{err}");
    assert!(store.is_empty());
}

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

fn single(entries: Vec<Entry>) -> Manifest {
    Manifest::new(vec![RootManifest {
        mount: ".".into(),
        entries,
    }])
}

#[test]
fn tree_facts_report_windows_incompatibility() {
    assert_eq!(
        tree_facts(&single(vec![file("src/main.c"), file("src/lib.c")])),
        (None, false, false)
    );
    let (reason, ..) = tree_facts(&single(vec![file("ok.txt"), file("src/aux.c")]));
    let reason = reason.unwrap();
    assert!(
        reason.contains("src/aux.c") && reason.contains("reserved"),
        "{reason}"
    );

    // The first reason in path order is the one reported.
    let (reason, ..) = tree_facts(&single(vec![file("z/what?.txt"), file("a/b:c")]));
    assert!(reason.unwrap().contains("a/b:c"));

    // Link targets count; the `..` that steers them does not.
    let (reason, _, symlinks) = tree_facts(&single(vec![link("a/l", "../b/ok")]));
    assert_eq!((reason, symlinks), (None, true));
    let (reason, ..) = tree_facts(&single(vec![link("a/l", "../b/con.txt")]));
    assert!(reason.unwrap().contains("a/l"));

    // A mount is part of the path the tree gets on the executor.
    let mounted = Manifest::new(vec![RootManifest {
        mount: "libs/nul".into(),
        entries: vec![file("ok.txt")],
    }]);
    assert!(tree_facts(&mounted).0.unwrap().contains("libs/nul"));
}

#[test]
fn tree_facts_report_case_and_normalisation_collisions() {
    let collide = |entries: Vec<Entry>| tree_facts(&single(entries)).1;
    assert!(collide(vec![file("README.md"), file("readme.md")]));
    assert!(collide(vec![
        file("caf\u{e9}.txt"),
        file("cafe\u{301}.txt")
    ]));
    // Directories collide although the full paths differ.
    assert!(collide(vec![file("Docs/a.txt"), file("docs/b.txt")]));
    assert!(collide(vec![file("src/x"), link("SRC", "src")]));
    assert!(!collide(vec![file("docs/a.txt"), file("docs/b.txt")]));

    // Across roots, through their mounts.
    let two = Manifest::new(vec![
        RootManifest {
            mount: "App".into(),
            entries: vec![file("f")],
        },
        RootManifest {
            mount: "app".into(),
            entries: vec![file("g")],
        },
    ]);
    assert!(tree_facts(&two).1);
}

/// The collision flag reaches the result when the filesystem can hold
/// both names.
#[test]
fn case_collision_is_reported_in_meta() {
    let Some(tmp) = plain_dir() else { return };
    write(&tmp.path().join("Name.txt"), b"upper");
    write(&tmp.path().join("name.txt"), b"lower");
    let listed = fs::read_dir(tmp.path()).unwrap().count();
    let result = snap_default(tmp.path());
    if listed == 2 {
        assert!(result.meta.case_sensitive_only);
    } else {
        eprintln!("note: this filesystem is case-insensitive; one name survived");
        assert!(!result.meta.case_sensitive_only);
    }
}
