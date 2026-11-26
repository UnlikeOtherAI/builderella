//! Edits during a capture, concurrent captures, barriers and immutability.

use super::*;
use crate::engine::{self, Hooks, MAX_PASSES, READ_ATTEMPTS, Tuning};
use crate::{CaptureBarrier, capture_with_clock};
use bella_core::ids::Digest;
use bella_core::snapshot::Consistency;
use bella_core::store::{ObjectStore, StoreError};
use bella_core::time::{ManualClock, SystemClock, UnixMillis};
use std::any::Any;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

type OnOpen<'a> = Box<dyn Fn(&Path, &str, u32) + Sync + 'a>;
type OnVerify<'a> = Box<dyn Fn(u32) + Sync + 'a>;

/// Changes the tree at chosen moments of a capture.
#[derive(Default)]
struct Meddle<'a> {
    opened: Option<OnOpen<'a>>,
    verify: Option<OnVerify<'a>>,
}

impl Hooks for Meddle<'_> {
    fn file_opened(&self, path: &Path, rel: &str, attempt: u32) {
        if let Some(opened) = &self.opened {
            opened(path, rel, attempt);
        }
    }

    fn before_verify(&self, pass: u32) {
        if let Some(verify) = &self.verify {
            verify(pass);
        }
    }
}

