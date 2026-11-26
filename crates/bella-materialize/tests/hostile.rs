//! A36: a manifest or store that lies never yields a tree, and never leaves
//! part of one behind.

mod common;

use bella_core::ids::Digest;
use bella_core::snapshot::{Entry, MANIFEST_VERSION, Manifest, ManifestError, RootManifest};
use bella_core::store::{MemStore, StoreError, put_bytes};
use bella_materialize::{MaterializeError, MaterializeOptions, materialize, verify_tree};
use common::{HostileStore, Snap, is_empty_dir, options, pattern, single_root};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn raw(mount: &str, entries: Vec<Entry>) -> Manifest {
    Manifest {
        version: MANIFEST_VERSION,
        roots: vec![RootManifest {
            mount: mount.into(),
            entries,
        }],
    }
}

fn file_entry(store: &MemStore, path: &str, bytes: &[u8]) -> Entry {
    Entry::File {
        path: path.into(),
        digest: put_bytes(store, bytes).unwrap(),
        size: bytes.len() as u64,
        executable: false,
    }
}

#[test]
fn a36_traversal_and_absolute_paths_are_rejected_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let sentinel = tmp.path().join("sentinel");
    fs::write(&sentinel, b"outside the destination").unwrap();
    let store = MemStore::new();

    for bad in [
        "../sentinel",
        "a/../../sentinel",
        "/etc/passwd",
        "a//b",
        "a/./b",
        "..",
        "",
    ] {
        let dest = tmp.path().join("dest");
        let manifest = raw(".", vec![file_entry(&store, bad, b"overwritten")]);
        let error = materialize(&manifest, &store, &dest, &options()).unwrap_err();
        assert!(
            matches!(error, MaterializeError::InvalidManifest(_)),
            "{bad:?}: {error}"
        );
        assert!(!dest.exists(), "{bad:?} created the destination");
    }
    for bad_mount in ["../up", "/abs", "a/../b"] {
        let dest = tmp.path().join("dest");
        let manifest = raw(bad_mount, vec![file_entry(&store, "x", b"x")]);
        let error = materialize(&manifest, &store, &dest, &options()).unwrap_err();
        assert!(
            matches!(error, MaterializeError::InvalidManifest(_)),
            "{bad_mount:?}: {error}"
        );
        assert!(!dest.exists());
    }
    let mut wrong_version = raw(".", vec![file_entry(&store, "x", b"x")]);
    wrong_version.version = MANIFEST_VERSION + 1;
    assert!(matches!(
        materialize(&wrong_version, &store, &tmp.path().join("dest"), &options()),
        Err(MaterializeError::InvalidManifest(_))
    ));

    assert_eq!(fs::read(&sentinel).unwrap(), b"outside the destination");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn a36_escaping_and_absolute_symlink_targets_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemStore::new();
    let dest = tmp.path().join("dest");

    for (link, target) in [
        ("link", "/etc/passwd"),
        ("link", "../outside"),
        ("a/link", "../../outside"),
        ("link", "sub/../../outside"),
        ("link", "C:\\Windows"),
        ("link", "\\\\server\\share"),
        ("link", ""),
    ] {
        let manifest = raw(
            ".",
            vec![
                Entry::Symlink {
                    path: link.into(),
                    target: target.into(),
                },
                file_entry(&store, "sub/x", b"x"),
            ],
        );
        let error = materialize(&manifest, &store, &dest, &options()).unwrap_err();
        assert!(
            matches!(error, MaterializeError::InvalidManifest(_)),
            "{link} -> {target:?}: {error}"
        );
        assert!(!dest.exists(), "{target:?} created the destination");
    }

    // Each target stays inside the root on its own, but `out` goes through
    // `a/b/up` (the root) and then one level higher.
    let mut snap = Snap::default();
    snap.file("a/b/f", b"f");
    snap.link("a/b/up", "../..");
    snap.link("out", "a/b/up/..");
    let manifest = snap.manifest();
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::SymlinkEscapes { path, .. }) if path == "out"
    ));
    let error = materialize(&manifest, &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::SymlinkEscapes { path, target } if path == "out" && target == "a/b/up/.."
        ),
        "{error}"
    );
    assert!(!dest.exists());
}

