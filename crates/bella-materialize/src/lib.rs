//! Materialize a snapshot manifest into a fresh directory.
//!
//! A worker receives a [`Manifest`](bella_core::snapshot::Manifest) from
//! another machine and turns it, together with the objects it names, into a
//! directory tree. The manifest is treated as hostile input:
//!
//! * [`materialize`] validates it, refuses anything the local platform cannot
//!   represent exactly, and only then writes. Every write stays inside the
//!   destination: directories first, then files with `create_new`, then
//!   symlinks last, so no write can travel through a link.
//! * Object sizes and digests are checked while the bytes are written. Any
//!   failure removes what was created and names the offending path.
//! * The tree is always built from nothing, so a file deleted at the origin
//!   cannot survive from an earlier checkout. Every file is its own copy;
//!   nothing is hard-linked or reflinked from a cache.
//!
//! [`check_platform`] reports platform problems without touching the disk,
//! [`verify_tree`] compares a directory with a manifest exactly, and
//! [`remove_tree`] deletes a workspace without following links.
//!
//! See `docs/architecture.md` §6.3 for the contract.

mod error;
mod plan;
mod platform;
mod probe;
mod remove;
mod verify;
mod write;

pub use error::MaterializeError;
pub use platform::{Incompatibility, PathLimits, check_platform};
pub use probe::is_case_sensitive;
pub use remove::remove_tree;
pub use verify::{EntryKind, TreeMismatch, verify_tree};
pub use write::{MaterializeOptions, MaterializeReport, materialize};
