//! Where everything lives inside a database directory.
//!
//! Every path in the engine is built here. Scattering `format!("{id:06}.vec")` across the write
//! path, the read path, compaction and the repair tool is how a rename ends up half-applied and
//! a segment becomes unreachable — and because collection names become path components, it is
//! also where a traversal bug would hide. One module, one set of rules, validated once.
//!
//! ```text
//! <db-root>/
//! ├── LOCK
//! ├── MANIFEST-A  MANIFEST-B
//! └── collections/<name>/
//!     ├── CATALOG
//!     ├── wal/000001.wal
//!     ├── segments/000001.{vec,dir,meta,del}
//!     └── index/flat-000001.idx
//! ```

use isha_vector_db_format::Slot;

use crate::error::Result;
use crate::path::DbPath;
use crate::validation;

/// Segment ids are zero-padded so a directory listing sorts in creation order, which makes
/// `ls` and the `inspect` command agree with the manifest without anyone having to sort.
const ID_WIDTH: usize = 6;

/// The single-writer lock file.
pub fn lock() -> Result<DbPath> {
    DbPath::root().join("LOCK")
}

/// One of the two manifest slots.
pub fn manifest(slot: Slot) -> Result<DbPath> {
    DbPath::root().join(slot.file_name())
}

/// The directory holding every collection.
pub fn collections_dir() -> Result<DbPath> {
    DbPath::root().join("collections")
}

/// One collection's directory.
///
/// # Errors
/// [`crate::error::ValidationError::InvalidCollectionName`] for a name that could escape the
/// database directory. Checked here as well as at the API boundary: one of the two will
/// eventually be bypassed by a code path nobody thought about.
pub fn collection_dir(name: &str) -> Result<DbPath> {
    validation::check_collection_name(name)?;
    collections_dir()?.join(name)
}

/// A collection's immutable specification.
pub fn catalog(name: &str) -> Result<DbPath> {
    collection_dir(name)?.join("CATALOG")
}

/// A collection's log directory.
pub fn wal_dir(name: &str) -> Result<DbPath> {
    collection_dir(name)?.join("wal")
}

/// One log file.
pub fn wal_file(name: &str, id: u64) -> Result<DbPath> {
    wal_dir(name)?.join(&format!("{id:0ID_WIDTH$}.wal"))
}

/// A collection's segment directory.
pub fn segments_dir(name: &str) -> Result<DbPath> {
    collection_dir(name)?.join("segments")
}

/// Which of a segment's four files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentFile {
    /// The fixed-stride vector block.
    Vectors,
    /// The row directory.
    Directory,
    /// Metadata and content records.
    Metadata,
    /// The live bitmap.
    Tombstones,
}

impl SegmentFile {
    /// The file extension.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Vectors => "vec",
            Self::Directory => "dir",
            Self::Metadata => "meta",
            Self::Tombstones => "del",
        }
    }

    /// All four, in the order a flush writes them.
    ///
    /// The tombstone bitmap is written last because it is the only mutable one: if a crash
    /// leaves a segment incomplete, the missing file is the cheapest one to have lost.
    pub const ALL: [SegmentFile; 4] = [
        Self::Vectors,
        Self::Directory,
        Self::Metadata,
        Self::Tombstones,
    ];
}

/// One of a segment's files.
pub fn segment_file(name: &str, id: u64, which: SegmentFile) -> Result<DbPath> {
    segments_dir(name)?.join(&format!("{id:0ID_WIDTH$}.{}", which.extension()))
}

/// A collection's index directory.
pub fn index_dir(name: &str) -> Result<DbPath> {
    collection_dir(name)?.join("index")
}

/// One index snapshot.
pub fn index_file(name: &str, kind: &str, id: u64) -> Result<DbPath> {
    index_dir(name)?.join(&format!("{kind}-{id:0ID_WIDTH$}.idx"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_where_the_architecture_says_they_are() {
        assert_eq!(lock().unwrap().as_str(), "LOCK");
        assert_eq!(manifest(Slot::A).unwrap().as_str(), "MANIFEST-A");
        assert_eq!(manifest(Slot::B).unwrap().as_str(), "MANIFEST-B");
        assert_eq!(
            collection_dir("products").unwrap().as_str(),
            "collections/products"
        );
        assert_eq!(
            catalog("products").unwrap().as_str(),
            "collections/products/CATALOG"
        );
        assert_eq!(
            wal_file("products", 1).unwrap().as_str(),
            "collections/products/wal/000001.wal"
        );
        assert_eq!(
            segment_file("products", 42, SegmentFile::Vectors)
                .unwrap()
                .as_str(),
            "collections/products/segments/000042.vec"
        );
        assert_eq!(
            index_file("products", "flat", 3).unwrap().as_str(),
            "collections/products/index/flat-000003.idx"
        );
    }

    #[test]
    fn segment_ids_are_padded_so_a_listing_sorts_in_creation_order() {
        let mut names: Vec<String> = [1u64, 2, 10, 100, 1000]
            .into_iter()
            .map(|id| {
                segment_file("c", id, SegmentFile::Vectors)
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let sorted = {
            let mut s = names.clone();
            s.sort();
            s
        };
        names.sort_by_key(|n| n.clone());
        assert_eq!(sorted, names, "lexical order must match numeric order");
        assert_eq!(sorted[0], "000001.vec");
        assert_eq!(sorted[4], "001000.vec");
    }

    #[test]
    fn every_segment_file_kind_has_a_distinct_extension() {
        let mut exts: Vec<&str> = SegmentFile::ALL.iter().map(|f| f.extension()).collect();
        exts.sort_unstable();
        let before = exts.len();
        exts.dedup();
        assert_eq!(exts.len(), before);
    }

    /// A traversal attempt must be refused here, not just at the API boundary.
    #[test]
    fn a_hostile_collection_name_cannot_produce_a_path() {
        for bad in ["..", "../../etc", "a/b", "", "with space", "..\\windows"] {
            assert!(collection_dir(bad).is_err(), "{bad:?} produced a path");
            assert!(catalog(bad).is_err(), "{bad:?} produced a catalog path");
            assert!(
                segment_file(bad, 1, SegmentFile::Vectors).is_err(),
                "{bad:?}"
            );
            assert!(wal_file(bad, 1).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_segment_id_at_the_top_of_the_range_still_produces_a_usable_path() {
        let p = segment_file("c", u64::MAX, SegmentFile::Metadata).unwrap();
        assert!(p.as_str().ends_with(".meta"));
        assert!(p.file_name().unwrap().len() > ID_WIDTH);
    }
}
