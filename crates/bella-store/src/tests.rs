//! Store behaviour tests. Every test works in its own temporary directory.

use crate::format::{self, HEADER_LEN, Header};
use crate::{FsStore, GcReport, Repaired, StoreOptions, StoreStats, Verification};
use bella_core::store::{ObjectStore, StoreError, get_bytes, put_bytes};
use bella_core::{Digest, ManualClock, UnixMillis};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, SystemTime};

const HOUR: Duration = Duration::from_secs(60 * 60);

/// Incompressible bytes from a fixed seed (xorshift64*), so zstd stores them
/// raw and every payload byte of the stored file is content.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.extend_from_slice(&state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Compressible bytes.
fn text(len: usize) -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog\n"
        .iter()
        .copied()
        .cycle()
        .take(len)
        .collect()
}

fn tmp_entries(store: &FsStore) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(store.root().join("tmp"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn listed(store: &FsStore) -> HashSet<Digest> {
    store.list().unwrap().map(Result::unwrap).collect()
}

fn manual_store(root: &Path) -> (FsStore, ManualClock) {
    let clock = ManualClock::at(UnixMillis::now());
    let store = FsStore::open_with(
        root,
        StoreOptions {
            clock: Arc::new(clock.clone()),
            ..StoreOptions::default()
        },
    )
    .unwrap();
    (store, clock)
}

fn limited_store(root: &Path, max_object_bytes: u64) -> FsStore {
    FsStore::open_with(
        root,
        StoreOptions {
            max_object_bytes,
            ..StoreOptions::default()
        },
    )
    .unwrap()
}

/// Asserts that reading the object fails with `InvalidData` and that
/// `verify` reports it as corrupt.
fn assert_rejected(store: &FsStore, digest: &Digest, case: &str) {
    match get_bytes(store, digest) {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{case}"),
        Err(other) => panic!("{case}: expected InvalidData, got {other}"),
        Ok(bytes) => panic!("{case}: {} bytes were returned as valid", bytes.len()),
    }
    assert!(
        matches!(store.verify(digest).unwrap(), Verification::Corrupt { .. }),
        "{case}: verify did not report corruption"
    );
}

fn assert_invalid_data<T>(result: Result<T, StoreError>, case: &str) {
    match result {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{case}"),
        Err(other) => panic!("{case}: expected InvalidData, got {other}"),
        Ok(_) => panic!("{case}: accepted"),
    }
}

fn assert_nothing_stored(store: &FsStore, case: &str) {
    assert_eq!(store.stats().unwrap(), StoreStats::default(), "{case}");
    assert!(tmp_entries(store).is_empty(), "{case}: temporary file left");
}

#[test]
fn round_trip_empty_small_and_multi_megabyte() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let mut large = text(3 * 1024 * 1024);
    large.extend_from_slice(&noise(1, 2 * 1024 * 1024 + 123));

    for content in [Vec::new(), b"hello".to_vec(), large] {
        let expected = Digest::of_bytes(&content);
        assert!(!store.contains(&expected).unwrap());

        let (digest, len) = store.put(&mut &content[..], None).unwrap();
        assert_eq!(digest, expected);
        assert_eq!(len, content.len() as u64);
        assert!(store.contains(&digest).unwrap());
        assert_eq!(store.size(&digest).unwrap(), Some(len));
        assert_eq!(get_bytes(&store, &digest).unwrap(), content);
        assert_eq!(store.verify(&digest).unwrap(), Verification::Valid { len });

        let path = dir
            .path()
            .join("objects")
            .join(digest.shard())
            .join(digest.as_str());
        let stored = fs::read(&path).unwrap();
        assert_eq!(
            Header::parse(stored[..HEADER_LEN].try_into().unwrap()).unwrap(),
            Header { len }
        );

        // Putting it again, with the digest stated, changes nothing.
        assert_eq!(
            store.put(&mut &content[..], Some(&expected)).unwrap(),
            (digest, len)
        );
        assert_eq!(fs::read(&path).unwrap(), stored);
    }
    assert_eq!(store.stats().unwrap().objects, 3);
    assert!(tmp_entries(&store).is_empty());

    // A second handle on the same root sees the same objects.
    let reopened = FsStore::open(dir.path()).unwrap();
    assert_eq!(listed(&reopened), listed(&store));
    assert_eq!(
        get_bytes(&reopened, &Digest::of_bytes(b"hello")).unwrap(),
        b"hello"
    );
}

#[test]
fn missing_object_is_reported_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let digest = Digest::of_bytes(b"never stored");
    assert!(!store.contains(&digest).unwrap());
    assert_eq!(store.size(&digest).unwrap(), None);
    assert!(matches!(store.open(&digest), Err(StoreError::NotFound(d)) if d == digest));
    assert!(matches!(
        store.open_compressed(&digest),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(store.verify(&digest).unwrap(), Verification::Missing);
}

#[test]
fn expected_digest_mismatch_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(2, 300_000);
    let actual = Digest::of_bytes(&content);
    let wrong = Digest::of_bytes(b"something else");

    match store.put(&mut &content[..], Some(&wrong)) {
        Err(StoreError::DigestMismatch {
            expected,
            actual: got,
        }) => {
            assert_eq!(expected, wrong);
            assert_eq!(got, actual);
        }
        other => panic!("expected a digest mismatch, got {:?}", other.map(|_| ())),
    }
    assert!(!store.contains(&wrong).unwrap());
    assert!(!store.contains(&actual).unwrap());
    assert_nothing_stored(&store, "mismatch");
}

#[test]
fn over_limit_object_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = limited_store(dir.path(), 100_000);
    assert_eq!(store.max_object_bytes(), 100_000);

    for len in [100_001, 100_000 + 70_000, 500_000] {
        let content = noise(3, len);
        assert!(
            matches!(
                store.put(&mut &content[..], None),
                Err(StoreError::TooLarge { limit: 100_000 })
            ),
            "{len} bytes"
        );
        assert_nothing_stored(&store, "over limit");
    }

    // Exactly at the limit is accepted.
    let content = noise(3, 100_000);
    let (digest, len) = store.put(&mut &content[..], None).unwrap();
    assert_eq!(len, 100_000);
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn failing_source_publishes_nothing() {
    struct Failing(usize);
    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("source went away"));
            }
            let n = self.0.min(buf.len());
            buf[..n].fill(b'x');
            self.0 -= n;
            Ok(n)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    for produced in [0, 10, 200_000] {
        assert!(matches!(
            store.put(&mut Failing(produced), None),
            Err(StoreError::Io(_))
        ));
        assert_nothing_stored(&store, "failing source");
    }
}

