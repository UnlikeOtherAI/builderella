//! Typed REST client, event stream, HTTP object store and worker socket.
//!
//! Everything a program needs to talk to a Builderella controller
//! (`docs/architecture.md` §6.15, wire details in §6.16):
//!
//! * [`Client`]: one async method per route of `bella_core::api`, returning
//!   the API's own types.
//! * [`Client::events`]: the durable event stream, resumed from the last
//!   sequence across reconnects.
//! * [`HttpObjectStore`]: the controller's object store as a
//!   `bella_core::store::ObjectStore`, with verified transfers.
//! * [`WorkerSocket`]: the worker's control connection.
//! * [`blocking::BlockingClient`]: the same calls for synchronous code.
//!
//! # Transport
//!
//! There is one rule, applied by every entry point: an `https` controller
//! is reached with the fleet certificate pinned by fingerprint
//! (`bella_net::pinned_client_config`) and never with the public web PKI,
//! so an `https` URL without a fingerprint is an error; an `http`
//! controller is reached without TLS, which `bella_net::ControllerUrl`
//! permits only for loopback or after an explicit opt-in. A server that
//! presents a different certificate is reported as
//! [`ClientError::Tls`] with `pin_mismatch` set, before any request data
//! (including the token) has been sent.
//!
//! # Errors
//!
//! [`ClientError`] keeps what the controller said ([`ClientError::Api`],
//! carrying the `bella_core::ApiError`) apart from failures to reach it
//! (`Transport`, `Tls`, `Timeout`) and from answers that make no sense
//! (`Decode`, `Http`).
//!
//! # Retries
//!
//! A request is repeated only after a transient failure (a transport error
//! or timeout, HTTP 502/503/504, `ErrorCode::Unavailable`) and only if
//! repeating it cannot change the outcome. These are retried:
//!
//! | Request | Why it is safe |
//! | --- | --- |
//! | every `GET` and `HEAD` | reads |
//! | `POST /objects/missing` | a lookup |
//! | `PUT /objects/{digest}` | content-addressed; the same bytes again |
//! | `POST /jobs` | carries the idempotency key, the same one on every try |
//! | `POST /jobs/{id}/cancel` | idempotent on the controller |
//! | `POST /environments/{id}/renew`, `/finish` | idempotent on the controller |
//!
//! Everything else (`POST /tokens`, `/enroll/pairing-codes`, `/enroll`,
//! `/snapshots`, `/plan`, `PUT /projects/{name}`, `PATCH /machines/{id}`)
//! is sent once. Any other 4xx or 5xx answer is never retried. Attempts
//! and total time are bounded by [`RetryPolicy`].
//!
//! # Timeouts
//!
//! Connecting is limited to 10 s and a request to
//! [`ClientOptions::request_timeout`]. The long poll of
//! [`Client::wait_job`] gets its wait on top of that. Streamed bodies (the
//! event stream, object transfers) have no overall limit; a stream that
//! delivers nothing for [`ClientOptions::stream_idle_timeout`] is treated
//! as dead.

#![forbid(unsafe_code)]

mod api;
pub mod blocking;
mod client;
mod error;
mod events;
mod object_store;
mod retry;
mod spool;
mod tls;
mod worker;

pub use api::{MAX_WAIT, new_idempotency_key};
pub use blocking::{BlockingClient, BlockingEvents};
pub use client::{Client, ClientOptions, USER_AGENT};
pub use error::ClientError;
pub use events::EventStream;
pub use object_store::{
    HttpObjectStore, OBJECT_CONTENT_TYPE, OBJECT_LENGTH_HEADER, ObjectStoreOptions,
};
pub use retry::RetryPolicy;
pub use worker::{WorkerSink, WorkerSocket, WorkerSocketOptions, WorkerStream};
