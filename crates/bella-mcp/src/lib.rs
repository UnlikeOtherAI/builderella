//! MCP stdio server exposing the fleet to coding agents.
//!
//! `bella mcp` runs [`serve`] on its standard streams. The server speaks the
//! Model Context Protocol as hand-written JSON-RPC 2.0 over
//! newline-delimited JSON and offers twelve tools (see [`TOOL_NAMES`]) for
//! planning, submitting, following and cancelling fleet jobs. Everything it
//! does to the fleet goes through the [`FleetOps`] trait, which the binary
//! implements with capture, upload and REST calls.
//!
//! What the server guarantees:
//!
//! * Standard output carries protocol messages only. Diagnostics go to
//!   standard error.
//! * A job id comes back as soon as the job exists. `wait_job` is bounded,
//!   and neither its timeout, a `notifications/cancelled` message nor the
//!   end of input cancels a job. Only an explicit `cancel_job` call reaches
//!   [`FleetOps::cancel`].
//! * Every path argument is resolved through the filesystem and must lie
//!   inside one of [`ServerOptions::allowed_roots`], so a link that points
//!   elsewhere is rejected.
//! * Results are bounded. Log text has terminal control sequences removed
//!   and is labelled as untrusted build output.
//! * There is no tool that changes machine policy, project policy, cloud
//!   budgets or enrollment. A policy denial from the fleet is reported as a
//!   tool error that tells the agent it lacks that authority.
//!
//! Protocol revisions: see [`SUPPORTED_PROTOCOL_VERSIONS`]. Clients that use
//! the `initialize` handshake and clients that send per-request metadata
//! (revision 2026-07-28) are both served, on the same connection.

mod ops;
mod paths;
mod rpc;
mod sanitize;
mod schema;
mod server;
mod tools;

pub use ops::{FleetOps, PlanArgs, SubmitArgs, SubmitOutcome};
pub use rpc::SUPPORTED_PROTOCOL_VERSIONS;
pub use server::{ServerOptions, serve};
pub use tools::TOOL_NAMES;

/// Write one diagnostic line to standard error. A closed or failing
/// standard error is ignored: diagnostics must never take the server down
/// or reach standard output.
pub(crate) fn diag(message: impl std::fmt::Display) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "bella mcp: {message}");
}
