//! Builderella domain model, state machines and wire contracts.
//!
//! This crate performs no I/O. Every other crate depends on it, so its types
//! are the contract between the controller, workers, clients and the
//! dashboard. Changing a serialized shape here is a protocol change.

pub mod api;
pub mod defaults;
pub mod error;
pub mod ids;
pub mod job;
pub mod lease;
pub mod machine;
pub mod platform;
pub mod protocol;
pub mod resources;
pub mod snapshot;
pub mod store;
pub mod time;
pub mod version;

pub use error::{ApiError, ErrorCode};
pub use ids::*;
pub use time::{Clock, ManualClock, SystemClock, UnixMillis};
