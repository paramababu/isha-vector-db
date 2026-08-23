//! What the database will tell you about itself.

use isha_vector_db_format::{IdKind, IndexSpec, Metric, VectorDType};

/// Database-wide counters.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DatabaseStats {
    /// The on-disk format version.
    pub format_version: u16,
    /// The manifest sequence, which increments once per commit.
    pub manifest_sequence: u64,
    /// Collections in the database.
    pub collections: usize,
    /// Live documents across every collection.
    pub live_documents: u64,
    /// Rows including tombstones, so the space compaction would reclaim is visible.
    pub total_rows: u64,
    /// Whether the handle is read-only.
    pub read_only: bool,
    /// Whether the storage backend makes `sync_data` a real durability point.
    ///
    /// `false` in the browser, where OPFS `flush()` is best-effort. Reported rather than
    /// glossed over, so an application can tell its user what guarantee it actually has.
    pub durable_sync: bool,
}

/// One collection's counters.
// No `Eq`: `dead_ratio` is a float, and pretending these compare exactly would be a lie.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CollectionStats {
    /// The collection's name.
    pub name: String,
    /// Vector dimension.
    pub dimension: u32,
    /// Similarity metric.
    pub metric: Metric,
    /// Component type.
    pub dtype: VectorDType,
    /// Document id representation.
    pub id_kind: IdKind,
    /// Index configuration.
    pub index: IndexSpec,
    /// Live documents, memtable included.
    pub live_documents: u64,
    /// Rows on disk, tombstones included.
    pub total_rows: u64,
    /// Segments on disk.
    pub segments: usize,
    /// Documents buffered in memory and not yet in a segment.
    pub buffered_documents: usize,
    /// Approximate bytes the memtable occupies.
    pub memtable_bytes: usize,
    /// Fraction of rows on disk that are tombstones, between 0 and 1.
    ///
    /// The number that says whether compaction is worth running.
    pub dead_ratio: f32,
}
