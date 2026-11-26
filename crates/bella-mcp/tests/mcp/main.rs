//! End-to-end tests: every test drives `bella_mcp::serve` over in-memory
//! pipes against a fake fleet, exactly as a client process would.

mod common;
mod protocol;
mod safety;
mod tools;