#[test]
fn a31_temp_names_are_unique_across_threads_and_store_handles() {
    let dir = tempfile::tempdir().unwrap();
    let stores = [
        FsStore::open(dir.path()).unwrap(),
        FsStore::open(dir.path()).unwrap(),
    ];
    const THREADS: usize = 4;
    const PER_THREAD: usize = 2_000;

    let barrier = Barrier::new(THREADS * stores.len());
    let barrier = &barrier;
    let names: Vec<String> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for store in &stores {
            for _ in 0..THREADS {
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    (0..PER_THREAD)
                        .map(|_| store.temp_name())
                        .collect::<Vec<_>>()
                }));
            }
        }
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });

    assert_eq!(names.len(), THREADS * stores.len() * PER_THREAD);
    let distinct: HashSet<&String> = names.iter().collect();
    assert_eq!(distinct.len(), names.len(), "two puts shared a temp name");
    let pid = format!("{:08x}-", std::process::id());
    assert!(
        names
            .iter()
            .all(|name| name.starts_with(&pid) && name.ends_with(".tmp"))
    );
}

#[test]
fn a31_concurrent_puts_of_same_and_different_objects_all_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let stores = [
        FsStore::open(dir.path()).unwrap(),
        FsStore::open(dir.path()).unwrap(),
    ];
    const THREADS: usize = 8;
    const ROUNDS: usize = 12;
    const SHARED: usize = 4;

    let shared: Vec<Vec<u8>> = (0..SHARED)
        .map(|i| noise(100 + i as u64, 40_000 + i * 30_000))
        .collect();
    let own = |thread: usize, round: usize| {
        let mut bytes = format!("thread {thread} round {round}\n").into_bytes();
        bytes.extend_from_slice(&text(round * 997));
        bytes
    };

    let barrier = Barrier::new(THREADS);
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let store = &stores[thread % stores.len()];
            let (shared, barrier, own) = (&shared, &barrier, &own);
            scope.spawn(move || {
                barrier.wait();
                for round in 0..ROUNDS {
                    let same = &shared[round % SHARED];
                    let (digest, len) = store.put(&mut &same[..], None).unwrap();
                    assert_eq!(digest, Digest::of_bytes(same));
                    assert_eq!(len, same.len() as u64);
                    assert_eq!(&get_bytes(store, &digest).unwrap(), same);

                    let mine = own(thread, round);
                    let digest = put_bytes(store, &mine).unwrap();
                    assert_eq!(get_bytes(store, &digest).unwrap(), mine);
                }
            });
        }
    });

    for store in &stores {
        assert_eq!(store.temp_collisions(), 0, "temp names collided");
    }
    let store = &stores[0];
    let mut expected: HashSet<Digest> = shared.iter().map(|b| Digest::of_bytes(b)).collect();
    for thread in 0..THREADS {
        for round in 0..ROUNDS {
            expected.insert(Digest::of_bytes(&own(thread, round)));
        }
    }
    assert_eq!(expected.len(), SHARED + THREADS * ROUNDS);
    assert_eq!(listed(store), expected);
    for digest in &expected {
        assert!(matches!(
            store.verify(digest).unwrap(),
            Verification::Valid { .. }
        ));
    }
    assert!(tmp_entries(store).is_empty());
}

const CHILD_ROOT: &str = "BELLA_STORE_TEST_CHILD_ROOT";
const CHILD_INDEX: &str = "BELLA_STORE_TEST_CHILD_INDEX";
const CHILD_SHARED: u64 = 24;
const CHILD_OWN: u64 = 12;

fn child_shared(i: u64) -> Vec<u8> {
    noise(1_000 + i, 20_000 + (i as usize) * 4_000)
}

fn child_own(child: u64, i: u64) -> Vec<u8> {
    format!("process {child} object {i}").into_bytes()
}