fn meddled(
    dir: &Path,
    store: &dyn ObjectStore,
    hooks: &dyn Hooks,
) -> Result<CaptureResult, CaptureError> {
    let tuning = Tuning {
        hooks: Some(hooks),
        ..Tuning::default()
    };
    engine::run(
        &[RootSpec::single(dir)],
        &CapturePolicy::default(),
        store,
        None,
        &SystemClock,
        &tuning,
    )
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn two_files() -> Repo {
    let repo = Repo::new();
    repo.write("steady.txt", b"steady");
    repo.write("busy.txt", b"busy");
    repo.commit_all("init");
    repo
}

/// a30: a file that is modified during every read is never stored in a
/// torn state. After the bounded retries the capture fails and names it.
#[test]
fn a30_file_changing_during_every_read_fails_with_its_path() {
    let repo = two_files();
    let reads = AtomicU32::new(0);
    let hooks = Meddle {
        opened: Some(Box::new(|path, rel, _| {
            if rel == "busy.txt" {
                reads.fetch_add(1, Ordering::SeqCst);
                append(path, b"+");
            }
        })),
        ..Meddle::default()
    };
    let store = MemStore::new();
    let err = meddled(&repo.dir, &store, &hooks).unwrap_err();
    match &err {
        CaptureError::FileChanged { path, attempts } => {
            assert_eq!(path, "busy.txt");
            assert_eq!(*attempts, READ_ATTEMPTS);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(err.to_string().contains("busy.txt"));
    // One read and three retries, and none of those reads was stored.
    assert_eq!(reads.load(Ordering::SeqCst), 4);
    assert!(store.is_empty());
}

/// a30: one edit during the read costs one retry, and the snapshot holds
/// the version that was read whole.
#[test]
fn a30_one_change_during_a_read_is_retried() {
    let repo = two_files();
    let hooks = Meddle {
        opened: Some(Box::new(|path, rel, attempt| {
            if rel == "busy.txt" && attempt == 1 {
                append(path, b" and then some");
            }
        })),
        ..Meddle::default()
    };
    let store = MemStore::new();
    let result = meddled(&repo.dir, &store, &hooks).unwrap();
    assert_eq!(content(&store, &result, "busy.txt"), b"busy and then some");
    assert_eq!(content(&store, &result, "steady.txt"), b"steady");
    assert_eq!(result.stats.read_retries, 1);
    assert_eq!(result.stats.passes, 1);
}

/// a30: a file deleted between being listed and being read is retried and
/// then found absent, like any other deleted file.
#[test]
fn a30_file_deleted_while_it_is_read_is_absent() {
    let repo = two_files();
    let hooks = Meddle {
        opened: Some(Box::new(|path, rel, _| {
            if rel == "busy.txt" {
                // Windows refuses to delete an open file; emptying it is
                // the same kind of change for the reader.
                if fs::remove_file(path).is_err() {
                    fs::write(path, b"").unwrap();
                }
            }
        })),
        ..Meddle::default()
    };
    let store = MemStore::new();
    let outcome = meddled(&repo.dir, &store, &hooks);
    if repo.path("busy.txt").exists() {
        // The file was emptied on every attempt instead: it never settled.
        assert!(matches!(outcome, Err(CaptureError::FileChanged { .. })));
    } else {
        assert_eq!(paths(&outcome.unwrap()), ["steady.txt"]);
    }
}

/// a30: an edit to a file that was already read is caught by the pass over
/// the tree at the end, and the capture starts over with the new state.
#[test]
fn a30_edit_after_a_file_was_read_restarts_the_capture() {
    let repo = two_files();
    let hooks = Meddle {
        verify: Some(Box::new(|pass| {
            if pass == 1 {
                repo.write("busy.txt", b"edited while the capture ran");
            }
        })),
        ..Meddle::default()
    };
    let store = MemStore::new();
    let result = meddled(&repo.dir, &store, &hooks).unwrap();
    assert_eq!(
        content(&store, &result, "busy.txt"),
        b"edited while the capture ran"
    );
    assert_eq!(result.stats.passes, 2);
}

/// a30: a file created or deleted while the capture runs is noticed too;
/// the snapshot is never a mix of before and after.
#[test]
fn a30_created_and_deleted_files_restart_the_capture() {
    let repo = two_files();
    let hooks = Meddle {
        verify: Some(Box::new(|pass| {
            if pass == 1 {
                repo.write("late.txt", b"created late");
                fs::remove_file(repo.path("steady.txt")).unwrap();
            }
        })),
        ..Meddle::default()
    };
    let store = MemStore::new();
    let result = meddled(&repo.dir, &store, &hooks).unwrap();
    assert_eq!(paths(&result), ["busy.txt", "late.txt"]);
    assert_eq!(content(&store, &result, "late.txt"), b"created late");
    assert_eq!(result.stats.passes, 2);
}

/// a30: a tree that never holds still fails after a bounded number of
/// passes instead of looping or returning a mixed snapshot.
#[test]
fn a30_tree_that_never_settles_fails() {
    let repo = two_files();
    let passes = AtomicU32::new(0);
    let hooks = Meddle {
        verify: Some(Box::new(|_| {
            passes.fetch_add(1, Ordering::SeqCst);
            append(&repo.path("steady.txt"), b"!");
        })),
        ..Meddle::default()
    };
    let err = meddled(&repo.dir, &MemStore::new(), &hooks).unwrap_err();
    match &err {
        CaptureError::TreeChanged { path, passes } => {
            assert_eq!(path, "steady.txt");
            assert_eq!(*passes, MAX_PASSES);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(passes.load(Ordering::SeqCst), MAX_PASSES);
}

fn patterned(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i * 31 + seed * 7 + i / 251) as u8)
        .collect()
}

/// a31: captures of the same tree running at once in one process do not
/// trip over each other's temporary files. Every one of them produces the
/// same snapshot and every object is intact.
#[test]
fn a31_concurrent_captures_do_not_collide() {
    let repo = Repo::new();
    let mut expected = BTreeMap::new();
    for i in 0..6 {
        let name = format!("data/blob-{i}.bin");
        let bytes = patterned(150_000 + i * 1_000, i);
        repo.write(&name, &bytes);
        expected.insert(name, bytes);
    }
    repo.commit_all("init");
    for i in 0..3 {
        let name = format!("untracked-{i}.bin");
        let bytes = patterned(90_000, 100 + i);
        repo.write(&name, &bytes);
        expected.insert(name, bytes);
    }

    let store = MemStore::new();
    let policy = CapturePolicy::default();
    let roots = [RootSpec::single(&repo.dir)];
    let results: Vec<CaptureResult> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let (store, policy, roots) = (&store, &policy, &roots);
                scope.spawn(move || {
                    if worker % 2 == 0 {
                        // A small in-memory allowance forces every file
                        // through a temporary spool file.
                        let tuning = Tuning {
                            spool_memory: 4096,
                            hooks: None,
                        };
                        engine::run(roots, policy, store, None, &SystemClock, &tuning)
                    } else {
                        capture(roots, policy, store, None)
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect()
    });

    for result in &results {
        assert_eq!(result.snapshot_id, results[0].snapshot_id);
        assert_eq!(result.manifest, results[0].manifest);
        assert_eq!(result.stats.files, 9);
    }
    assert_eq!(tree(&store, &results[0]), expected);
    assert_eq!(store.len(), 9);
}

/// A store that notes whether the barrier was held at each write.
struct WatchedStore {
    inner: MemStore,
    barrier_held: Arc<AtomicBool>,
    puts_with_barrier: AtomicU32,
    puts_without_barrier: AtomicU32,
}

impl ObjectStore for WatchedStore {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        self.inner.contains(digest)
    }

    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        if self.barrier_held.load(Ordering::SeqCst) {
            self.puts_with_barrier.fetch_add(1, Ordering::SeqCst);
        } else {
            self.puts_without_barrier.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.put(reader, expected)
    }

    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        self.inner.open(digest)
    }

    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError> {
        self.inner.size(digest)
    }
}

struct TestBarrier {
    held: Arc<AtomicBool>,
    acquired: AtomicU32,
    refuse: bool,
}

struct Held(Arc<AtomicBool>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl CaptureBarrier for TestBarrier {
    fn acquire(&self) -> Result<Box<dyn Any + Send>, String> {
        self.acquired.fetch_add(1, Ordering::SeqCst);
        if self.refuse {
            return Err("a writer did not answer".into());
        }
        self.held.store(true, Ordering::SeqCst);
        Ok(Box::new(Held(self.held.clone())))
    }
}

/// a35: several roots are one snapshot taken under one barrier. A policy
/// that requires a coordinated capture fails without a barrier before
/// anything is read, and succeeds with one that is held throughout.
#[test]
fn a35_multiple_roots_are_captured_under_one_barrier() {
    let app = Repo::new();
    app.write("src/main.rs", b"fn main() {}");
    app.commit_all("init");
    app.write("notes.txt", b"untracked");
    let shared = app.sibling("shared");
    write(&shared.join("lib/util.rs"), b"pub fn util() {}");
    write(&shared.join(".env"), b"TOKEN=1");
    let shared_is_plain = crate::git::root_kind(&shared).unwrap() == crate::git::RootKind::Plain;

    let roots = [
        RootSpec::mounted(&shared, "libs/shared"),
        RootSpec::mounted(&app.dir, "app"),
    ];
    let coordinated = CapturePolicy {
        required_consistency: Consistency::Coordinated,
        ..CapturePolicy::default()
    };
    let held = Arc::new(AtomicBool::new(false));
    let store = WatchedStore {
        inner: MemStore::new(),
        barrier_held: held.clone(),
        puts_with_barrier: AtomicU32::new(0),
        puts_without_barrier: AtomicU32::new(0),
    };

    // No barrier: refused before a single file is read.
    let err = capture(&roots, &coordinated, &store, None).unwrap_err();
    match &err {
        CaptureError::Consistency { required, achieved } => {
            assert_eq!(*required, Consistency::Coordinated);
            assert_eq!(*achieved, Consistency::BestEffort);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(store.inner.is_empty());

    // A barrier that cannot be acquired is no barrier.
    let refusing = TestBarrier {
        held: held.clone(),
        acquired: AtomicU32::new(0),
        refuse: true,
    };
    assert!(matches!(
        capture(&roots, &coordinated, &store, Some(&refusing)),
        Err(CaptureError::Consistency { .. })
    ));
    assert!(store.inner.is_empty());
    let best_effort = capture(&roots, &CapturePolicy::default(), &store, Some(&refusing)).unwrap();
    assert_eq!(best_effort.meta.consistency, Consistency::BestEffort);
    assert!(
        best_effort
            .warnings
            .iter()
            .any(|w| w.contains("a writer did not answer")),
        "{:?}",
        best_effort.warnings
    );

    let barrier = TestBarrier {
        held: held.clone(),
        acquired: AtomicU32::new(0),
        refuse: false,
    };
    let fresh = WatchedStore {
        inner: MemStore::new(),
        barrier_held: held.clone(),
        puts_with_barrier: AtomicU32::new(0),
        puts_without_barrier: AtomicU32::new(0),
    };
    let clock = ManualClock::at(UnixMillis(1_700_000_000_000));
    let result = capture_with_clock(&roots, &coordinated, &fresh, Some(&barrier), &clock).unwrap();

    assert_eq!(result.meta.consistency, Consistency::Coordinated);
    assert_eq!(result.meta.captured_at, UnixMillis(1_700_000_000_000));
    assert_eq!(
        barrier.acquired.load(Ordering::SeqCst),
        1,
        "one barrier for all roots"
    );
    assert_eq!(fresh.puts_with_barrier.load(Ordering::SeqCst), 3);
    assert_eq!(fresh.puts_without_barrier.load(Ordering::SeqCst), 0);
    assert!(
        !held.load(Ordering::SeqCst),
        "the barrier is released afterwards"
    );

    // One manifest, roots in canonical order, each with its own provenance.
    let mounts: Vec<&str> = result
        .manifest
        .roots
        .iter()
        .map(|r| r.mount.as_str())
        .collect();
    assert_eq!(mounts, ["app", "libs/shared"]);
    assert_eq!(
        paths(&result),
        [
            "app/notes.txt",
            "app/src/main.rs",
            "libs/shared/lib/util.rs"
        ]
    );
    assert_eq!(result.excluded_secrets, ["libs/shared/.env"]);
    assert_eq!(result.meta.roots.len(), 2);
    assert_eq!(result.meta.roots[0].mount, "app");
    assert!(result.meta.roots[0].git.is_some());
    assert_eq!(result.meta.roots[1].mount, "libs/shared");
    assert_eq!(result.meta.roots[1].label, "shared");
    if shared_is_plain {
        assert!(result.meta.roots[1].git.is_none());
    }
    result.manifest.validate().unwrap();
    assert_eq!(result.snapshot_id, result.manifest.id());
    // The same tree without a barrier is the same snapshot: consistency is
    // a fact about the capture, not part of the identity.
    assert_eq!(result.snapshot_id, best_effort.snapshot_id);
}

/// a37: a snapshot is immutable. Edits and deletions in the tree after the
/// capture change neither the manifest nor the stored bytes.
#[test]
fn a37_snapshot_survives_later_edits_and_deletions() {
    let repo = Repo::new();
    repo.write("a.txt", b"original a");
    repo.write("dir/b.txt", b"original b");
    repo.write("dir/c.txt", b"original c");
    repo.commit_all("init");
    repo.write("untracked.txt", b"original untracked");

    let store = MemStore::new();
    let first = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    let manifest_bytes = first.manifest.canonical_bytes();
    let captured: BTreeMap<String, Vec<u8>> = [
        ("a.txt", "original a"),
        ("dir/b.txt", "original b"),
        ("dir/c.txt", "original c"),
        ("untracked.txt", "original untracked"),
    ]
    .into_iter()
    .map(|(path, text)| (path.to_string(), text.as_bytes().to_vec()))
    .collect();
    assert_eq!(tree(&store, &first), captured);

    repo.write("a.txt", b"edited after the capture");
    fs::remove_file(repo.path("dir/b.txt")).unwrap();
    fs::remove_file(repo.path("untracked.txt")).unwrap();
    repo.write("new.txt", b"created after the capture");
    repo.commit_all("later work");

    // The first snapshot still reads back exactly as it was captured.
    assert_eq!(first.manifest.canonical_bytes(), manifest_bytes);
    assert_eq!(first.snapshot_id, first.manifest.id());
    assert_eq!(tree(&store, &first), captured);
    for digest in first.manifest.digests() {
        assert!(store.contains(&digest).unwrap());
    }

    // A new capture describes the new state and leaves the old one alone.
    let second = snap(&repo.dir, &CapturePolicy::default(), &store).unwrap();
    assert_ne!(second.snapshot_id, first.snapshot_id);
    assert_eq!(paths(&second), ["a.txt", "dir/c.txt", "new.txt"]);
    assert_eq!(
        content(&store, &second, "a.txt"),
        b"edited after the capture"
    );
    assert_eq!(tree(&store, &first), captured);
}
