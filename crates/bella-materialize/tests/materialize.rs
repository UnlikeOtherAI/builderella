//! Materializing trees: round trips, freshness (A27), names (A28) and modes
//! and links (A29).

mod common;

use bella_core::platform::Os;
use bella_core::snapshot::{Entry, Manifest, RootManifest};
use bella_core::store::{MemStore, put_bytes};
use bella_materialize::{
    Incompatibility, MaterializeError, MaterializeOptions, PathLimits, TreeMismatch,
    check_platform, is_case_sensitive, materialize, verify_tree,
};
use common::{Snap, is_empty_dir, list_tree, materialize_links, options, pattern, single_root};
use std::fs;

fn current_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// Nested directories, an empty file, a large file, an executable, an empty
/// directory and relative symlinks to a file, to a directory and to nothing.
fn sample() -> Snap {
    let mut snap = Snap::default();
    snap.file("README.md", b"# sample\n");
    snap.file("empty", b"");
    snap.file("src/deep/nested/mod.rs", b"pub fn answer() -> u32 { 42 }\n");
    snap.exec("bin/run.sh", b"#!/bin/sh\necho ok\n");
    snap.file("big.bin", &pattern(3 * 1024 * 1024 + 17));
    snap.dir("empty-dir/inner");
    snap.link("bin/latest", "run.sh");
    snap.link("docs", "src/deep");
    snap.link("dangling", "src/not-there");
    snap
}

#[test]
fn round_trip_nested_tree_passes_verify() {
    let snap = sample();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("work");

    let with_links = snap.manifest();
    let (manifest, report) = match materialize_links(&with_links, &snap.store, &dest) {
        Some(report) => (with_links, report),
        None => {
            let plain = snap.without_links();
            let report = materialize(&plain, &snap.store, &dest, &options()).unwrap();
            (plain, report)
        }
    };
    let links = manifest
        .roots
        .iter()
        .flat_map(|root| &root.entries)
        .filter(|entry| matches!(entry, Entry::Symlink { .. }))
        .count() as u64;

    assert_eq!(verify_tree(&manifest, &dest), Ok(()));
    assert_eq!(report.files, 5);
    assert_eq!(report.symlinks, links);
    // bin, src, src/deep, src/deep/nested, empty-dir, empty-dir/inner
    assert_eq!(report.dirs, 6);
    assert_eq!(report.bytes, manifest.total_bytes());

    assert_eq!(fs::read(dest.join("README.md")).unwrap(), b"# sample\n");
    assert_eq!(fs::metadata(dest.join("empty")).unwrap().len(), 0);
    assert_eq!(
        fs::read(dest.join("big.bin")).unwrap(),
        pattern(3 * 1024 * 1024 + 17)
    );
    assert!(is_empty_dir(&dest.join("empty-dir").join("inner")));
    if links > 0 {
        // The directory link works as a directory.
        assert_eq!(
            fs::read(dest.join("docs").join("nested").join("mod.rs")).unwrap(),
            b"pub fn answer() -> u32 { 42 }\n"
        );
    }
}

#[test]
fn a27_deleted_file_does_not_survive_in_a_fresh_tree() {
    let mut a = Snap::default();
    a.file("keep.txt", b"kept");
    a.file("gone.txt", b"deleted at the origin");
    a.file("old/name.txt", b"renamed");
    let mut b = Snap::default();
    b.file("keep.txt", b"kept");
    b.file("new/name.txt", b"renamed");

    let tmp = tempfile::tempdir().unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    materialize(&a.manifest(), &a.store, &dir_a, &options()).unwrap();
    materialize(&b.manifest(), &b.store, &dir_b, &options()).unwrap();

    assert_eq!(verify_tree(&b.manifest(), &dir_b), Ok(()));
    assert!(!dir_b.join("gone.txt").exists());
    assert!(!dir_b.join("old").exists());
    assert_eq!(
        list_tree(&dir_b),
        ["keep.txt", "new", "new/name.txt"]
            .map(String::from)
            .into_iter()
            .collect()
    );

    // The older tree is exactly what verification would refuse for B, so a
    // survivor could not go unnoticed.
    let stale = verify_tree(&b.manifest(), &dir_a).unwrap_err();
    assert_eq!(
        stale,
        [
            TreeMismatch::Extra {
                path: "gone.txt".into()
            },
            TreeMismatch::Missing { path: "new".into() },
            TreeMismatch::Missing {
                path: "new/name.txt".into()
            },
            TreeMismatch::Extra { path: "old".into() },
        ]
    );

    // And B cannot be laid over A: the old tree is left as it was.
    let error = materialize(&b.manifest(), &b.store, &dir_a, &options()).unwrap_err();
    assert!(matches!(
        error,
        MaterializeError::DestinationNotEmpty { .. }
    ));
    assert_eq!(verify_tree(&a.manifest(), &dir_a), Ok(()));
}