/// Body of one writer process for
/// `a31_concurrent_puts_from_several_processes`. Does nothing when run as
/// an ordinary test.
#[test]
fn a31_child_process_puts() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let child: u64 = std::env::var(CHILD_INDEX).unwrap().parse().unwrap();
    let store = FsStore::open(root).unwrap();
    for i in 0..CHILD_SHARED.max(CHILD_OWN) {
        if i < CHILD_SHARED {
            let content = child_shared(i);
            let digest = put_bytes(&store, &content).unwrap();
            assert_eq!(get_bytes(&store, &digest).unwrap(), content);
        }
        if i < CHILD_OWN {
            put_bytes(&store, &child_own(child, i)).unwrap();
        }
    }
    assert_eq!(store.temp_collisions(), 0, "temp names collided");
}

#[test]
fn a31_concurrent_puts_from_several_processes() {
    const CHILDREN: u64 = 4;
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();

    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..CHILDREN)
        .map(|child| {
            Command::new(&exe)
                .args(["--exact", "tests::a31_child_process_puts", "--nocapture"])
                .env(CHILD_ROOT, dir.path())
                .env(CHILD_INDEX, child.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "writer process failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut expected: HashSet<Digest> = (0..CHILD_SHARED)
        .map(|i| Digest::of_bytes(&child_shared(i)))
        .collect();
    for child in 0..CHILDREN {
        for i in 0..CHILD_OWN {
            expected.insert(Digest::of_bytes(&child_own(child, i)));
        }
    }
    assert_eq!(
        expected.len() as u64,
        CHILD_SHARED + CHILDREN * CHILD_OWN,
        "test objects must be distinct"
    );
    assert_eq!(listed(&store), expected);
    for digest in &expected {
        assert!(matches!(
            store.verify(digest).unwrap(),
            Verification::Valid { .. }
        ));
    }
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn a36_flipped_truncated_or_extended_object_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(7, 200_000);
    let digest = put_bytes(&store, &content).unwrap();
    let path = store.object_path(&digest);
    let good = fs::read(&path).unwrap();
    assert!(
        good.len() > content.len(),
        "the test needs the content stored raw"
    );

    // Flipped bytes: every header field, then the first, middle and last
    // byte of the content.
    let payload_start = good.len() - content.len();
    let flips = [
        0,
        3,
        4,
        5,
        7,
        8,
        12,
        15,
        payload_start,
        payload_start + content.len() / 2,
        good.len() - 1,
    ];
    for index in flips {
        let mut bad = good.clone();
        bad[index] ^= 0x40;
        fs::write(&path, &bad).unwrap();
        assert_rejected(&store, &digest, &format!("byte {index} flipped"));
    }

    // Truncated: empty, inside the header, header only, inside the frame,
    // one byte short.
    for cut in [
        0,
        5,
        HEADER_LEN,
        HEADER_LEN + 1,
        good.len() / 2,
        good.len() - 1,
    ] {
        fs::write(&path, &good[..cut]).unwrap();
        assert_rejected(&store, &digest, &format!("truncated to {cut} bytes"));
    }

    // Extended: a zero byte, random bytes, and a second well-formed frame.
    let mut second_frame = Cursor::new(Vec::new());
    format::encode(&mut &b"more"[..], &mut second_frame, u64::MAX).unwrap();
    let second_frame = second_frame.into_inner()[HEADER_LEN..].to_vec();
    for (case, extra) in [
        ("one zero byte appended", vec![0u8]),
        ("random bytes appended", noise(8, 100)),
        ("a second frame appended", second_frame),
    ] {
        let mut bad = good.clone();
        bad.extend_from_slice(&extra);
        fs::write(&path, &bad).unwrap();
        assert_rejected(&store, &digest, case);
    }

    // The checks above are about the damage, not the harness: the original
    // bytes read back fine.
    fs::write(&path, &good).unwrap();
    assert_eq!(
        store.verify(&digest).unwrap(),
        Verification::Valid {
            len: content.len() as u64
        }
    );
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
}

#[test]
fn a36_no_damaged_file_is_ever_returned_as_other_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = text(900);
    let digest = put_bytes(&store, &content).unwrap();
    let path = store.object_path(&digest);
    let good = fs::read(&path).unwrap();
    assert!(good.len() < 200, "the test needs a small stored file");

    let mut detected = 0;
    for index in 0..good.len() {
        for mask in [0x01u8, 0x80, 0xFF] {
            let mut bad = good.clone();
            bad[index] ^= mask;
            fs::write(&path, &bad).unwrap();
            match get_bytes(&store, &digest) {
                // A flipped bit that zstd does not interpret can leave the
                // content intact; what may never happen is other content.
                Ok(bytes) => assert_eq!(bytes, content, "byte {index} ^ {mask:#04x}"),
                Err(_) => detected += 1,
            }
            if index < HEADER_LEN {
                assert!(
                    get_bytes(&store, &digest).is_err(),
                    "header byte {index} ^ {mask:#04x} went unnoticed"
                );
            }
        }
    }
    assert!(detected >= HEADER_LEN * 3);

    // Every proper prefix of the file is an error.
    for cut in 0..good.len() {
        fs::write(&path, &good[..cut]).unwrap();
        assert!(
            get_bytes(&store, &digest).is_err(),
            "prefix of {cut} bytes was accepted"
        );
    }
}

#[test]
fn crash_leftover_in_tmp_is_ignored_then_collected_after_grace() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(9, 50_000);
    let digest = put_bytes(&store, &content).unwrap();
    let stored = fs::read(store.object_path(&digest)).unwrap();
    let tmp = dir.path().join("tmp");

    // A writer that died half way through an object.
    let dead_writer = tmp.join("0000dead-0123456789abcdef-00000000.tmp");
    fs::write(&dead_writer, &stored[..stored.len() / 2]).unwrap();
    // A leftover carrying the very name this store would use next.
    let used = store.temp_name();
    let (prefix, seq) = used.trim_end_matches(".tmp").rsplit_once('-').unwrap();
    let next = u32::from_str_radix(seq, 16).unwrap() + 1;
    let same_name = tmp.join(format!("{prefix}-{next:08x}.tmp"));
    fs::write(&same_name, b"left behind").unwrap();
    // Something that is not the store's at all.
    let foreign = tmp.join("notes.txt");
    fs::write(&foreign, b"not a temporary object").unwrap();

    // Readers and writers are unaffected.
    assert_eq!(listed(&store), HashSet::from([digest.clone()]));
    assert_eq!(store.stats().unwrap().objects, 1);
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    let other = put_bytes(&store, b"written past the leftover").unwrap();
    assert_eq!(store.temp_collisions(), 1);
    assert_eq!(fs::read(&same_name).unwrap(), b"left behind");
    assert_eq!(
        get_bytes(&store, &other).unwrap(),
        b"written past the leftover"
    );

    // Inside the grace period the leftovers stay.
    let report = store.gc(&|_| true, HOUR).unwrap();
    assert_eq!(
        report,
        GcReport {
            objects_live: 2,
            temps_in_grace: 2,
            ..GcReport::default()
        }
    );
    assert!(dead_writer.exists() && same_name.exists());

    // After it they are removed; the foreign file and the objects are not.
    // Temporary files carry operating-system timestamps, so the test ages
    // them by moving those back.
    for path in [&dead_writer, &same_name, &foreign] {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - 2 * HOUR).unwrap();
    }
    let report = store.gc(&|_| true, HOUR).unwrap();
    assert_eq!(
        report,
        GcReport {
            objects_live: 2,
            temps_removed: 2,
            bytes_freed: (stored.len() / 2 + b"left behind".len()) as u64,
            ..GcReport::default()
        }
    );
    assert_eq!(tmp_entries(&store), ["notes.txt"]);
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
}

