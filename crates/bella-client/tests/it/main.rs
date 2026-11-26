//! Integration tests against a mock controller on loopback, over plain
//! HTTP and over TLS with a pinned fleet certificate.

mod support;

mod blocking;
mod events;
mod objects;
mod rest;
mod retry;
mod tls;
mod worker;