/// The link that leaves its root through another link is named by its path
/// in the destination, mount included.
#[test]
fn a36_symlink_chain_in_a_mounted_root_is_named_with_its_mount() {
    let store = MemStore::new();
    let link = |path: &str, target: &str| Entry::Symlink {
        path: path.into(),
        target: target.into(),
    };
    let manifest = Manifest::new(vec![
        RootManifest {
            mount: "app".into(),
            entries: vec![
                file_entry(&store, "sub/x", b"x"),
                link("sub/top", ".."),
                link("up", "sub/top/../lib/y"),
            ],
        },
        RootManifest {
            mount: "lib".into(),
            entries: vec![file_entry(&store, "y", b"y")],
        },
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("ws");
    let error = materialize(&manifest, &store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::SymlinkEscapes { path, target }
                if path == "app/up" && target == "sub/top/../lib/y"
        ),
        "{error}"
    );
    assert!(!dest.exists());
}

/// Twenty files; the caller damages the object of `d1/f07`.
fn twenty_files() -> (Snap, Digest) {
    let mut snap = Snap::default();
    let mut victim = None;
    for index in 0..20usize {
        let path = format!("d{}/f{index:02}", index % 3);
        let mut bytes = pattern(1000 + index);
        bytes.push(index as u8);
        let digest = snap.file(&path, &bytes);
        if index == 7 {
            victim = Some(digest);
        }
    }
    (snap, victim.unwrap())
}

#[test]
fn a36_missing_object_is_an_error_naming_the_path_and_leaves_nothing() {
    let (snap, victim) = twenty_files();
    snap.store.remove(&victim);
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();

    let dest = tmp.path().join("absent-before");
    let error = materialize(&manifest, &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::MissingObject { path, digest } if path == "d1/f07" && *digest == victim
        ),
        "{error}"
    );
    assert!(error.to_string().contains("d1/f07"));
    assert!(!dest.exists(), "a partial tree was left behind");

    // A destination that existed empty is handed back empty.
    let dest = tmp.path().join("empty-before");
    fs::create_dir(&dest).unwrap();
    assert!(matches!(
        materialize(&manifest, &snap.store, &dest, &options()),
        Err(MaterializeError::MissingObject { .. })
    ));
    assert!(dest.is_dir() && is_empty_dir(&dest));
}

#[test]
fn a36_corrupt_object_is_rejected_and_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();

    // Same length, one byte changed.
    let (snap, victim) = twenty_files();
    let mut tampered = pattern(1007);
    tampered.push(7);
    tampered[500] ^= 0x01;
    snap.store.corrupt(&victim, tampered);
    let dest = tmp.path().join("same-length");
    let error = materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::DigestMismatch { path, digest } if path == "d1/f07" && *digest == victim
        ),
        "{error}"
    );
    assert!(!dest.exists());

    // Shorter than declared: the store's own verification fails the read.
    let (snap, victim) = twenty_files();
    snap.store.corrupt(&victim, b"short".to_vec());
    let dest = tmp.path().join("shorter");
    let error = materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(&error, MaterializeError::DigestMismatch { path, .. } if path == "d1/f07"),
        "{error}"
    );
    assert!(!dest.exists());

    // Longer than declared: reading stops at the declared size.
    let (snap, victim) = twenty_files();
    snap.store.corrupt(&victim, pattern(50_000));
    let dest = tmp.path().join("longer");
    let error = materialize(&snap.manifest(), &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::ObjectTooLong { path, expected: 1008, .. } if path == "d1/f07"
        ),
        "{error}"
    );
    assert!(!dest.exists());
}