#[test]
fn a46_gc_keeps_live_and_young_objects_and_rechecks_liveness_before_delete() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = manual_store(dir.path());
    let live = put_bytes(&store, b"referenced by a job").unwrap();
    let dead = put_bytes(&store, &noise(11, 10_000)).unwrap();
    let rescued = put_bytes(&store, b"referenced while gc was running").unwrap();
    let dead_stored_len = fs::metadata(store.object_path(&dead)).unwrap().len();
    clock.advance(2 * HOUR);
    let young = put_bytes(&store, b"uploaded a moment ago").unwrap();

    // Every liveness question, with whether the object was still on disk
    // when it was asked.
    let calls: RefCell<Vec<(Digest, bool)>> = RefCell::new(Vec::new());
    let asked = |digest: &Digest| {
        calls
            .borrow()
            .iter()
            .filter(|(asked, _)| asked == digest)
            .count()
    };
    let is_live = |digest: &Digest| {
        let on_disk = store.object_path(digest).exists();
        calls.borrow_mut().push((digest.clone(), on_disk));
        // `rescued` gains a reference between the scan and the delete.
        *digest == live || (*digest == rescued && asked(digest) >= 2)
    };

    let report = store.gc(&is_live, HOUR).unwrap();
    assert_eq!(
        report,
        GcReport {
            objects_removed: 1,
            objects_live: 2,
            objects_in_grace: 1,
            bytes_freed: dead_stored_len,
            ..GcReport::default()
        }
    );
    assert!(store.contains(&live).unwrap());
    assert!(store.contains(&young).unwrap());
    assert!(store.contains(&rescued).unwrap(), "re-check was ignored");
    assert!(!store.contains(&dead).unwrap());
    assert_eq!(
        get_bytes(&store, &rescued).unwrap(),
        b"referenced while gc was running"
    );

    // The deleted object was asked about twice, the second time while it
    // still existed: the delete came after the last liveness check.
    assert_eq!(asked(&dead), 2);
    assert_eq!(asked(&rescued), 2);
    assert_eq!(asked(&live), 1);
    assert_eq!(asked(&young), 1);
    assert!(calls.borrow().iter().all(|(_, on_disk)| *on_disk));

    // Once the grace period has passed, unreferenced objects go.
    clock.advance(2 * HOUR);
    let report = store.gc(&|digest| *digest == live, HOUR).unwrap();
    assert_eq!(report.objects_removed, 2);
    assert_eq!(report.objects_live, 1);
    assert_eq!(listed(&store), HashSet::from([live]));
}

#[test]
fn a46_putting_an_object_again_restarts_its_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = manual_store(dir.path());
    let digest = put_bytes(&store, b"uploaded twice").unwrap();
    clock.advance(2 * HOUR);

    // The second upload finds the object present. Its caller has not yet
    // recorded the reference, so only the grace period protects it.
    assert_eq!(put_bytes(&store, b"uploaded twice").unwrap(), digest);
    let report = store.gc(&|_| false, HOUR).unwrap();
    assert_eq!(
        report,
        GcReport {
            objects_in_grace: 1,
            ..GcReport::default()
        }
    );
    assert!(store.contains(&digest).unwrap());

    clock.advance(2 * HOUR);
    assert_eq!(store.gc(&|_| false, HOUR).unwrap().objects_removed, 1);
    assert!(!store.contains(&digest).unwrap());
}

