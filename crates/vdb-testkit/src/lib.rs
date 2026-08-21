//! Shared test machinery: conformance suites, fault injection and deterministic generators.
//!
//! This is a normal dependency rather than a `dev-dependency` of one crate, because the same
//! machinery is needed by the engine's tests, by `vdb-cli`'s verification commands and by the
//! benchmark harness. Without a shared crate it gets copy-pasted three times and the copies
//! drift.

#![forbid(unsafe_code)]
// This crate exists to make tests fail loudly, so panicking is its job rather than a smell.
#![allow(clippy::panic)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]
#![warn(missing_docs)]

pub mod conformance;
pub mod rng;

pub use conformance::{storage_conformance, ConformanceReport};
pub use rng::Rng;
