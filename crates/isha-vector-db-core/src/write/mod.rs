//! The write path: validate, log, then apply.
pub mod memtable;

pub use memtable::{Lookup, MemRow, Memtable};