#[test]
fn a46_gc_waits_for_a_publish_in_progress_and_sees_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = manual_store(dir.path());
    let content = noise(37, 20_000);
    let digest = put_bytes(&store, &content).unwrap();
    clock.advance(2 * HOUR);

    let (asked_tx, asked_rx) = mpsc::channel();
    // Stands for another writer that is inside its publish step.
    let publishing = store.lock_shared().unwrap();
    let report = std::thread::scope(|scope| {
        let gc = scope.spawn(|| {
            let is_live = |_: &Digest| {
                asked_tx.send(()).unwrap();
                false
            };
            store.gc(&is_live, HOUR).unwrap()
        });
        // The collector has found the object old and unreferenced. It
        // cannot delete it while a publish is in progress, and by the time
        // it may, this upload has refreshed the object.
        asked_rx.recv().unwrap();
        put_bytes(&store, &content).unwrap();
        drop(publishing);
        gc.join().unwrap()
    });

    assert_eq!(
        report,
        GcReport {
            objects_in_grace: 1,
            ..GcReport::default()
        }
    );
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
}

#[test]
fn a46_put_racing_gc_never_loses_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = manual_store(dir.path());
    let content = noise(13, 30_000);
    let digest = put_bytes(&store, &content).unwrap();

    for round in 0..40 {
        // The object is old and unreferenced when an upload of it and a
        // collection start together.
        clock.advance(2 * HOUR);
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                store.gc(&|_| false, HOUR).unwrap();
            });
            scope.spawn(|| {
                barrier.wait();
                put_bytes(&store, &content).unwrap();
            });
        });
        // Whichever went first, the upload that returned left its object.
        assert_eq!(
            get_bytes(&store, &digest).unwrap(),
            content,
            "round {round}"
        );
    }
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn open_reader_survives_gc_of_its_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(17, 400_000);
    let digest = put_bytes(&store, &content).unwrap();

    let mut reader = store.open(&digest).unwrap();
    let mut head = vec![0u8; 1_000];
    reader.read_exact(&mut head).unwrap();

    let report = store.gc(&|_| false, Duration::ZERO).unwrap();
    assert_eq!(report.objects_removed, 1);
    assert_eq!(report.failed, 0);
    assert!(!store.contains(&digest).unwrap());

    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    head.extend_from_slice(&rest);
    assert_eq!(head, content);
}

#[test]
fn publishing_over_an_object_a_reader_holds_open_counts_as_published() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(19, 300_000);
    let digest = put_bytes(&store, &content).unwrap();
    let path = store.object_path(&digest);

    let mut reader = store.open(&digest).unwrap();
    let mut head = vec![0u8; 1_000];
    reader.read_exact(&mut head).unwrap();

    // A second writer finished the same object and renames it into place.
    // Unix replaces the file; Windows may refuse while the reader is open.
    let mut temp = store.create_temp().unwrap();
    let encoded = format::encode(&mut &content[..], temp.file_mut().unwrap(), u64::MAX).unwrap();
    temp.close();
    if store
        .rename_into_place(temp.path(), &path, encoded.len)
        .unwrap()
    {
        temp.disarm();
    }
    drop(temp);
    assert!(tmp_entries(&store).is_empty());

    // So does an ordinary put of it.
    assert_eq!(put_bytes(&store, &content).unwrap(), digest);

    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    head.extend_from_slice(&rest);
    assert_eq!(head, content);
    assert!(matches!(
        store.verify(&digest).unwrap(),
        Verification::Valid { .. }
    ));
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn put_replaces_an_object_whose_header_is_damaged() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = text(5_000);
    let digest = put_bytes(&store, &content).unwrap();
    let path = store.object_path(&digest);

    let mut bad = fs::read(&path).unwrap();
    bad[1] ^= 0xFF;
    fs::write(&path, &bad).unwrap();
    assert!(matches!(
        store.verify(&digest).unwrap(),
        Verification::Corrupt { .. }
    ));
    assert!(store.size(&digest).is_err());

    assert_eq!(put_bytes(&store, &content).unwrap(), digest);
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    assert!(tmp_entries(&store).is_empty());
}

/// `content` in the stored format, as a store would write it.
fn stored_form(content: &[u8]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    format::encode(&mut &content[..], &mut out, u64::MAX).unwrap();
    out.into_inner()
}

/// Damages the stored body of `digest` and leaves its header alone, so the
/// object still looks present and of the right length.
fn corrupt_body(store: &FsStore, digest: &Digest) {
    let path = store.object_path(digest);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    assert!(last >= HEADER_LEN, "the object has no body to damage");
    bytes[last] ^= 0x40;
    fs::write(&path, &bytes).unwrap();
    assert_rejected(store, digest, "after damaging the body");
    assert_eq!(
        store.size(digest).unwrap(),
        Header::read_from(&mut &bytes[..]).ok().map(|h| h.len)
    );
}