#[test]
fn a36_object_longer_or_shorter_than_the_manifest_size_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemStore::new();
    let digest = put_bytes(&store, b"12345").unwrap();
    let lying = |size: u64| {
        single_root(vec![
            file_entry(&store, "ok.txt", b"fine"),
            Entry::File {
                path: "sub/lie.bin".into(),
                digest: digest.clone(),
                size,
                executable: false,
            },
        ])
    };

    let dest = tmp.path().join("longer");
    let error = materialize(&lying(3), &store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::ObjectTooLong { path, expected: 3, .. } if path == "sub/lie.bin"
        ),
        "{error}"
    );
    assert!(!dest.exists());

    let error = materialize(&lying(0), &store, &dest, &options()).unwrap_err();
    assert!(
        matches!(&error, MaterializeError::ObjectTooLong { expected: 0, .. }),
        "{error}"
    );
    assert!(!dest.exists());

    let error = materialize(&lying(10), &store, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::ObjectTooShort { path, expected: 10, actual: 5, .. }
                if path == "sub/lie.bin"
        ),
        "{error}"
    );
    assert!(!dest.exists());

    // The honest size works, into the same place.
    let honest = lying(5);
    materialize(&honest, &store, &dest, &options()).unwrap();
    assert_eq!(verify_tree(&honest, &dest), Ok(()));
}

#[test]
fn a36_reading_stops_at_the_declared_size() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("bomb");
    let manifest = single_root(vec![Entry::File {
        path: "bomb.bin".into(),
        digest: Digest::of_bytes(b"whatever"),
        size: 100_000,
        executable: false,
    }]);

    // An object that never ends.
    let served = Arc::new(AtomicU64::new(0));
    let endless = HostileStore {
        good: 0,
        then_fail: false,
        served: Arc::clone(&served),
    };
    let error = materialize(&manifest, &endless, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::ObjectTooLong { path, expected: 100_000, .. } if path == "bomb.bin"
        ),
        "{error}"
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        100_001,
        "exactly one byte past the declared size tells that the object is longer"
    );
    assert!(!dest.exists());

    // A store that fails halfway through.
    let failing = HostileStore {
        good: 40_000,
        then_fail: true,
        served: Arc::new(AtomicU64::new(0)),
    };
    let error = materialize(&manifest, &failing, &dest, &options()).unwrap_err();
    assert!(
        matches!(
            &error,
            MaterializeError::Store { path, source: StoreError::Io(_), .. } if path == "bomb.bin"
        ),
        "{error}"
    );
    assert!(!dest.exists());
}

#[test]
fn a36_total_bytes_and_entry_count_limits_are_enforced_before_writing() {
    let mut snap = Snap::default();
    snap.file("a", &[1u8; 10]);
    snap.file("b", &[2u8; 10]);
    snap.file("c", &[3u8; 10]);
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("limited");
    let limits = |max_total_bytes, max_files| MaterializeOptions {
        max_total_bytes,
        max_files,
        threads: 2,
    };

    let error = materialize(&manifest, &snap.store, &dest, &limits(29, 100)).unwrap_err();
    assert!(
        matches!(
            error,
            MaterializeError::TooManyBytes {
                bytes: 30,
                limit: 29
            }
        ),
        "{error}"
    );
    assert!(!dest.exists());

    let error = materialize(&manifest, &snap.store, &dest, &limits(1000, 2)).unwrap_err();
    assert!(
        matches!(
            error,
            MaterializeError::TooManyEntries {
                entries: 3,
                limit: 2
            }
        ),
        "{error}"
    );
    assert!(!dest.exists());

    // Directories and links cost inodes too, so they count.
    let mut inodes = Snap::default();
    inodes.file("f", b"");
    inodes.dir("d1");
    inodes.dir("d2");
    let error = materialize(&inodes.manifest(), &inodes.store, &dest, &limits(0, 2)).unwrap_err();
    assert!(
        matches!(error, MaterializeError::TooManyEntries { entries: 3, .. }),
        "{error}"
    );
    assert!(!dest.exists());

    // Declared sizes that overflow a u64 are over the limit, not a wrap to
    // a small number.
    let huge = |path: &str| Entry::File {
        path: path.into(),
        digest: Digest::of_bytes(b"x"),
        size: u64::MAX,
        executable: false,
    };
    let overflow = single_root(vec![huge("x"), huge("y"), huge("z")]);
    let error = materialize(&overflow, &snap.store, &dest, &limits(u64::MAX - 1, 100)).unwrap_err();
    assert!(
        matches!(
            error,
            MaterializeError::TooManyBytes {
                bytes: u64::MAX,
                ..
            }
        ),
        "{error}"
    );
    assert!(!dest.exists());

    // Exactly at both limits is allowed.
    materialize(&manifest, &snap.store, &dest, &limits(30, 3)).unwrap();
    assert_eq!(verify_tree(&manifest, &dest), Ok(()));
}

