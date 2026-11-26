//! End-to-end tests: an in-process controller on a loopback port, real HTTP
//! requests and a scripted worker over the real WebSocket.

// Test helpers return the refused answer by value; its size does not matter.
#![allow(clippy::result_large_err)]

mod api;
mod common;
mod lifecycle;
