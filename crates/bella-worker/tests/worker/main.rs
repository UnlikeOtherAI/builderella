//! End-to-end tests of the worker daemon against a scripted controller,
//! with the host's real containment adapter and real child processes.
//!
//! Every managed process is this test binary itself, started through
//! [`helper_entry`] in a mode named by its environment, so the tests run
//! the same commands on every operating system.

mod support;

mod commands;
mod lifecycle;
mod logs;
mod protection;
mod recovery;

/// Entry point of the helper processes. Does nothing in a normal test run.
#[test]
fn helper_entry() {
    support::helper_entry();
}