#[test]
fn a36_the_reported_error_does_not_depend_on_the_thread_count() {
    let mut snap = Snap::default();
    let mut digests = Vec::new();
    for index in 0..80usize {
        let mut bytes = pattern(30_000 + index);
        bytes.push(index as u8);
        digests.push(snap.file(&format!("f{index:03}"), &bytes));
    }
    // Three bad files; `f011` sorts first.
    snap.store.remove(&digests[40]);
    snap.store.corrupt(&digests[11], pattern(30_012));
    snap.store.remove(&digests[63]);
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();

    for threads in [1, 2, 3, 8, 32] {
        for round in 0..5 {
            let dest = tmp.path().join(format!("t{threads}-{round}"));
            let error = materialize(
                &manifest,
                &snap.store,
                &dest,
                &MaterializeOptions {
                    threads,
                    ..options()
                },
            )
            .unwrap_err();
            assert!(
                matches!(&error, MaterializeError::DigestMismatch { path, .. } if path == "f011"),
                "threads={threads} round={round}: {error}"
            );
            assert!(!dest.exists(), "threads={threads}: a partial tree was left");
        }
    }
}

#[test]
fn a36_directories_implied_by_deep_paths_count_against_the_limit() {
    let mut snap = Snap::default();
    // One entry, six objects on disk: five directories and the file.
    snap.file("a/b/c/d/e/f", b"deep");
    let manifest = snap.manifest();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("deep");
    let limit = |max_files| MaterializeOptions {
        max_files,
        ..options()
    };

    let error = materialize(&manifest, &snap.store, &dest, &limit(5)).unwrap_err();
    assert!(
        matches!(error, MaterializeError::TooManyEntries { limit: 5, .. }),
        "{error}"
    );
    assert!(!dest.exists());

    let report = materialize(&manifest, &snap.store, &dest, &limit(6)).unwrap();
    assert_eq!((report.files, report.dirs), (1, 5));
}

#[test]
fn a36_entangled_symlinks_are_refused_instead_of_resolved_forever() {
    let mut snap = Snap::default();
    snap.file("b/f", b"f");
    // `l0` wanders in and out of `b`; every other link goes through `l0`
    // until the hop limit, which costs thousands of steps per link.
    snap.link("l0", &format!("{}b", "b/../".repeat(50)));
    let through_l0 = format!("{}l0", "l0/../".repeat(42));
    for index in 1..=400 {
        snap.link(&format!("l{index}"), &through_l0);
    }
    let manifest = snap.manifest();
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::SymlinksTooComplex { .. })
    ));

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("links");
    let error = materialize(&manifest, &snap.store, &dest, &options()).unwrap_err();
    assert!(
        matches!(&error, MaterializeError::SymlinksTooComplex { path } if path.starts_with('l')),
        "{error}"
    );
    assert!(!dest.exists());
}
