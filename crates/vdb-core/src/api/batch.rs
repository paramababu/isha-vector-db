//! Atomic write batches.
//!
//! v1 ships batches rather than interactive transactions, and
//! `docs/architecture/07-errors-concurrency-txn.md` §7.4 explains why: an interactive
//! transaction needs either a write lock held for the transaction's lifetime — letting an
//! application deadlock its own database by forgetting to commit, a terrible failure mode for
//! an embedded library — or full MVCC. Batches cover the real workload, which is
//! "ingest a lot of embeddings, then query".
//!
//! A batch is buffered in memory and written as a single append, so its commit record cannot
//! become durable separately from what it commits.

use crate::document::{DocId, DocumentInput};
use crate::error::Result;
use crate::metadata::Metadata;
use crate::validation;

/// One operation in a batch.
///
/// Owned rather than borrowed, unlike [`DocumentInput`]: a batch outlives the call that builds
/// it, so it cannot hold references into the caller's buffers.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BatchOp {
    /// Insert or replace a document.
    Upsert {
        /// The document's id.
        id: DocId,
        /// Its vector, in stored byte form.
        vector: Vec<u8>,
        /// Its metadata, if any.
        metadata: Option<Metadata>,
        /// Its content, if any.
        content: Option<Vec<u8>>,
    },
    /// Remove a document.
    Delete {
        /// The document's id.
        id: DocId,
    },
}

impl BatchOp {
    /// The document this operation concerns.
    pub fn id(&self) -> &DocId {
        match self {
            Self::Upsert { id, .. } | Self::Delete { id } => id,
        }
    }
}

/// A set of writes applied atomically: all of them, or none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteBatch {
    ops: Vec<BatchOp>,
}

impl WriteBatch {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty batch with room for `capacity` operations.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ops: Vec::with_capacity(capacity),
        }
    }

    /// Add an insert-or-replace.
    pub fn upsert(&mut self, doc: DocumentInput<'_>) -> &mut Self {
        self.ops.push(BatchOp::Upsert {
            id: doc.id,
            vector: doc.vector.to_bytes(),
            metadata: doc.metadata,
            content: doc.content.map(<[u8]>::to_vec),
        });
        self
    }

    /// Add a deletion.
    pub fn delete(&mut self, id: impl Into<DocId>) -> &mut Self {
        self.ops.push(BatchOp::Delete { id: id.into() });
        self
    }

    /// Operations in the batch.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the batch is empty. An empty batch is legal and is a no-op.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The operations.
    pub fn ops(&self) -> &[BatchOp] {
        &self.ops
    }

    /// Consume the batch, yielding its operations.
    pub fn into_ops(self) -> Vec<BatchOp> {
        self.ops
    }

    /// Check the batch's size against the engine's limit.
    ///
    /// # Errors
    /// [`crate::error::ValidationError::BatchTooLarge`].
    pub fn validate(&self) -> Result<()> {
        validation::check_batch_size(self.ops.len())
    }
}

/// What a batch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct BatchReport {
    /// Documents that did not previously exist.
    pub inserted: usize,
    /// Documents that replaced an existing one.
    pub updated: usize,
    /// Documents removed.
    pub deleted: usize,
    /// Deletions that matched nothing. Not an error — deleting an absent document is a no-op —
    /// but worth reporting, because a large number usually means the caller's ids are wrong.
    pub missing_deletes: usize,
}

impl BatchReport {
    /// Operations that changed something.
    pub fn changed(&self) -> usize {
        self.inserted + self.updated + self.deleted
    }
}
