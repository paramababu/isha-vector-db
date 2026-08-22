//! The vdb on-disk format.
//!
//! This crate is the only place that knows what a vdb database looks like as bytes. It is
//! separate from `vdb-core` for three reasons:
//!
//! 1. **A migration tool must read old formats without linking the current engine.** If the
//!    decoders lived in the engine, migrating a v1 database with a v3 build would mean keeping
//!    v1 decode paths alive inside the engine forever.
//! 2. **The format needs its own fuzz corpus, golden fixtures and semver.** They belong next to
//!    the code they protect.
//! 3. **It has no dependencies, on purpose.** A third-party codec is entitled to change its
//!    encoding in a minor release; ours is a published contract with users' data. See
//!    ADR-0004.
//!
//! # The rule that governs every decoder here
//!
//! **Never allocate based on a length field before checking it against the bytes actually
//! available.** A corrupt or hostile file's length prefix is the single most common way a parser
//! becomes an out-of-memory crash. Every read goes through [`Reader`], which bounds-checks
//! before it hands back anything.
//!
//! # Layout conventions
//!
//! - Little-endian throughout. Every target we support is little-endian; this is recorded as a
//!   format invariant so a future big-endian port knows exactly what it must convert.
//! - Every file opens with a 32-byte [`FileHeader`] carrying its kind, format version and a
//!   header checksum, so a reader knows what it is holding before interpreting a single byte
//!   of payload.
//! - Encodings are **canonical**: one logical value has exactly one byte representation.
//!   Without that, checksums and golden fixtures are not reproducible and compaction cannot be
//!   verified.

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

pub mod block;
pub mod catalog;
pub mod crc32c;
pub mod cursor;
pub mod error;
pub mod header;
pub mod manifest;
pub mod segment;
pub mod value;
pub mod varint;
pub mod wal;

pub use block::{decode_block, encode_block, open_block, verify_block};
pub use catalog::{Catalog, IdKind, IndexSpec, Metric, VectorDType, MAX_DIMENSION};
pub use crc32c::{crc32c, Crc32c};
pub use cursor::{Reader, Writer};
pub use error::{FormatError, MalformedKind, Result};
pub use header::{FileHeader, FileKind, HeaderFlags, HEADER_LEN};
pub use manifest::{CollectionEntry, Manifest, SegmentRef, Slot, SlotScan, SlotStatus};
pub use segment::{
    Directory, DirectoryWriter, MetaBlock, MetaRecord, MetaWriter, RowEntry, Tombstones,
    VectorBlock, VectorBlockWriter,
};
pub use value::{find_path, skip_value, Value, MAX_VALUE_DEPTH};
pub use wal::{WalFrame, WalOp, WalScan, WalTail};

// The format is little-endian throughout, and the index crate reads stored vectors by
// reinterpreting bytes as `f32` in native order — which is only correct on a little-endian
// target. Every platform this project supports is little-endian, and §1.4 records that as an
// explicit non-goal rather than an oversight. Making it a build failure means a future port
// meets a clear message instead of silently wrong distances.
#[cfg(target_endian = "big")]
compile_error!(
    "vdb's on-disk format is little-endian only (see docs/architecture/01-scope.md §1.4). \
     Porting to a big-endian target requires byte-swapping every integer and float in \
     vdb-format, and reworking how vdb-index-flat reads stored vectors."
);

/// The format version this build writes.
pub const FORMAT_VERSION: u16 = 1;

/// The oldest format version this build can read.
///
/// Opening anything outside `MIN_READABLE_VERSION..=FORMAT_VERSION` fails loudly rather than
/// attempting a best-effort read of a layout we do not understand.
pub const MIN_READABLE_VERSION: u16 = 1;

/// Shared prefix of every file's magic. The four bytes after it identify the file kind.
pub const MAGIC_PREFIX: [u8; 4] = *b"VDB1";
