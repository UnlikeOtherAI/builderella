//! Git working trees: what is listed and which bytes are captured.

use super::*;

fn git_meta(result: &CaptureResult) -> &bella_core::snapshot::GitProvenance {
    result.meta.roots[0].git.as_ref().expect("a git root")
}

/// a26: the index and the working tree disagree; the working tree is what
/// a build would see, so its bytes are captured. Nothing git owns changes.
#[test]
fn a26_working_tree_bytes_win_over_the_index() {
    let repo = Repo::new();
    repo.write("file.txt", b"committed\n");
    repo.commit_all("init");
    repo.write("file.txt", b"staged\n");
    repo.git(&["add", "file.txt"]);
    repo.write("file.txt", b"working\n");

    let index_before = fs::read(repo.path(".git/index")).unwrap();
    let head_before = repo.git(&["rev-parse", "HEAD"]);
    let refs_before = repo.git(&["for-each-ref"]);

    let store = MemStore::new();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();

    assert_eq!(content(&store, &result, "file.txt"), b"working\n");
    assert_eq!(paths(&result), ["file.txt"]);
    assert!(git_meta(&result).dirty);

    assert_eq!(fs::read(repo.path(".git/index")).unwrap(), index_before);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head_before);
    assert_eq!(repo.git(&["for-each-ref"]), refs_before);
    assert_eq!(repo.git(&["show", ":file.txt"]), "staged");
    assert_eq!(repo.git(&["stash", "list"]), "");
    assert_eq!(fs::read(repo.path("file.txt")).unwrap(), b"working\n");
}

/// a27: a tracked file deleted from the working tree, without staging the
/// deletion, is not in the snapshot.
#[test]
fn a27_tracked_file_deleted_on_disk_is_absent() {
    let repo = Repo::new();
    repo.write("keep.txt", b"keep");
    repo.write("gone.txt", b"gone");
    repo.write("dir/inner.txt", b"inner");
    repo.commit_all("init");
    fs::remove_file(repo.path("gone.txt")).unwrap();
    fs::remove_dir_all(repo.path("dir")).unwrap();

    let store = MemStore::new();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();

    assert_eq!(paths(&result), ["keep.txt"]);
    assert_eq!(store.len(), 1);
    assert_eq!(result.stats.files, 1);
    // The index still lists them: the capture did not stage the deletion.
    assert!(repo.git(&["ls-files"]).contains("gone.txt"));
}

/// a27: a tracked file replaced by a directory is absent; the new files
/// inside are captured as untracked files.
#[test]
fn a27_tracked_file_replaced_by_a_directory() {
    let repo = Repo::new();
    repo.write("thing", b"a file");
    repo.commit_all("init");
    fs::remove_file(repo.path("thing")).unwrap();
    repo.write("thing/inner.txt", b"now a directory");

    let result = snap_default(&repo.dir);
    assert_eq!(paths(&result), ["thing/inner.txt"]);
}

/// a28: spaces, quotes, newlines and non-ASCII characters in names reach
/// the manifest exactly, for tracked and untracked files.
#[test]
fn a28_unusual_names_survive() {
    let mut tracked = vec![
        "dir with space/ünï côdé.txt",
        "it's here.txt",
        "日本語/ファイル.txt",
        "emoji 🎉.md",
    ];
    let mut untracked = vec!["un tracked ✓.txt"];
    if cfg!(unix) {
        tracked.extend([
            "new\nline.txt",
            "quo\"te.txt",
            "tab\there.txt",
            "back\\slash.txt",
            "trailing space ",
        ]);
        untracked.push("un\ntracked \"q\".txt");
    }
    let repo = Repo::new();
    for name in &tracked {
        repo.write(name, name.as_bytes());
    }
    repo.commit_all("init");
    for name in &untracked {
        repo.write(name, name.as_bytes());
    }

    let store = MemStore::new();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();

    let mut expected: Vec<&str> = tracked.iter().chain(&untracked).copied().collect();
    expected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(paths(&result), expected);
    for name in expected {
        assert_eq!(content(&store, &result, name), name.as_bytes(), "{name:?}");
    }
    assert_eq!(
        result.meta.windows_incompatible.is_some(),
        cfg!(unix),
        "{:?}",
        result.meta.windows_incompatible
    );
    result.manifest.validate().unwrap();
}

