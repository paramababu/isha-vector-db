//! Small, dependency-free primitives the engine and the format both need.
//!
//! Everything here is pure: no allocation beyond what the caller asks for, no I/O, no clock.
//! If something in this module starts to feel like a general-purpose library, it belongs
//! somewhere else.

pub mod bitmap;
pub mod crc32c;
pub mod varint;

pub use bitmap::Bitmap;
pub use crc32c::{crc32c, Crc32c};
