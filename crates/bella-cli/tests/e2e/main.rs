//! End-to-end tests of the `bella` binary: a real controller, real workers
//! and the client commands, all on loopback, all started from the built
//! binary. Nothing here calls into the CLI's code.

mod support;

mod access;
mod flows;
mod lifecycle;
mod mcp;
