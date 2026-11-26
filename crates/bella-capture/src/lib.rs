//! Capture a working tree into an immutable snapshot.
//!
//! [`capture`] turns one or more local source roots into a
//! [`Manifest`] and the objects it references in an [`ObjectStore`]. The
//! snapshot describes the bytes that were in the working tree, including
//! uncommitted edits, and stays valid whatever happens to the tree later.
//!
//! # What is captured
//!
//! * **Git working trees** (the root may be a repository, a subdirectory of
//!   one or a linked worktree): every tracked path and, by policy, every
//!   untracked path that is not ignored. The bytes come from the working
//!   tree, never from the index. A tracked file that is missing on disk is
//!   absent from the snapshot. Initialised submodules and untracked nested
//!   repositories are captured from their own working trees under their
//!   path. Git is only ever queried; no command that writes to a repository
//!   is run, and `.git` is never captured.
//! * **Other directories**: every file below the root. Symlinks are
//!   recorded, never followed. A root that an enclosing repository ignores
//!   is captured this way too, with a warning: that repository's rules
//!   would select nothing.
//! * Links are never followed. A relative link that stays inside its root
//!   is recorded as a link; an absolute or escaping one fails the capture,
//!   as does a directory link (a Windows junction) that git lists files
//!   below.
//! * Sockets, FIFOs and devices are left out with a warning.
//! * Ignored files only when `include_ignored` declares them.
//! * Empty directories are **not** recorded. Git cannot represent them, so
//!   recording them only for untracked directories would make the same
//!   tree hash differently depending on how it was obtained.
//! * Line endings are whatever the working tree holds. A checkout that
//!   converts them has different bytes and therefore a different snapshot
//!   id than one that does not.
//!
//! # Patterns
//!
//! `exclude`, `include_ignored`, `secret_patterns` and `allow_secrets` take
//! patterns relative to each source root, written with `/`: `*`, `?` and
//! `[a-z]` match inside one path component, `**` matches whole components.
//! A pattern without `/` matches by name at any depth; a pattern with `/`
//! is anchored at the root. `exclude`, `include_ignored` and
//! `allow_secrets` also cover everything below a directory they match.
//! `secret_patterns` match a file by its name or its relative path, without
//! regard to letter case.
//!
//! # Consistency
//!
//! Each file is read once; the bytes that were hashed are the bytes that
//! are stored. A file that changes while it is read is read again a bounded
//! number of times. After the last file the tree is listed and examined
//! again, and a difference restarts the capture a bounded number of times.
//! That detects mutation, it does not prevent it: without a
//! [`CaptureBarrier`] the result is [`Consistency::BestEffort`]. A change
//! is recognised by size, modification time and, on Unix, inode and
//! status-change time; a rewrite that preserves all of them on a
//! filesystem with coarse timestamps goes unnoticed.

mod engine;
mod enumerate;
mod error;
mod git;
mod pattern;
mod read;
#[cfg(test)]
mod tests;

pub use error::CaptureError;

use bella_core::defaults::{GIB, MAX_OBJECT_BYTES};
use bella_core::ids::SnapshotId;
use bella_core::snapshot::{Consistency, Manifest, SnapshotMeta};
use bella_core::store::ObjectStore;
use bella_core::time::{Clock, SystemClock};
use std::any::Any;
use std::path::PathBuf;

/// One local directory and where it is materialized in the source set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSpec {
    /// The directory to capture.
    pub path: PathBuf,
    /// Its place in the source set: `.` for a single root, otherwise a
    /// relative path that is not nested inside another root's mount.
    pub mount: String,
}

impl RootSpec {
    /// A single-root capture of `path`, mounted at `.`.
    pub fn single(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mount: ".".into(),
        }
    }

    pub fn mounted(path: impl Into<PathBuf>, mount: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mount: mount.into(),
        }
    }
}

/// What a capture includes and what it refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturePolicy {
    /// Capture untracked files that git does not ignore. Applies to git
    /// working trees; a plain directory has no tracked files to fall back to.
    pub include_untracked: bool,
    /// Ignored files or directories that the build needs. An entry without
    /// wildcards must exist in at least one root.
    pub include_ignored: Vec<String>,
    /// Never captured, tracked or not.
    pub exclude: Vec<String>,
    /// Files that look like credentials. A match that git does not track is
    /// left out and reported; a tracked match fails the capture.
    pub secret_patterns: Vec<String>,
    /// Paths that may be captured although they match a secret pattern.
    pub allow_secrets: Vec<String>,
    /// Largest single file.
    pub max_file_bytes: u64,
    /// Largest sum of file sizes in one snapshot.
    pub max_total_bytes: u64,
    /// The capture fails before reading anything if it cannot be at least
    /// this coherent.
    pub required_consistency: Consistency,
}