#[test]
fn a36_body_corrupt_object_survives_a_put_and_is_removed_then_put_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(23, 50_000);
    let digest = put_bytes(&store, &content).unwrap();
    let other = put_bytes(&store, b"an unrelated object").unwrap();
    corrupt_body(&store, &digest);

    // A plain put looks at the header only and keeps the damaged copy.
    assert_eq!(put_bytes(&store, &content).unwrap(), digest);
    assert_rejected(&store, &digest, "after a plain put");

    assert!(store.remove(&digest).unwrap());
    assert!(!store.contains(&digest).unwrap());
    assert_eq!(store.verify(&digest).unwrap(), Verification::Missing);
    assert_eq!(listed(&store), HashSet::from([other.clone()]));
    // Nothing is left to remove, and nothing else was touched.
    assert!(!store.remove(&digest).unwrap());
    assert_eq!(get_bytes(&store, &other).unwrap(), b"an unrelated object");

    assert_eq!(put_bytes(&store, &content).unwrap(), digest);
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    assert_eq!(
        store.verify(&digest).unwrap(),
        Verification::Valid {
            len: content.len() as u64
        }
    );
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn remove_ignores_liveness_and_grace_and_leaves_an_open_reader_intact() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _clock) = manual_store(dir.path());
    let content = noise(29, 300_000);
    let digest = put_bytes(&store, &content).unwrap();
    // Just published: a collector would keep it for the whole grace period.
    assert_eq!(store.gc(&|_| false, HOUR).unwrap().objects_in_grace, 1);

    let mut reader = store.open(&digest).unwrap();
    let mut head = vec![0u8; 1_000];
    reader.read_exact(&mut head).unwrap();

    assert!(store.remove(&digest).unwrap());
    assert!(!store.contains(&digest).unwrap());
    assert_eq!(store.stats().unwrap(), StoreStats::default());

    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    head.extend_from_slice(&rest);
    assert_eq!(head, content);
}

#[test]
fn a36_repair_replaces_a_body_corrupt_object_and_only_that() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = manual_store(dir.path());
    let content = noise(31, 80_000);
    let digest = Digest::of_bytes(&content);

    // Nothing stored yet: repair publishes.
    assert_eq!(
        store.repair(&digest, &mut &content[..]).unwrap(),
        Repaired::Stored
    );
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);

    // Intact: the stored file is kept byte for byte, only made young again.
    let path = store.object_path(&digest);
    let stored = fs::read(&path).unwrap();
    clock.advance(2 * HOUR);
    assert_eq!(
        store.repair(&digest, &mut &content[..]).unwrap(),
        Repaired::Intact
    );
    assert_eq!(fs::read(&path).unwrap(), stored);
    assert_eq!(store.gc(&|_| false, HOUR).unwrap().objects_in_grace, 1);

    // Damaged body: replaced, and the replacement starts a new grace period.
    corrupt_body(&store, &digest);
    clock.advance(2 * HOUR);
    assert!(matches!(
        store.repair(&digest, &mut &content[..]).unwrap(),
        Repaired::Replaced { reason } if !reason.is_empty()
    ));
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    assert_eq!(fs::read(&path).unwrap(), stored);
    assert_eq!(store.gc(&|_| false, HOUR).unwrap().objects_in_grace, 1);

    // Damaged header: replaced as well.
    let mut bad = stored.clone();
    bad[1] ^= 0xFF;
    fs::write(&path, &bad).unwrap();
    assert!(matches!(
        store.repair(&digest, &mut &content[..]).unwrap(),
        Repaired::Replaced { .. }
    ));
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    assert_eq!(
        store.stats().unwrap(),
        StoreStats {
            objects: 1,
            stored_bytes: stored.len() as u64
        }
    );
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn a36_repair_checks_the_supplied_copy_before_touching_the_stored_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = limited_store(dir.path(), 100_000);
    let content = text(40_000);
    let digest = put_bytes(&store, &content).unwrap();
    corrupt_body(&store, &digest);
    let damaged = fs::read(store.object_path(&digest)).unwrap();

    // Wrong content, in either form, repairs nothing.
    let wrong = text(40_001);
    assert!(matches!(
        store.repair(&digest, &mut &wrong[..]),
        Err(StoreError::DigestMismatch { .. })
    ));
    let wrong_stored = stored_form(&wrong);
    assert!(matches!(
        store.repair_compressed(&digest, &mut &wrong_stored[..]),
        Err(StoreError::DigestMismatch { .. })
    ));
    // So does the damaged copy itself, offered back as the replacement.
    assert_invalid_data(
        store.repair_compressed(&digest, &mut &damaged[..]),
        "damaged replacement",
    );
    let too_large = text(100_001);
    assert!(matches!(
        store.repair(&Digest::of_bytes(&too_large), &mut &too_large[..]),
        Err(StoreError::TooLarge { limit: 100_000 })
    ));
    assert_eq!(fs::read(store.object_path(&digest)).unwrap(), damaged);
    assert!(tmp_entries(&store).is_empty());

    // A good copy in the stored format replaces it without recompression.
    let good = stored_form(&content);
    assert!(matches!(
        store.repair_compressed(&digest, &mut &good[..]).unwrap(),
        Repaired::Replaced { .. }
    ));
    assert_eq!(fs::read(store.object_path(&digest)).unwrap(), good);
    assert_eq!(get_bytes(&store, &digest).unwrap(), content);
    assert_eq!(
        store.repair_compressed(&digest, &mut &good[..]).unwrap(),
        Repaired::Intact
    );
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn concurrent_repairs_and_puts_of_a_damaged_object_leave_it_intact() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsStore::open(dir.path()).unwrap());
    let content = Arc::new(noise(37, 200_000));
    let digest = put_bytes(&*store, &content).unwrap();
    corrupt_body(&store, &digest);

    let start = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|index| {
            let (store, content, digest, start) = (
                Arc::clone(&store),
                Arc::clone(&content),
                digest.clone(),
                Arc::clone(&start),
            );
            std::thread::spawn(move || {
                start.wait();
                if index % 2 == 0 {
                    Some(store.repair(&digest, &mut &content[..]).unwrap())
                } else {
                    put_bytes(&*store, &content).unwrap();
                    None
                }
            })
        })
        .collect();
    let outcomes: Vec<Repaired> = workers
        .into_iter()
        .filter_map(|worker| worker.join().unwrap())
        .collect();
    // The exclusive lock lets exactly one repair find the damage.
    let replaced = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Repaired::Replaced { .. }))
        .count();
    assert_eq!(replaced, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Repaired::Replaced { .. } | Repaired::Intact)),
        "{outcomes:?}"
    );
    assert_eq!(get_bytes(&*store, &digest).unwrap(), *content);
    assert!(tmp_entries(&store).is_empty());
}