#[test]
fn a27_non_empty_destination_is_refused() {
    let mut snap = Snap::default();
    snap.file("a.txt", b"a");
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();

    let occupied = tmp.path().join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("theirs"), b"untouched").unwrap();
    let error = materialize(&manifest, &snap.store, &occupied, &options()).unwrap_err();
    assert!(
        matches!(&error, MaterializeError::DestinationNotEmpty { dest } if *dest == occupied),
        "{error}"
    );
    assert_eq!(list_tree(&occupied), ["theirs".to_string()].into());
    assert_eq!(fs::read(occupied.join("theirs")).unwrap(), b"untouched");

    let a_file = tmp.path().join("a-file");
    fs::write(&a_file, b"untouched").unwrap();
    let error = materialize(&manifest, &snap.store, &a_file, &options()).unwrap_err();
    assert!(
        matches!(error, MaterializeError::DestinationNotDirectory { .. }),
        "{error}"
    );
    assert_eq!(fs::read(&a_file).unwrap(), b"untouched");

    // An existing empty directory is accepted and filled.
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    materialize(&manifest, &snap.store, &empty, &options()).unwrap();
    assert_eq!(verify_tree(&manifest, &empty), Ok(()));

    // Missing parents are created.
    let deep = tmp.path().join("x").join("y").join("z");
    materialize(&manifest, &snap.store, &deep, &options()).unwrap();
    assert_eq!(verify_tree(&manifest, &deep), Ok(()));
}

