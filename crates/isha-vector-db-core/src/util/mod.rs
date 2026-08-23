//! Small, dependency-free primitives the engine and the format both need.
//!
//! Everything here is pure: no allocation beyond what the caller asks for, no I/O, no clock.
//! If something in this module starts to feel like a general-purpose library, it belongs
//! somewhere else.

pub mod bitmap;

pub use bitmap::Bitmap;

// Checksums and varints are properties of the on-disk format, not of the engine, so they live
// in `isha-vector-db-format` — which must stay usable by migration tooling without linking the engine.
// Re-exported here because the engine's persistence layer uses them constantly.
pub use isha_vector_db_format::{crc32c, varint, Crc32c};
