//! Integrity checking.
//!
//! Three levels, because the useful question differs by situation. An application opening a
//! database wants to know it is structurally sound without reading every byte; a support
//! engineer looking at a user's damaged file wants everything checked.
//!
//! Verification **reports** rather than repairs. Deciding what to discard is not a decision a
//! library should make silently on a user's behalf, and a report is what makes the decision
//! possible.

/// How thoroughly to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum VerifyLevel {
    /// Headers and the manifest only. Milliseconds, whatever the database size.
    Quick,
    /// Every block's checksum. Reads every byte, so it costs what the database is big.
    Checksums,
    /// Checksums plus cross-file consistency: row counts, id uniqueness, reachable metadata.
    Full,
}

/// What verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReport {
    /// The level that was run.
    pub level: VerifyLevel,
    /// Per-collection results.
    pub collections: Vec<CollectionVerify>,
    /// Problems that mean data is damaged or unreadable.
    pub errors: Vec<String>,
    /// Things that are odd but not damage — orphan files, an unusually high dead ratio.
    pub warnings: Vec<String>,
}

impl VerifyReport {
    /// Whether nothing was found wrong.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// Segments checked across every collection.
    pub fn segments_checked(&self) -> usize {
        self.collections.iter().map(|c| c.segments_checked).sum()
    }
}

/// One collection's results.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectionVerify {
    /// The collection's name.
    pub name: String,
    /// Segments inspected.
    pub segments_checked: usize,
    /// Live documents counted directly, rather than taken from the manifest.
    pub live_documents: u64,
    /// Rows including tombstones.
    pub total_rows: u64,
}
