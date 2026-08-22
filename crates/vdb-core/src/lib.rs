//! # vdb-core
//!
//! The engine for an embedded, offline-first vector database.
//!
//! ## The one invariant that matters
//!
//! **This crate performs no I/O.** It does not open files, spawn threads, read the clock, or
//! touch the network. Everything it needs from the outside world arrives through injected
//! traits — chiefly [`Storage`](storage::Storage). That is what makes the same engine run
//! unchanged on Android, iOS, Node, the browser and a desktop, and it is enforced
//! mechanically by `.github/workflows/ci-core-purity.yml`, not by convention.
//!
//! See `docs/architecture/` for the full design.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![warn(missing_docs)]

pub mod api;
pub mod clock;
pub mod document;
pub mod error;
pub mod metadata;
pub mod path;
pub mod persistence;
pub mod storage;
pub mod util;
pub mod validation;
pub mod vector;
pub mod write;

pub use api::{
    BatchReport, Collection, CollectionInfo, CollectionSpec, CollectionStats, Database,
    DatabaseConfig, DatabaseStats, Snapshot, UpsertOutcome, WriteBatch,
};
pub use clock::{Clock, ManualClock};
pub use document::{DocId, Document, DocumentInput, IdKind, Include, RowId};
pub use error::{DbError, ErrorCode, Recoverability, Result};
pub use metadata::{Metadata, Value};
pub use path::DbPath;
pub use persistence::Durability;
pub use vector::{VectorDType, VectorView};