#[cfg(unix)]
#[test]
fn a27_destination_that_is_a_symlink_is_refused() {
    let mut snap = Snap::default();
    snap.file("a.txt", b"a");
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let error = materialize(&snap.manifest(), &snap.store, &link, &options()).unwrap_err();
    assert!(
        matches!(error, MaterializeError::DestinationNotDirectory { .. }),
        "{error}"
    );
    assert!(is_empty_dir(&real));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn multiple_roots_land_under_their_mounts() {
    let store = MemStore::new();
    let file = |path: &str, bytes: &[u8]| Entry::File {
        path: path.into(),
        digest: put_bytes(&store, bytes).unwrap(),
        size: bytes.len() as u64,
        executable: false,
    };
    let manifest = Manifest::new(vec![
        RootManifest {
            mount: "libs/shared".into(),
            entries: vec![file("src/a.txt", b"shared")],
        },
        RootManifest {
            mount: "app".into(),
            entries: vec![file("src/a.txt", b"app")],
        },
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("set");
    let report = materialize(&manifest, &store, &dest, &options()).unwrap();

    assert_eq!(
        fs::read(dest.join("app").join("src").join("a.txt")).unwrap(),
        b"app"
    );
    assert_eq!(
        fs::read(dest.join("libs").join("shared").join("src").join("a.txt")).unwrap(),
        b"shared"
    );
    // app, app/src, libs, libs/shared, libs/shared/src
    assert_eq!((report.files, report.dirs), (2, 5));
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));
}

#[test]
fn a28_unicode_spaces_and_newlines_round_trip_exactly() {
    let mut snap = Snap::default();
    snap.file("dir with spaces/ünï côdé.txt", b"one");
    snap.file("日本語/ファイル.txt", b"two");
    snap.file("emoji 🎉.txt", b"three");
    // Decomposed on purpose: the name must come back in the form it went in.
    snap.file("nfd/cafe\u{301}.txt", b"four");
    if !cfg!(windows) {
        snap.file("new\nline/tab\there.txt", b"five");
        snap.file("back\\slash and: colon?.txt", b"six");
    }
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("names");
    materialize(&manifest, &snap.store, &dest, &options()).unwrap();

    assert_eq!(verify_tree(&manifest, &dest), Ok(()));
    let mut expected: std::collections::BTreeSet<String> = snap
        .entries
        .iter()
        .map(|entry| entry.path().to_string())
        .collect();
    for dir in ["dir with spaces", "日本語", "nfd"] {
        expected.insert(dir.to_string());
    }
    if !cfg!(windows) {
        expected.insert("new\nline".to_string());
    }
    assert_eq!(list_tree(&dest), expected);
}

#[test]
fn a28_windows_problems_are_reported_up_front_by_check_platform() {
    let mut snap = Snap::default();
    for path in [
        "ok/file.txt",
        "src/aux.c",
        "a/b:c",
        "trailing./x",
        "tab\tname",
        "new\nline",
        "back\\slash",
        "COM\u{b9}",
    ] {
        snap.file(path, b"x");
    }
    snap.link("link-backslash", "ok\\file.txt");
    snap.link("link-device", "ok/NUL");
    snap.link("link-fine", "ok/file.txt");
    let manifest = snap.manifest();
    assert!(manifest.validate().is_ok());

    let problems = check_platform(&manifest, Os::Windows, false).unwrap_err();
    let bad_names: Vec<&str> = problems
        .iter()
        .filter_map(|problem| match problem {
            Incompatibility::WindowsName { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        bad_names,
        [
            "COM\u{b9}",
            "a/b:c",
            "back\\slash",
            "new\nline",
            "src/aux.c",
            "tab\tname",
            "trailing./x"
        ]
    );
    let bad_links: Vec<&str> = problems
        .iter()
        .filter_map(|problem| match problem {
            Incompatibility::WindowsSymlinkTarget { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(bad_links, ["link-backslash", "link-device"]);
    assert_eq!(problems.len(), bad_names.len() + bad_links.len());

    // The same tree is fine where those names are ordinary.
    assert_eq!(check_platform(&manifest, Os::Linux, true), Ok(()));
    assert_eq!(check_platform(&manifest, Os::Macos, true), Ok(()));
}

#[test]
fn a28_over_long_names_and_paths_are_reported_per_platform() {
    let entry = |path: String| Entry::File {
        path,
        digest: bella_core::ids::Digest::of_bytes(b"x"),
        size: 1,
        executable: false,
    };

    // 300 units on Windows, 300 bytes elsewhere: longer than MAX_PATH only.
    let deep = vec!["d".repeat(29); 10].join("/");
    assert_eq!(deep.len(), 299);
    let manifest = single_root(vec![entry(format!("{deep}/f"))]);
    let problems = check_platform(&manifest, Os::Windows, true).unwrap_err();
    assert!(
        problems.contains(&Incompatibility::PathTooLong {
            path: format!("{deep}/f"),
            len: 301,
            limit: 259,
        }),
        "{problems:?}"
    );
    assert!(
        problems.contains(&Incompatibility::PathTooLong {
            path: deep.clone(),
            len: 299,
            limit: 247,
        }),
        "{problems:?}"
    );
    assert_eq!(check_platform(&manifest, Os::Linux, true), Ok(()));
    assert_eq!(check_platform(&manifest, Os::Macos, true), Ok(()));

    // A directory is limited to 247 units on Windows even when a file of
    // the same length would fit.
    let dir = vec!["d".repeat(49); 5].join("/");
    assert_eq!(dir.len(), 249);
    let manifest = single_root(vec![Entry::Dir { path: dir.clone() }]);
    assert_eq!(
        check_platform(&manifest, Os::Windows, true),
        Err(vec![Incompatibility::PathTooLong {
            path: dir,
            len: 249,
            limit: 247,
        }])
    );

    // One name over 255 units is too long everywhere.
    let manifest = single_root(vec![entry("n".repeat(256))]);
    for os in [Os::Windows, Os::Linux, Os::Macos] {
        let problems = check_platform(&manifest, os, true).unwrap_err();
        assert!(
            matches!(
                problems[0],
                Incompatibility::NameTooLong {
                    len: 256,
                    limit: 255,
                    ..
                }
            ),
            "{os:?}: {problems:?}"
        );
    }

    // Units differ: 100 three-byte characters are 100 UTF-16 units.
    let manifest = single_root(vec![entry("日".repeat(100))]);
    assert_eq!(check_platform(&manifest, Os::Windows, true), Ok(()));
    for os in [Os::Linux, Os::Macos] {
        assert!(matches!(
            check_platform(&manifest, os, true).unwrap_err()[..],
            [Incompatibility::NameTooLong {
                len: 300,
                limit: 255,
                ..
            }]
        ));
    }
}

/// A relative path of exactly `len` bytes made of `d` directories and a
/// final file `f`, with no name longer than 201 bytes.
fn rel_path_of_len(len: usize) -> String {
    let mut dirs: Vec<u8> = vec![b'd'; len - 2];
    for index in (100..dirs.len()).step_by(101) {
        if index + 1 < dirs.len() {
            dirs[index] = b'/';
        }
    }
    format!("{}/f", String::from_utf8(dirs).unwrap())
}

#[test]
fn a28_destination_path_over_the_os_limit_is_a_precise_error() {
    let limits = PathLimits::for_os(current_os());
    let mut snap = Snap::default();
    snap.file("short.txt", b"fits");
    // Fits on its own, not once the destination directory is in front.
    let rel = rel_path_of_len(limits.dir_path - 10);
    snap.file(&rel, b"too deep");
    let manifest = snap.manifest();
    assert_eq!(check_platform(&manifest, current_os(), true), Ok(()));

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("long");
    let error = materialize(&manifest, &snap.store, &dest, &options()).unwrap_err();
    let MaterializeError::Incompatible(problems) = &error else {
        panic!("unexpected error: {error}");
    };
    assert!(!problems.is_empty());
    for problem in problems {
        let Incompatibility::PathTooLong { path, len, limit } = problem else {
            panic!("unexpected problem: {problem}");
        };
        assert!(rel.starts_with(path.as_str()), "{path}");
        assert!(len > limit);
    }
    assert!(error.to_string().contains(&rel[..50]), "{error}");
    assert!(!dest.exists());
}

#[cfg(windows)]
#[test]
fn a28_windows_refuses_reserved_names_and_never_renames() {
    let tmp = tempfile::tempdir().unwrap();
    for path in [
        "src/aux.c",
        "a\\b",
        "..\\escape",
        "C:",
        "name.",
        "a/b:stream",
    ] {
        let mut snap = Snap::default();
        snap.file("ok.txt", b"ok");
        snap.file(path, b"x");
        let dest = tmp.path().join("win");
        let error = materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap_err();
        assert!(
            matches!(
                &error,
                MaterializeError::Incompatible(problems)
                    if matches!(&problems[..], [Incompatibility::WindowsName { path: p, .. }] if p == path)
            ),
            "{path:?}: {error}"
        );
        assert!(!dest.exists(), "{path:?} left a tree behind");
    }

    let mut snap = Snap::default();
    snap.file("ok.txt", b"ok");
    snap.link("link", "sub\\ok.txt");
    let dest = tmp.path().join("win-link");
    let error = materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::Incompatible(problems)
                if matches!(&problems[..], [Incompatibility::WindowsSymlinkTarget { .. }])
        ),
        "{error}"
    );
    assert!(!dest.exists());
}

#[test]
fn a28_case_collisions_are_refused_on_a_case_insensitive_volume() {
    let mut snap = Snap::default();
    snap.file("README.md", b"upper");
    snap.file("readme.md", b"lower");
    snap.file("Dir/a", b"a");
    snap.file("dir/b", b"b");
    let manifest = snap.manifest();

    // Reported up front, including the directories that would merge.
    assert_eq!(
        check_platform(&manifest, Os::Linux, false),
        Err(vec![
            Incompatibility::Collision {
                first: "Dir".into(),
                second: "dir".into()
            },
            Incompatibility::Collision {
                first: "README.md".into(),
                second: "readme.md".into()
            },
        ])
    );
    assert_eq!(check_platform(&manifest, Os::Linux, true), Ok(()));
    assert_eq!(check_platform(&manifest, Os::Macos, true), Ok(()));

    let tmp = tempfile::tempdir().unwrap();
    let case_sensitive = is_case_sensitive(tmp.path()).unwrap();
    let dest = tmp.path().join("case");
    let result = materialize(&manifest, &snap.store, &dest, &options());
    if case_sensitive {
        result.unwrap();
        assert_eq!(verify_tree(&manifest, &dest), Ok(()));
        assert_eq!(fs::read(dest.join("README.md")).unwrap(), b"upper");
        assert_eq!(fs::read(dest.join("readme.md")).unwrap(), b"lower");
    } else {
        let error = result.unwrap_err();
        let MaterializeError::Incompatible(problems) = &error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .all(|problem| matches!(problem, Incompatibility::Collision { .. }))
        );
        assert!(!dest.exists(), "one file must not overwrite the other");

        // An empty directory that was there before is left empty.
        fs::create_dir(&dest).unwrap();
        assert!(materialize(&manifest, &snap.store, &dest, &options()).is_err());
        assert!(is_empty_dir(&dest));
    }
}

#[test]
fn a28_normalization_collisions_are_refused_where_the_volume_folds_them() {
    let mut snap = Snap::default();
    snap.file("caf\u{e9}.txt", b"composed");
    snap.file("cafe\u{301}.txt", b"decomposed");
    let manifest = snap.manifest();

    let collision = vec![Incompatibility::Collision {
        first: "cafe\u{301}.txt".into(),
        second: "caf\u{e9}.txt".into(),
    }];
    assert_eq!(check_platform(&manifest, Os::Linux, true), Ok(()));
    // macOS volumes ignore normalization even when they are case-sensitive.
    assert_eq!(
        check_platform(&manifest, Os::Macos, true),
        Err(collision.clone())
    );
    assert_eq!(
        check_platform(&manifest, Os::Windows, false),
        Err(collision.clone())
    );

    let tmp = tempfile::tempdir().unwrap();
    let folds = cfg!(target_os = "macos") || !is_case_sensitive(tmp.path()).unwrap();
    let dest = tmp.path().join("nfc");
    let result = materialize(&manifest, &snap.store, &dest, &options());
    if folds {
        assert!(
            matches!(&result, Err(MaterializeError::Incompatible(problems)) if *problems == collision),
            "{result:?}"
        );
        assert!(!dest.exists());
    } else {
        result.unwrap();
        assert_eq!(verify_tree(&manifest, &dest), Ok(()));
    }
}

#[test]
fn a29_executable_bit_is_applied_on_unix_and_ignored_on_windows() {
    let mut snap = Snap::default();
    snap.exec("bin/tool", b"#!/bin/sh\n");
    snap.file("bin/data", b"data");
    snap.exec("empty-exec", b"");
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("modes");
    materialize(&manifest, &snap.store, &dest, &options()).unwrap();
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));
    assert_eq!(
        fs::read(dest.join("bin").join("tool")).unwrap(),
        b"#!/bin/sh\n"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&dest.join("bin").join("tool")), 0o755);
        assert_eq!(mode(&dest.join("bin").join("data")), 0o644);
        assert_eq!(mode(&dest.join("empty-exec")), 0o755);
        assert_eq!(mode(&dest.join("bin")), 0o755);
    }
    #[cfg(windows)]
    {
        // Nothing to apply: the files are plain, writable files.
        for name in ["tool", "data"] {
            let metadata = fs::metadata(dest.join("bin").join(name)).unwrap();
            assert!(metadata.is_file() && !metadata.permissions().readonly());
        }
    }
}