impl CapturePolicy {
    /// The secret patterns a default policy starts with.
    pub const DEFAULT_SECRET_PATTERNS: [&'static str; 11] = [
        ".env",
        ".env.*",
        "*.pem",
        "*.key",
        "id_rsa*",
        "id_ed25519*",
        ".npmrc",
        ".netrc",
        "*.p12",
        "*.pfx",
        "credentials.json",
    ];

    /// Names that match a secret pattern but are, by convention, safe to
    /// capture: checked-in templates and public halves of key pairs.
    pub const DEFAULT_ALLOW_SECRETS: [&'static str; 5] = [
        ".env.example",
        ".env.sample",
        ".env.template",
        ".env.dist",
        "*.pub",
    ];
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            include_untracked: true,
            include_ignored: Vec::new(),
            exclude: Vec::new(),
            secret_patterns: Self::DEFAULT_SECRET_PATTERNS
                .iter()
                .map(|p| p.to_string())
                .collect(),
            allow_secrets: Self::DEFAULT_ALLOW_SECRETS
                .iter()
                .map(|p| p.to_string())
                .collect(),
            max_file_bytes: MAX_OBJECT_BYTES,
            max_total_bytes: 16 * GIB,
            required_consistency: Consistency::BestEffort,
        }
    }
}

/// A way to stop cooperating writers for the duration of a capture.
pub trait CaptureBarrier {
    /// Ask every participating writer to pause. The returned guard is held
    /// until the capture has finished and released by dropping it.
    fn acquire(&self) -> Result<Box<dyn Any + Send>, String>;
}

/// Counters of one capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Files in the manifest.
    pub files: u64,
    /// Symlinks in the manifest.
    pub symlinks: u64,
    /// Sum of the file sizes in the manifest.
    pub bytes: u64,
    /// Objects this capture uploaded to the store.
    pub objects_stored: u64,
    pub bytes_stored: u64,
    /// Files whose object the store already had.
    pub objects_reused: u64,
    /// Reads repeated because a file changed while it was read.
    pub read_retries: u32,
    /// Passes over the tree; more than one means an edit was detected
    /// after its file had been read.
    pub passes: u32,
}

/// A finished capture. Every object the manifest references is in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureResult {
    /// Canonical and validated.
    pub manifest: Manifest,
    pub snapshot_id: SnapshotId,
    pub meta: SnapshotMeta,
    pub stats: CaptureStats,
    /// Files left out because they match a secret pattern and git does not
    /// track them. Sorted.
    pub excluded_secrets: Vec<String>,
    /// Things that did not stop the capture but that the user should see.
    pub warnings: Vec<String>,
}

/// Capture `roots` into `store` and describe them in one manifest.
///
/// With a `barrier`, its guard is acquired first and held until the capture
/// returns, and the result is [`Consistency::Coordinated`]; all roots are
/// captured under that one guard. Without one, or when it cannot be
/// acquired, the result is [`Consistency::BestEffort`]. If that is weaker
/// than `policy.required_consistency` the capture fails before any file is
/// read.
///
/// On success every file in the manifest was read exactly once in a state
/// that did not change during the read, and nothing listed changed before
/// the capture finished. On failure objects stored so far remain in the
/// store, unreferenced.
pub fn capture(
    roots: &[RootSpec],
    policy: &CapturePolicy,
    store: &dyn ObjectStore,
    barrier: Option<&dyn CaptureBarrier>,
) -> Result<CaptureResult, CaptureError> {
    capture_with_clock(roots, policy, store, barrier, &SystemClock)
}

/// [`capture`] with the clock that stamps `meta.captured_at`.
pub fn capture_with_clock(
    roots: &[RootSpec],
    policy: &CapturePolicy,
    store: &dyn ObjectStore,
    barrier: Option<&dyn CaptureBarrier>,
    clock: &dyn Clock,
) -> Result<CaptureResult, CaptureError> {
    engine::run(
        roots,
        policy,
        store,
        barrier,
        clock,
        &engine::Tuning::default(),
    )
}
