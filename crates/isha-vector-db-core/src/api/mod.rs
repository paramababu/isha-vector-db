//! The public API: what a user of this library touches.
//!
//! Everything below this module is an implementation detail. The types here are the ones the
//! C ABI wraps and every SDK mirrors, so their shape is a contract long before 1.0 — a change
//! here is six changes once the bindings exist.

mod batch;
mod collection;
mod config;
mod database;
mod search;
mod stats;
mod verify;

pub use batch::{BatchOp, BatchReport, WriteBatch};
pub use collection::{Collection, CompactOptions, CompactReport, Snapshot, UpsertOutcome};
pub use config::{CollectionSpec, DatabaseConfig};
pub use database::{CollectionInfo, Database};
pub use search::{Hit, SearchRequest, SearchResponse, SearchStats};
pub use stats::{CollectionStats, DatabaseStats};
pub use verify::{CollectionVerify, VerifyLevel, VerifyReport};
