//! Why a capture failed.

use bella_core::snapshot::{Consistency, ManifestError};
use bella_core::store::StoreError;
use std::io;

/// A capture either produces a complete snapshot or one of these. Paths are
/// relative to their source root and prefixed with the mount when the
/// capture has several roots.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    #[error("no source roots were given")]
    NoRoots,

    #[error("source root `{path}`: {reason}")]
    InvalidRoot { path: String, reason: String },

    #[error("invalid `{list}` pattern `{pattern}`: {reason}")]
    InvalidPattern {
        list: &'static str,
        pattern: String,
        reason: String,
    },

    /// Raised before any file is read.
    #[error("capture consistency is {achieved:?} but the policy requires {required:?}")]
    Consistency {
        required: Consistency,
        achieved: Consistency,
    },

    #[error("`{dir}` is inside a git repository but git could not be run: {reason}")]
    GitUnavailable { dir: String, reason: String },

    #[error("`git {command}` failed in `{dir}`: {detail}")]
    Git {
        command: String,
        dir: String,
        detail: String,
    },

    /// `path` is a lossy rendering of the offending name.
    #[error("path `{path}` is not valid UTF-8")]
    NonUtf8Path { path: String },

    #[error(
        "tracked file `{path}` matches secret pattern `{pattern}`; \
         list it in `allow_secrets` to capture it, or stop tracking it"
    )]
    TrackedSecret { path: String, pattern: String },

    #[error(
        "declared input `{path}` matches secret pattern `{pattern}`; \
         list it in `allow_secrets` to capture it"
    )]
    DeclaredInputIsSecret { path: String, pattern: String },

    #[error("declared ignored input `{pattern}` does not exist")]
    MissingDeclaredInput { pattern: String },

    #[error("symlink `{path}` -> `{target}`: {reason}")]
    Symlink {
        path: String,
        target: String,
        reason: String,
    },

    #[error("LFS object not hydrated: `{path}` is a Git LFS pointer; run `git lfs pull`")]
    LfsNotHydrated { path: String },

    #[error(
        "submodule `{path}` is registered but not initialised; \
         run `git submodule update --init`"
    )]
    SubmoduleNotInitialised { path: String },

    /// `missing` holds at most the first twenty paths; `count` is the total.
    #[error(
        "sparse checkout leaves {count} tracked path(s) out of the working tree, \
         first `{}`; check them out or cover them with `exclude`",
        .missing.first().map(String::as_str).unwrap_or_default()
    )]
    SparseCheckout { count: usize, missing: Vec<String> },

    #[error("`{path}` is {size} bytes, over the {limit}-byte file limit")]
    FileTooLarge { path: String, size: u64, limit: u64 },

    #[error("the capture exceeds the {limit}-byte total limit at `{path}`")]
    TotalTooLarge { path: String, limit: u64 },

    #[error("`{path}` kept changing while it was read ({attempts} attempts)")]
    FileChanged { path: String, attempts: u32 },

    #[error("the tree kept changing during the capture ({passes} passes); last change at `{path}`")]
    TreeChanged { path: String, passes: u32 },

    #[error("`{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("storing `{path}`: {source}")]
    Store {
        path: String,
        #[source]
        source: StoreError,
    },

    #[error(transparent)]
    Manifest(#[from] ManifestError),
}