#[test]
fn a29_symlinks_are_real_symlinks() {
    let mut snap = Snap::default();
    snap.file("data/real.txt", b"real");
    snap.link("to-file", "data/real.txt");
    snap.link("to-dir", "data");
    snap.link("data/up", "../data/real.txt");
    snap.link("dangling", "data/missing.txt");
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("links");
    let Some(report) = materialize_links(&manifest, &snap.store, &dest) else {
        return;
    };
    assert_eq!((report.files, report.symlinks), (1, 4));
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));

    let target = |rel: &[&str]| {
        let path = rel.iter().fold(dest.clone(), |path, part| path.join(part));
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{rel:?} is a copy, not a link"
        );
        fs::read_link(&path)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    assert_eq!(target(&["to-file"]), "data/real.txt");
    assert_eq!(target(&["to-dir"]), "data");
    assert_eq!(target(&["data", "up"]), "../data/real.txt");
    assert_eq!(target(&["dangling"]), "data/missing.txt");

    // The links resolve, and the dangling one stays dangling.
    assert_eq!(fs::read(dest.join("to-file")).unwrap(), b"real");
    assert_eq!(
        fs::read(dest.join("to-dir").join("real.txt")).unwrap(),
        b"real"
    );
    assert_eq!(fs::read(dest.join("data").join("up")).unwrap(), b"real");
    assert!(fs::metadata(dest.join("dangling")).is_err());

    // Writing through a link changes the one real file: no copy was made.
    fs::write(dest.join("to-file"), b"edited").unwrap();
    assert_eq!(
        fs::read(dest.join("data").join("real.txt")).unwrap(),
        b"edited"
    );
}