/// a29: making a file executable changes nothing but its mode, and the
/// snapshot id still changes. Windows has no mode bits, so the bit comes
/// from the git index there.
#[test]
fn a29_chmod_only_change_changes_the_snapshot_id() {
    let repo = Repo::new();
    repo.write("run.sh", b"#!/bin/sh\necho hi\n");
    repo.write("data.txt", b"data");
    repo.commit_all("init");
    let before = snap_default(&repo.dir);
    assert!(matches!(
        entry(&before, "run.sh"),
        Some(Entry::File {
            executable: false,
            ..
        })
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(repo.path("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    repo.git(&["update-index", "--chmod=+x", "run.sh"]);

    let after = snap_default(&repo.dir);
    assert!(matches!(
        entry(&after, "run.sh"),
        Some(Entry::File {
            executable: true,
            ..
        })
    ));
    assert!(matches!(
        entry(&after, "data.txt"),
        Some(Entry::File {
            executable: false,
            ..
        })
    ));
    assert_ne!(before.snapshot_id, after.snapshot_id);
}

/// a29: an untracked file has no index entry, so on a filesystem without
/// mode bits it is never executable; with them, the file mode decides.
#[test]
fn a29_untracked_executable_bit() {
    let repo = Repo::new();
    repo.write("tracked.txt", b"t");
    repo.commit_all("init");
    repo.write("tool.sh", b"#!/bin/sh\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(repo.path("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let result = snap_default(&repo.dir);
    let Some(Entry::File { executable, .. }) = entry(&result, "tool.sh") else {
        panic!("tool.sh was not captured");
    };
    assert_eq!(*executable, cfg!(unix));
}

/// a29: retargeting a symlink changes nothing but the link, and the
/// snapshot id still changes. The link is stored as a link.
#[test]
fn a29_symlink_only_change_changes_the_snapshot_id() {
    let repo = Repo::new();
    repo.write("a.txt", b"a");
    repo.write("b.txt", b"b");
    repo.commit_all("init");
    if !symlink("a.txt", &repo.path("link"), false) {
        return;
    }
    let store = MemStore::new();
    let before = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(
        entry(&before, "link"),
        Some(&Entry::Symlink {
            path: "link".into(),
            target: "a.txt".into()
        })
    );
    assert!(before.meta.has_symlinks);
    assert_eq!(before.stats.symlinks, 1);
    // The link is not dereferenced: two files, two objects.
    assert_eq!(store.len(), 2);

    fs::remove_file(repo.path("link")).unwrap();
    assert!(symlink("b.txt", &repo.path("link"), false));
    let after = snap_default(&repo.dir);
    assert_eq!(
        entry(&after, "link"),
        Some(&Entry::Symlink {
            path: "link".into(),
            target: "b.txt".into()
        })
    );
    assert_ne!(before.snapshot_id, after.snapshot_id);
}

/// a29: a checkout without symlink support (`core.symlinks=false`, the
/// usual case on Windows) holds a link as a small file with the target in
/// it. Git treats that file as the link and so does the capture.
#[test]
fn a29_symlink_placeholder_is_recorded_as_a_link() {
    let repo = Repo::new();
    repo.git(&["config", "core.symlinks", "false"]);
    repo.write("docs/target.txt", b"content");
    repo.write("docs/link", b"target.txt");
    let blob = repo.git(&["hash-object", "-w", "docs/link"]);
    repo.git(&["add", "docs/target.txt"]);
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("120000,{blob},docs/link"),
    ]);
    repo.git(&["commit", "-q", "-m", "init"]);

    let store = MemStore::new();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(
        entry(&result, "docs/link"),
        Some(&Entry::Symlink {
            path: "docs/link".into(),
            target: "target.txt".into()
        })
    );
    assert!(result.meta.has_symlinks);
    assert_eq!(store.len(), 1);

    // A placeholder that points out of the tree fails like a real link.
    repo.write("docs/link", b"../../outside");
    let err = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap_err();
    assert!(
        matches!(&err, CaptureError::Symlink { path, .. } if path == "docs/link"),
        "{err}"
    );
}

/// The root may be a subdirectory of a repository: only that subtree is
/// captured, with paths relative to it.
#[test]
fn subdirectory_of_a_repository_is_a_root() {
    let repo = Repo::new();
    repo.write("pkg/a.txt", b"a");
    repo.write("pkg/sub/b.txt", b"b");
    repo.write("other/c.txt", b"c");
    repo.write(".gitignore", b"*.log\n");
    repo.commit_all("init");
    repo.write("pkg/new.txt", b"new");
    repo.write("pkg/debug.log", b"ignored by the rule at the top");
    repo.write("other/new.txt", b"outside");

    let store = MemStore::new();
    let result = snap(&repo.path("pkg"), &CapturePolicy::default(), &store).unwrap();
    assert_eq!(paths(&result), ["a.txt", "new.txt", "sub/b.txt"]);
    assert_eq!(content(&store, &result, "sub/b.txt"), b"b");
    assert_eq!(result.meta.roots[0].label, "pkg");
    assert!(git_meta(&result).dirty);
}

/// The root may be a linked `git worktree`, whose `.git` is a file.
#[test]
fn linked_worktree_is_a_root() {
    let repo = Repo::new();
    repo.write("f.txt", b"main version");
    repo.commit_all("init");
    let wt = repo.sibling("wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feature",
        wt.to_str().unwrap(),
    ]);
    write(&wt.join("f.txt"), b"feature version");
    write(&wt.join("untracked.txt"), b"u");

    let store = MemStore::new();
    let result = snap(&wt, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(paths(&result), ["f.txt", "untracked.txt"]);
    assert_eq!(content(&store, &result, "f.txt"), b"feature version");
    assert_eq!(git_meta(&result).branch.as_deref(), Some("feature"));
    assert_eq!(result.meta.roots[0].label, "wt");
}

/// `.git` never appears in a snapshot, whatever the policy includes.
#[test]
fn git_directory_is_never_captured() {
    let repo = Repo::new();
    repo.write("f.txt", b"f");
    repo.write(".gitignore", b"cache/\n");
    repo.commit_all("init");
    repo.write("cache/x", b"x");
    let policy = CapturePolicy {
        include_ignored: vec!["**".into()],
        ..CapturePolicy::default()
    };
    let result = snap(&repo.dir, &policy, &MemStore::new()).unwrap();
    assert_eq!(paths(&result), [".gitignore", "cache/x", "f.txt"]);
}

#[test]
fn provenance_records_head_branch_dirty_and_remote() {
    let repo = Repo::new();
    repo.write("f.txt", b"f");
    repo.commit_all("init");
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://someone:s3cret-token@example.invalid/org/repo.git",
    ]);

    let clean = snap_default(&repo.dir);
    let git = git_meta(&clean);
    assert_eq!(
        git.head.as_deref(),
        Some(repo.git(&["rev-parse", "HEAD"]).as_str())
    );
    assert_eq!(git.branch.as_deref(), Some("main"));
    assert!(!git.dirty);
    assert_eq!(
        git.remote_url.as_deref(),
        Some("https://example.invalid/org/repo.git")
    );
    assert_eq!(clean.meta.roots[0].mount, ".");
    assert_eq!(clean.meta.roots[0].label, "repo");

    repo.write("new.txt", b"untracked");
    assert!(git_meta(&snap_default(&repo.dir)).dirty);

    // Detached: a head but no branch.
    fs::remove_file(repo.path("new.txt")).unwrap();
    repo.git(&["checkout", "-q", "--detach"]);
    let detached = snap_default(&repo.dir);
    assert!(git_meta(&detached).head.is_some());
    assert_eq!(git_meta(&detached).branch, None);
    // Provenance is not identity: the tree is the same, so is the id.
    assert_eq!(detached.snapshot_id, clean.snapshot_id);
}

#[test]
fn repository_without_commits_is_captured() {
    let repo = Repo::new();
    repo.write("first.txt", b"not committed yet");
    let result = snap_default(&repo.dir);
    assert_eq!(paths(&result), ["first.txt"]);
    let git = git_meta(&result);
    assert_eq!(git.head, None);
    assert_eq!(git.branch.as_deref(), Some("main"));
    assert!(git.dirty);
}

const LFS_POINTER: &[u8] = b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
size 123456\n";

/// a32: a pointer file where the LFS filter applies means the real object
/// was never downloaded. Building from the pointer would be wrong.
#[test]
fn a32_lfs_pointer_is_not_a_hydrated_object() {
    let repo = Repo::new();
    repo.write(
        ".gitattributes",
        b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );
    repo.write("readme.txt", b"hello");
    repo.commit_all("init");

    // Real content in an LFS path is fine.
    repo.write("assets/model.bin", b"\x00\x01real bytes");
    // A pointer outside any LFS path is just a file.
    repo.write("docs/example-pointer.txt", LFS_POINTER);
    let store = MemStore::new();
    let result = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(
        content(&store, &result, "docs/example-pointer.txt"),
        LFS_POINTER
    );
    assert!(entry(&result, "assets/model.bin").is_some());

    repo.write("assets/model.bin", LFS_POINTER);
    let err = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap_err();
    assert!(
        matches!(&err, CaptureError::LfsNotHydrated { path } if path == "assets/model.bin"),
        "{err}"
    );
    assert!(err.to_string().contains("LFS object not hydrated"), "{err}");
}

/// Parent repository with `vendor/lib` as a submodule. Returns the parent
/// and the repository the submodule was added from.
fn with_submodule() -> (Repo, Repo) {
    let app = Repo::new();
    let lib = Repo::adopt({
        let dir = app.sibling("lib-origin");
        fs::create_dir(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        dir
    });
    lib.write("lib.c", b"int lib(void) { return 1; }\n");
    lib.write(".gitignore", b"*.o\n");
    lib.commit_all("lib");

    app.write("main.c", b"int main(void) { return 0; }\n");
    let url = lib.dir.to_str().unwrap().replace('\\', "/");
    app.git(&["submodule", "--quiet", "add", &url, "vendor/lib"]);
    app.commit_all("app with submodule");
    (app, lib)
}

/// a32: an initialised submodule is captured from its own working tree,
/// uncommitted edits and untracked files included, under its path.
#[test]
fn a32_initialised_submodule_is_captured_from_its_working_tree() {
    let (app, _lib) = with_submodule();
    let sub = Repo::adopt(app.path("vendor/lib"));
    sub.write("lib.c", b"int lib(void) { return 2; } /* edited */\n");
    sub.write("extra.c", b"/* untracked in the submodule */\n");
    sub.write("lib.o", b"ignored by the submodule's own rules");

    let store = MemStore::new();
    let result = snap(&app.dir, &CapturePolicy::default(), &store).unwrap();
    assert_eq!(
        paths(&result),
        [
            ".gitmodules",
            "main.c",
            "vendor/lib/.gitignore",
            "vendor/lib/extra.c",
            "vendor/lib/lib.c",
        ]
    );
    assert_eq!(
        content(&store, &result, "vendor/lib/lib.c"),
        b"int lib(void) { return 2; } /* edited */\n"
    );

    // Excluding the submodule leaves it out without inspecting it.
    let policy = CapturePolicy {
        exclude: vec!["vendor/lib".into()],
        ..CapturePolicy::default()
    };
    let without = snap(&app.dir, &policy, &store).unwrap();
    assert_eq!(paths(&without), [".gitmodules", "main.c"]);
}

/// a32: a clone that never ran `git submodule update --init` has an empty
/// directory where the submodule belongs. The snapshot would be missing
/// source, so the capture fails and names the submodule.
#[test]
fn a32_uninitialised_submodule_fails() {
    let (app, _lib) = with_submodule();
    let clone = app.sibling("clone");
    git(
        app.dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            app.dir.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let clone = Repo::adopt(clone);

    let store = MemStore::new();
    let err = snap(&clone.dir, &CapturePolicy::default(), &store).unwrap_err();
    assert!(
        matches!(&err, CaptureError::SubmoduleNotInitialised { path } if path == "vendor/lib"),
        "{err}"
    );
    assert!(store.is_empty(), "nothing is read before the listing fails");
}

/// a32: a sparse checkout leaves tracked paths out of the working tree.
/// That is not a deletion; the capture fails unless the policy excludes
/// every missing path.
#[test]
fn a32_sparse_checkout_with_missing_paths_fails_unless_excluded() {
    let repo = Repo::new();
    repo.write("top.txt", b"top");
    repo.write("kept/one.txt", b"one");
    repo.write("dropped/two.txt", b"two");
    repo.write("dropped/deep/three.txt", b"three");
    repo.commit_all("init");
    repo.git(&["sparse-checkout", "init", "--cone"]);
    repo.git(&["sparse-checkout", "set", "kept"]);
    assert!(!repo.path("dropped/two.txt").exists());

    let store = MemStore::new();
    let err = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap_err();
    match &err {
        CaptureError::SparseCheckout { count, missing } => {
            assert_eq!(*count, 2);
            assert_eq!(missing, &["dropped/deep/three.txt", "dropped/two.txt"]);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(err.to_string().contains("dropped/deep/three.txt"), "{err}");
    assert!(store.is_empty());

    // Covering only part of what is missing is not enough.
    let partly = CapturePolicy {
        exclude: vec!["dropped/deep".into()],
        ..CapturePolicy::default()
    };
    assert!(matches!(
        snap(&repo.dir, &partly, &store),
        Err(CaptureError::SparseCheckout { count: 1, .. })
    ));

    let policy = CapturePolicy {
        exclude: vec!["dropped".into()],
        ..CapturePolicy::default()
    };
    let result = snap(&repo.dir, &policy, &store).unwrap();
    assert_eq!(paths(&result), ["kept/one.txt", "top.txt"]);
}

/// An untracked directory that is its own repository is captured from its
/// working tree by that repository's rules, and the result says so.
#[test]
fn untracked_nested_repository_is_captured_with_a_warning() {
    let repo = Repo::new();
    repo.write("main.txt", b"main");
    repo.commit_all("init");
    let nested = Repo::adopt({
        let dir = repo.path("vendor/dep");
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        dir
    });
    nested.write(".gitignore", b"*.log\n");
    nested.write("kept.txt", b"kept");
    nested.write("noise.log", b"ignored inside the nested repository");

    let result = snap_default(&repo.dir);
    assert_eq!(
        paths(&result),
        ["main.txt", "vendor/dep/.gitignore", "vendor/dep/kept.txt"]
    );
    assert!(
        result.warnings.iter().any(|w| w.contains("vendor/dep")),
        "{:?}",
        result.warnings
    );
}

/// A tracked path below a directory that was replaced by a symlink is not
/// read through the link: git considers it deleted, and following the link
/// would capture files from wherever it points.
#[cfg(unix)]
#[test]
fn tracked_path_behind_a_symlinked_directory_is_absent() {
    let repo = Repo::new();
    repo.write("real/f.txt", b"real");
    repo.write("moved/f.txt", b"must not be captured as real/f.txt twice");
    repo.commit_all("init");
    fs::remove_dir_all(repo.path("real")).unwrap();
    assert!(symlink("moved", &repo.path("real"), true));

    let result = snap_default(&repo.dir);
    assert_eq!(paths(&result), ["moved/f.txt", "real"]);
    assert_eq!(
        entry(&result, "real"),
        Some(&Entry::Symlink {
            path: "real".into(),
            target: "moved".into()
        })
    );
}

#[test]
fn untracked_files_are_left_out_when_the_policy_says_so() {
    let repo = Repo::new();
    repo.write("tracked.txt", b"t");
    repo.commit_all("init");
    repo.write("untracked.txt", b"u");
    repo.write(".env", b"SECRET=1");
    let policy = CapturePolicy {
        include_untracked: false,
        ..CapturePolicy::default()
    };
    let result = snap(&repo.dir, &policy, &MemStore::new()).unwrap();
    assert_eq!(paths(&result), ["tracked.txt"]);
    // Not selected, so not reported as withheld either.
    assert!(result.excluded_secrets.is_empty());
    assert!(!git_meta(&result).dirty);
}