#[test]
fn compressed_objects_move_between_stores_without_recompression() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let source = FsStore::open(source_dir.path()).unwrap();
    let target = FsStore::open(target_dir.path()).unwrap();

    let mut large = text(400_000);
    large.extend_from_slice(&noise(21, 300_000));
    for content in [Vec::new(), b"small".to_vec(), large] {
        let (digest, len) = source.put(&mut &content[..], None).unwrap();
        let (mut file, stored_len) = source.open_compressed(&digest).unwrap();
        let mut wire = Vec::new();
        file.read_to_end(&mut wire).unwrap();
        assert_eq!(wire.len() as u64, stored_len);
        assert_eq!(wire, fs::read(source.object_path(&digest)).unwrap());

        assert_eq!(target.put_compressed(&mut &wire[..], &digest).unwrap(), len);
        assert_eq!(get_bytes(&target, &digest).unwrap(), content);
        assert_eq!(target.size(&digest).unwrap(), Some(len));
        assert_eq!(fs::read(target.object_path(&digest)).unwrap(), wire);

        // The wire bytes are also readable without a store.
        let (mut reader, recorded) = format::decode(&wire[..], &digest).unwrap();
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(recorded, len);
        assert_eq!(decoded, content);

        // Receiving it again is harmless.
        assert_eq!(target.put_compressed(&mut &wire[..], &digest).unwrap(), len);
    }
    assert_eq!(target.stats().unwrap(), source.stats().unwrap());
    assert!(tmp_entries(&target).is_empty());
}

/// A stored object whose frame is `blocks` empty raw blocks: valid zstd
/// that decodes to nothing.
fn empty_object_with_padding(blocks: usize) -> Vec<u8> {
    let mut wire = Header { len: 0 }.encode().to_vec();
    wire.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]); // frame magic
    wire.extend_from_slice(&[0x00, 0x00]); // descriptor, 1 KiB window
    for _ in 0..blocks {
        wire.extend_from_slice(&[0x00, 0x00, 0x00]); // raw block, 0 bytes
    }
    wire.extend_from_slice(&[0x01, 0x00, 0x00]); // last raw block, 0 bytes
    wire
}

