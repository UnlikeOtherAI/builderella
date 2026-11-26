//! Errors returned by [`materialize`](crate::materialize).

use crate::platform::Incompatibility;
use bella_core::ids::Digest;
use bella_core::snapshot::ManifestError;
use bella_core::store::StoreError;
use std::io;
use std::path::PathBuf;

/// Why a manifest was not materialized.
///
/// Paths in the variants are manifest paths relative to the destination,
/// `/`-separated and including the root's mount. Unless the error is
/// [`MaterializeError::CleanupFailed`], nothing that `materialize` created is
/// left behind.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MaterializeError {
    /// `Manifest::validate` rejected the manifest. Nothing was written.
    #[error("invalid manifest: {0}")]
    InvalidManifest(#[from] ManifestError),

    /// The destination exists and already contains something.
    #[error("destination `{}` is not empty", .dest.display())]
    DestinationNotEmpty { dest: PathBuf },

    /// The destination exists and is a file, a symlink or something else
    /// that is not a real directory.
    #[error("destination `{}` exists and is not a directory", .dest.display())]
    DestinationNotDirectory { dest: PathBuf },

    /// The tree would hold more filesystem objects than
    /// `MaterializeOptions::max_files` allows. Every manifest entry counts,
    /// and so does every directory a path implies. `entries` is the number
    /// counted when the limit was passed; counting stops there.
    #[error("the tree needs {entries} or more files, links and directories, the limit is {limit}")]
    TooManyEntries { entries: u64, limit: u64 },

    /// The declared file sizes add up to more than
    /// `MaterializeOptions::max_total_bytes`. `bytes` saturates at `u64::MAX`.
    #[error("manifest declares {bytes} bytes of files, above the limit of {limit}")]
    TooManyBytes { bytes: u64, limit: u64 },

    /// A path component of `path` is a symlink entry of the same manifest, so
    /// creating `path` would write through that link.
    #[error("`{path}` would be created through the symlink `{link}`")]
    LinkInPath { path: String, link: String },

    /// Two entries claim the same place, or a file is used as a directory.
    #[error("`{path}` conflicts with `{with}`")]
    Conflict { path: String, with: String },

    /// The link's target, resolved through the other links of the manifest,
    /// leaves the source root it belongs to.
    #[error("symlink `{path}` -> `{target}` resolves outside its source root")]
    SymlinkEscapes { path: String, target: String },

    /// The manifest's symlinks point through each other so heavily that
    /// working out where they lead was abandoned at `path`.
    #[error("the symlinks of the manifest are too entangled to resolve (stopped at `{path}`)")]
    SymlinksTooComplex { path: String },

    /// A path component is not a plain file name on this platform.
    #[error("`{path}` is not a plain relative path on this platform")]
    UnsafePath { path: String },

    /// The tree cannot be represented exactly on this platform or volume.
    /// The list is what [`check_platform`](crate::check_platform) reports,
    /// plus destination path lengths.
    #[error("{}", describe_incompatibilities(.0))]
    Incompatible(Vec<Incompatibility>),

    /// The store has no object for this file.
    #[error("object {digest} for `{path}` is missing from the store")]
    MissingObject { path: String, digest: Digest },

    /// The object's bytes do not hash to the digest the manifest names.
    #[error("object for `{path}` does not match its digest {digest}")]
    DigestMismatch { path: String, digest: Digest },

    /// The object holds more bytes than the manifest declares. Reading
    /// stopped at the declared size.
    #[error("object {digest} for `{path}` is longer than the declared {expected} bytes")]
    ObjectTooLong {
        path: String,
        digest: Digest,
        expected: u64,
    },

    /// The object ended before the declared size.
    #[error("object {digest} for `{path}` has {actual} bytes, the manifest declares {expected}")]
    ObjectTooShort {
        path: String,
        digest: Digest,
        expected: u64,
        actual: u64,
    },

    /// The store failed while opening or reading the object.
    #[error("cannot read object {digest} for `{path}`: {source}")]
    Store {
        path: String,
        digest: Digest,
        #[source]
        source: StoreError,
    },

    /// The operating system or volume refused to create a symlink (missing
    /// privilege on Windows, or a filesystem without links). The link is
    /// never replaced by a copy of its target.
    #[error("cannot create symlink `{path}`: symlinks are not available here ({source})")]
    SymlinksUnsupported {
        path: String,
        #[source]
        source: io::Error,
    },

    /// A filesystem operation failed.
    #[error("cannot {op} `{}`: {source}", .path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Materialization failed with `error` and removing the partial tree
    /// failed as well, so `dest` may still hold part of it.
    #[error("{error}; removing the partial tree at `{}` also failed: {source}", .dest.display())]
    CleanupFailed {
        error: Box<MaterializeError>,
        dest: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn describe_incompatibilities(list: &[Incompatibility]) -> String {
    match list {
        [] => "the snapshot cannot be materialized on this platform".to_string(),
        [only] => format!("the snapshot cannot be materialized here: {only}"),
        [first, rest @ ..] => format!(
            "the snapshot cannot be materialized here: {first} (and {} more)",
            rest.len()
        ),
    }
}