#[test]
fn every_file_is_its_own_copy() {
    let mut snap = Snap::default();
    snap.file("one.txt", b"same bytes");
    snap.file("two.txt", b"same bytes");
    snap.file("sub/three.txt", b"same bytes");
    let manifest = snap.manifest();
    assert_eq!(snap.store.len(), 1);
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    materialize(&manifest, &snap.store, &first, &options()).unwrap();
    materialize(&manifest, &snap.store, &second, &options()).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut inodes = std::collections::BTreeSet::new();
        for tree in [&first, &second] {
            for rel in ["one.txt", "two.txt", "sub/three.txt"] {
                let metadata = fs::metadata(tree.join(rel)).unwrap();
                assert_eq!(metadata.nlink(), 1, "{rel} is hard-linked");
                inodes.insert(metadata.ino());
            }
        }
        assert_eq!(inodes.len(), 6);
    }

    // Changing one copy changes nothing else, in this tree or the other.
    fs::write(first.join("one.txt"), b"changed!!!").unwrap();
    assert_eq!(fs::read(first.join("two.txt")).unwrap(), b"same bytes");
    assert_eq!(verify_tree(&manifest, &second), Ok(()));
    assert_eq!(
        verify_tree(&manifest, &first),
        Err(vec![TreeMismatch::Content {
            path: "one.txt".into(),
            expected: bella_core::ids::Digest::of_bytes(b"same bytes"),
        }])
    );
}

#[test]
fn the_tree_and_report_do_not_depend_on_the_thread_count() {
    let mut snap = Snap::default();
    for index in 0..60usize {
        let len = match index % 6 {
            0 => 0,
            1 => 1,
            2 => 65_536,
            3 => 65_537,
            4 => 200_000,
            _ => index * 37,
        };
        let mut bytes = pattern(len);
        bytes.extend_from_slice(index.to_string().as_bytes());
        snap.file(&format!("d{}/f{index:03}", index % 7), &bytes);
    }
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let mut reports = Vec::new();
    for threads in [0, 1, 2, 8, 64] {
        let dest = tmp.path().join(format!("t{threads}"));
        let report = materialize(
            &manifest,
            &snap.store,
            &dest,
            &MaterializeOptions {
                threads,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(verify_tree(&manifest, &dest), Ok(()), "threads={threads}");
        reports.push((report.files, report.symlinks, report.dirs, report.bytes));
    }
    assert!(reports.iter().all(|report| *report == reports[0]));
    assert_eq!(reports[0], (60, 0, 7, manifest.total_bytes()));
}