#[test]
fn put_compressed_rejects_streams_that_do_not_match() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = FsStore::open(source_dir.path()).unwrap();
    let content = noise(23, 150_000);
    let (digest, len) = source.put(&mut &content[..], None).unwrap();
    let wire = fs::read(source.object_path(&digest)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();

    // Well-formed, but not the object that was announced.
    let other = Digest::of_bytes(b"something else");
    match store.put_compressed(&mut &wire[..], &other) {
        Err(StoreError::DigestMismatch { expected, actual }) => {
            assert_eq!(expected, other);
            assert_eq!(actual, digest);
        }
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
    assert!(!store.contains(&other).unwrap());
    assert!(!store.contains(&digest).unwrap());
    assert_nothing_stored(&store, "wrong digest");

    // Content changed in transit.
    let mut tampered = wire.clone();
    let middle = wire.len() / 2;
    tampered[middle] ^= 0x01;
    assert!(store.put_compressed(&mut &tampered[..], &digest).is_err());
    assert_nothing_stored(&store, "tampered");

    for cut in [0, 10, HEADER_LEN, wire.len() / 2, wire.len() - 1] {
        let case = format!("stream cut at {cut}");
        assert_invalid_data(store.put_compressed(&mut &wire[..cut], &digest), &case);
        assert_nothing_stored(&store, &case);
    }

    let mut extended = wire.clone();
    extended.push(0);
    assert_invalid_data(
        store.put_compressed(&mut &extended[..], &digest),
        "trailing byte",
    );
    assert_nothing_stored(&store, "trailing byte");

    for claimed in [len - 1, len + 1] {
        let mut relabelled = wire.clone();
        relabelled[..HEADER_LEN].copy_from_slice(&Header { len: claimed }.encode());
        let case = format!("recorded length {claimed}");
        assert_invalid_data(store.put_compressed(&mut &relabelled[..], &digest), &case);
        assert_nothing_stored(&store, &case);
    }

    assert_invalid_data(
        store.put_compressed(&mut &b"plain bytes, not a stored object"[..], &digest),
        "not a stored object",
    );
    assert_nothing_stored(&store, "not a stored object");

    // A small recorded length in front of a frame that expands to megabytes.
    let zeros = vec![0u8; 8 * 1024 * 1024];
    let mut bomb = Cursor::new(Vec::new());
    format::encode(&mut &zeros[..], &mut bomb, u64::MAX).unwrap();
    let mut bomb = bomb.into_inner();
    assert!(bomb.len() < 4_096);
    bomb[..HEADER_LEN].copy_from_slice(&Header { len: 10 }.encode());
    assert_invalid_data(
        store.put_compressed(&mut &bomb[..], &Digest::of_bytes(&zeros[..10])),
        "decompression bomb",
    );
    assert_nothing_stored(&store, "decompression bomb");

    // A stream far longer than its content could ever need.
    let empty = Digest::of_bytes(b"");
    assert_invalid_data(
        store.put_compressed(&mut &empty_object_with_padding(2_000)[..], &empty),
        "padded stream",
    );
    assert_nothing_stored(&store, "padded stream");
    // The same construction within the bound is a valid empty object, so
    // the rejection above is the size bound and nothing else.
    assert_eq!(
        store
            .put_compressed(&mut &empty_object_with_padding(10)[..], &empty)
            .unwrap(),
        0
    );
    assert_eq!(get_bytes(&store, &empty).unwrap(), b"");

    // Recorded length over the store's limit: refused before any decoding.
    let small_dir = tempfile::tempdir().unwrap();
    let small = limited_store(small_dir.path(), len - 1);
    assert!(matches!(
        small.put_compressed(&mut &wire[..], &digest),
        Err(StoreError::TooLarge { limit }) if limit == len - 1
    ));
    assert_nothing_stored(&small, "over limit");
}

#[test]
fn size_reads_the_header_without_reading_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    let content = noise(29, 1024 * 1024);
    let digest = put_bytes(&store, &content).unwrap();
    assert_eq!(store.size(&digest).unwrap(), Some(1024 * 1024));

    // With the body cut away right after the header the object is unusable,
    // yet the size is still answered: it never looked at the body.
    let path = store.object_path(&digest);
    let stored = fs::read(&path).unwrap();
    fs::write(&path, &stored[..HEADER_LEN]).unwrap();
    assert_eq!(store.size(&digest).unwrap(), Some(1024 * 1024));
    assert_rejected(&store, &digest, "header only");

    // An empty object has size zero, not "absent".
    let empty = put_bytes(&store, b"").unwrap();
    assert_eq!(store.size(&empty).unwrap(), Some(0));
}

#[test]
fn list_and_stats_count_only_published_objects() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::open(dir.path()).unwrap();
    assert_eq!(store.stats().unwrap(), StoreStats::default());
    assert!(listed(&store).is_empty());

    let digests: HashSet<Digest> = [&b"one"[..], &text(70_000)[..], &noise(31, 9_000)[..]]
        .into_iter()
        .map(|content| put_bytes(&store, content).unwrap())
        .collect();
    let stored_bytes: u64 = digests
        .iter()
        .map(|digest| fs::metadata(store.object_path(digest)).unwrap().len())
        .sum();

    // Files that are not objects: wrong name, right name in the wrong
    // fan-out directory, a directory, something in the objects root.
    let any = digests.iter().next().unwrap();
    let shard = dir.path().join("objects").join(any.shard());
    fs::write(shard.join("README"), b"not an object").unwrap();
    let directory = Digest::of_bytes(b"a directory");
    fs::create_dir(store.object_path(&directory)).unwrap();
    let misplaced = Digest::of_bytes(b"misplaced");
    let wrong_shard = if misplaced.shard() == "00" {
        "01"
    } else {
        "00"
    };
    fs::write(
        dir.path()
            .join("objects")
            .join(wrong_shard)
            .join(misplaced.as_str()),
        b"misplaced",
    )
    .unwrap();
    fs::write(dir.path().join("objects").join("stray"), b"stray").unwrap();

    assert_eq!(listed(&store), digests);
    assert_eq!(
        store.stats().unwrap(),
        StoreStats {
            objects: 3,
            stored_bytes
        }
    );

    // Collection leaves them alone too.
    let report = store.gc(&|_| false, Duration::ZERO).unwrap();
    assert_eq!(report.objects_removed, 3);
    assert_eq!(report.bytes_freed, stored_bytes);
    assert!(shard.join("README").exists());
    assert!(dir.path().join("objects").join("stray").exists());
    assert_eq!(store.stats().unwrap(), StoreStats::default());
}

#[test]
fn a_root_with_another_layout_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    FsStore::open(dir.path()).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("VERSION")).unwrap(),
        "1\n"
    );
    assert!(tmp_entries(&FsStore::open(dir.path()).unwrap()).is_empty());

    fs::write(dir.path().join("VERSION"), "2\n").unwrap();
    match FsStore::open(dir.path()) {
        Err(StoreError::Unavailable(message)) => assert!(message.contains("layout version `2`")),
        other => panic!("expected the store to be refused, got {other:?}"),
    }
}
