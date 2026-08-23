//! Every resource limit in the engine, in one table.
//!
//! Scattering limits across the modules that enforce them is how a system ends up with three
//! different maximum id lengths, none of them documented. These are part of the public
//! contract: each has a test asserting the boundary and the error, and each appears in the API
//! reference.
//!
//! The numbers are chosen for an embedded database on a phone, not a server. Where a limit
//! exists purely to stop something absurd, it is set generously; where it protects memory, it is
//! set to what a mid-range device can actually hold.

/// Largest vector dimension. Matches the format's own bound.
pub const MAX_DIMENSION: u32 = isha_vector_db_format::MAX_DIMENSION;

/// Longest document id, in bytes.
///
/// Generous enough for a URL or a UUID-with-prefix, bounded because the id map lives in memory
/// and its per-entry cost is the engine's dominant overhead per document.
pub const MAX_DOC_ID_LEN: usize = 512;

/// Longest collection name, in bytes.
pub const MAX_COLLECTION_NAME_LEN: usize = 64;

/// Largest encoded metadata per document, in bytes.
pub const MAX_METADATA_BYTES: usize = 64 * 1024;

/// Deepest metadata nesting. Matches the format's decode-time bound.
pub const MAX_METADATA_DEPTH: usize = isha_vector_db_format::MAX_VALUE_DEPTH;

/// Largest opaque content payload per document, in bytes.
///
/// Content is convenience storage for the text a vector came from, not a blob store. A limit
/// here keeps a segment's metadata file from being dominated by payloads that belong in the
/// application's own storage.
pub const MAX_CONTENT_BYTES: usize = 1024 * 1024;

/// Largest `top_k` a search may request.
pub const MAX_TOP_K: usize = 10_000;

/// Most operations in one atomic batch.
///
/// A batch is buffered before it commits, so this bounds peak memory during a bulk import.
pub const MAX_BATCH_OPS: usize = 100_000;

/// Most nodes in a filter expression.
pub const MAX_FILTER_NODES: usize = 256;

/// Deepest filter nesting.
pub const MAX_FILTER_DEPTH: usize = 32;

/// Characters permitted in a collection name.
///
/// Deliberately narrow. A collection name becomes a path component on every platform we
/// support, so this is a security control as much as a naming convention: nothing here can
/// escape a directory, collide case-insensitively on macOS or Windows, or need escaping in a
/// shell.
pub const fn is_valid_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
