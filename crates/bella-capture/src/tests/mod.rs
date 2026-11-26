//! Tests against real temporary directories and git repositories.

mod consistency;
mod git_trees;
mod plain;
mod policy;

use crate::{CaptureError, CapturePolicy, CaptureResult, RootSpec, capture};
use bella_core::snapshot::Entry;
use bella_core::store::{MemStore, get_bytes};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Run git for test setup and return its trimmed output.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Capture Test",
            "-c",
            "user.email=capture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "advice.detachedHead=false",
        ])
        .args(args)
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A git working tree in a temporary directory.
struct Repo {
    dir: PathBuf,
    _tmp: Option<TempDir>,
}

impl Repo {
    /// A fresh repository on branch `main` with no commits.
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("repo");
        fs::create_dir(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        let repo = Self {
            dir,
            _tmp: Some(tmp),
        };
        repo.configure();
        repo
    }

    /// An existing checkout (a clone, a worktree, a submodule) next to or
    /// inside another test repository.
    fn adopt(dir: PathBuf) -> Self {
        let repo = Self { dir, _tmp: None };
        repo.configure();
        repo
    }

    /// Settings that keep the test independent of the machine's git
    /// configuration.
    fn configure(&self) {
        let no_excludes = self.dir.parent().unwrap().join("no-global-excludes");
        for (key, value) in [
            ("user.name", "Capture Test"),
            ("user.email", "capture@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.excludesFile", no_excludes.to_str().unwrap()),
        ] {
            git(&self.dir, &["config", key, value]);
        }
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.dir, args)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// A path beside the repository, inside the same temporary directory.
    fn sibling(&self, name: &str) -> PathBuf {
        self.dir.parent().unwrap().join(name)
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        write(&self.path(rel), bytes);
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A temporary directory that is not inside any git working tree, or
/// `None` (with a note) when the machine's temporary directory is.
fn plain_dir() -> Option<TempDir> {
    let tmp = TempDir::new().unwrap();
    let inside_git = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .is_ok_and(|output| output.status.success());
    if inside_git {
        eprintln!("skipped: the temporary directory is inside a git working tree");
        return None;
    }
    Some(tmp)
}

/// Create a symlink. Returns `false`, with a note, when Windows refuses
/// because the account may not create symlinks.
fn symlink(target: &str, link: &Path, to_directory: bool) -> bool {
    #[cfg(unix)]
    {
        let _ = to_directory;
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
        let target = target.replace('/', "\\");
        let made = if to_directory {
            symlink_dir(&target, link)
        } else {
            symlink_file(&target, link)
        };
        match made {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => {
                eprintln!("skipped: this account may not create symlinks on Windows");
                false
            }
            Err(e) => panic!("creating symlink {}: {e}", link.display()),
        }
    }
}

fn snap(
    dir: &Path,
    policy: &CapturePolicy,
    store: &MemStore,
) -> Result<CaptureResult, CaptureError> {
    capture(&[RootSpec::single(dir)], policy, store, None)
}

/// Capture with the default policy into a throwaway store.
fn snap_default(dir: &Path) -> CaptureResult {
    snap(dir, &CapturePolicy::default(), &MemStore::new()).unwrap()
}

/// Every path in the manifest, prefixed with its mount unless that is `.`.
fn paths(result: &CaptureResult) -> Vec<String> {
    let mut all = Vec::new();
    for root in &result.manifest.roots {
        for entry in &root.entries {
            all.push(if root.mount == "." {
                entry.path().to_string()
            } else {
                format!("{}/{}", root.mount, entry.path())
            });
        }
    }
    all
}

fn entry<'a>(result: &'a CaptureResult, path: &str) -> Option<&'a Entry> {
    result
        .manifest
        .roots
        .iter()
        .flat_map(|root| &root.entries)
        .find(|entry| entry.path() == path)
}

/// The stored bytes of one file of the snapshot.
fn content(store: &MemStore, result: &CaptureResult, path: &str) -> Vec<u8> {
    match entry(result, path) {
        Some(Entry::File { digest, size, .. }) => {
            let bytes = get_bytes(store, digest).unwrap();
            assert_eq!(bytes.len() as u64, *size, "{path}");
            bytes
        }
        other => panic!("{path} is not a file in the snapshot: {other:?}"),
    }
}

/// Every file of the snapshot with its stored bytes.
fn tree(store: &MemStore, result: &CaptureResult) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for root in &result.manifest.roots {
        for entry in &root.entries {
            if let Entry::File { path, digest, .. } = entry {
                let name = if root.mount == "." {
                    path.clone()
                } else {
                    format!("{}/{path}", root.mount)
                };
                files.insert(name, get_bytes(store, digest).unwrap());
            }
        }
    }
    files
}
