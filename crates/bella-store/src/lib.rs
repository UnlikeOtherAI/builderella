//! Content-addressed object store on the local filesystem.
//!
//! [`FsStore`] implements [`bella_core::store::ObjectStore`]. Objects are
//! immutable and named by the SHA-256 of their uncompressed bytes; they are
//! kept zstd-compressed.
//!
//! # Directory layout (version 1)
//!
//! ```text
//! <root>/VERSION                    layout version, the text "1"
//! <root>/lock                       empty file carrying the publish lock
//! <root>/objects/<2 hex>/<digest>   one stored object per file
//! <root>/tmp/<pid>-<nonce>-<n>.tmp  objects being written
//! ```
//!
//! `<2 hex>` is [`Digest::shard`](bella_core::Digest::shard). All 256
//! fan-out directories are created when the store is opened. A root whose
//! `VERSION` is not `1` is refused.
//!
//! # Stored-object format (version 1)
//!
//! Each object file is a 16-byte header followed by exactly one zstd frame
//! (level 3). The header records the uncompressed length, which is what
//! makes `size` a constant-time read. The byte layout and the rules readers
//! enforce are in [`format`]. The same bytes are the wire format of
//! [`FsStore::put_compressed`] and [`FsStore::open_compressed`], so objects
//! travel between stores without recompression; [`format::encode`] and
//! [`format::decode`] produce and consume it outside a store.
//!
//! # Guarantees
//!
//! * **Atomic publication.** An object is written to a temporary file that
//!   no other writer shares, flushed to disk, and renamed into place. A
//!   reader sees a complete object or none. A crash leaves at most stale
//!   files under `tmp/`, which readers never look at.
//! * **What is stored is what was hashed.** `put` hashes the bytes as it
//!   compresses them. A digest mismatch or an object over the size limit
//!   publishes nothing and leaves no temporary file.
//! * **Nothing unverified is returned as valid.** A reader from `open`
//!   yields exactly the recorded number of bytes and ends with `InvalidData`
//!   if the file is damaged, truncated, extended, or does not hash to its
//!   digest. The check completes at end of stream, so content is trusted
//!   only once the reader has returned `Ok(0)`.
//! * **Concurrent use.** Any number of threads and processes may put, read
//!   and collect on one root. Two puts of the same object both succeed and
//!   leave one file.
//! * **Collection never races a put.** See [`FsStore::gc`].
//! * **A damaged object can be put right.** Re-putting an object costs one
//!   header read, so `put` does not notice damage inside a stored body.
//!   [`FsStore::repair`] is the explicit call that reads the stored copy in
//!   full and replaces it if it is damaged; [`FsStore::remove`] deletes one
//!   object unconditionally. Both take the publish lock the way `gc` does.
//!
//! # Platform differences
//!
//! * Unix: the object's directory is fsynced after the rename, so a put
//!   that returned survives power loss.
//! * Windows: the file is flushed before the rename, but a directory cannot
//!   be flushed through the standard library; the rename itself is as
//!   durable as the filesystem's metadata journal makes it. A rename onto an
//!   object that another writer published first can be refused while a
//!   reader has it open; that is treated as already published.
//! * The publish lock is an advisory file lock (`flock` / `LockFileEx`).
//!   Opening a store on a filesystem without one fails.
//! * Ages in `gc` come from file modification times, so they are only as
//!   fine as the filesystem's timestamps (two seconds on FAT).

pub mod format;
mod store;

#[cfg(test)]
mod tests;

pub use store::{FsStore, GcReport, ObjectIter, Repaired, StoreOptions, StoreStats, Verification};
