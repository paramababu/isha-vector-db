//! Writing and reading segments.
//!
//! A segment is immutable once written, apart from its tombstone bitmap. That is what lets
//! readers work without locks: a reader scanning a segment cannot race a writer, because
//! writers only ever create new segments.
//!
//! # Write ordering
//!
//! A flush writes all four files and syncs each, and only then does the caller commit a
//! manifest naming the new segment. A crash before that commit leaves the files on disk with
//! nothing pointing at them — orphans, which the next open deletes. A crash after it leaves a
//! complete segment. There is no visible state in between, which is the property the crash
//! sweep exists to check.

use std::collections::HashMap;

use vdb_format::segment::{
    Directory, DirectoryWriter, MetaBlock, MetaRecord, MetaWriter, RowEntry, Tombstones,
    VectorBlock, VectorBlockWriter,
};
use vdb_format::{Catalog, FileKind, SegmentRef};

use crate::document::{DocId, Document, Include};
use crate::error::{from_format_at, CorruptionError, Result};
use crate::metadata::Metadata;
use crate::path::DbPath;
use crate::persistence::layout::{self, SegmentFile};
use crate::storage::{OpenMode, Storage};
use crate::vector::{VectorDType, VectorView};
use crate::write::Memtable;

/// What a flush produced.
#[derive(Debug, Clone, PartialEq)]
pub struct FlushResult {
    /// The new segment, ready to be named in a manifest commit.
    pub segment: SegmentRef,
    /// Documents deleted in the memtable that may still exist in older segments.
    ///
    /// A flush cannot resolve these itself: a tombstone for a document the memtable never held
    /// refers to a row in some *other* segment, and finding it needs the collection's id map.
    /// Returned rather than silently dropped.
    pub pending_deletions: Vec<DocId>,
}

/// Write a collection's specification. Called once, when the collection is created.
///
/// # Errors
/// Any storage or format error.
pub fn write_catalog(storage: &dyn Storage, catalog: &Catalog) -> Result<()> {
    let path = layout::catalog(&catalog.name)?;
    storage.create_dir_all(&layout::collection_dir(&catalog.name)?)?;
    let bytes = catalog.encode().map_err(|e| from_format_at(e, &path))?;
    let mut file = storage.open_file(&path, OpenMode::Create)?;
    file.truncate(0)?;
    file.write_at(&bytes, 0)?;
    file.sync_data()
}

/// Read a collection's specification.
///
/// # Errors
/// [`CorruptionError`] if the file is damaged, or any storage error. A missing catalog for a
/// collection the manifest names is corruption, not an absence: the manifest promised it.
pub fn read_catalog(storage: &dyn Storage, name: &str) -> Result<Catalog> {
    let path = layout::catalog(name)?;
    let bytes = read_file(storage, &path)?.ok_or_else(|| {
        crate::DbError::Corruption(CorruptionError::MissingSegment {
            collection: name.to_owned(),
            segment: 0,
        })
    })?;
    Catalog::decode(&bytes).map_err(|e| from_format_at(e, &path))
}

/// Flush a memtable's live rows into a new segment.
///
/// # Errors
/// Any storage or format error.
pub fn flush_memtable(
    storage: &dyn Storage,
    catalog: &Catalog,
    segment_id: u64,
    memtable: &Memtable,
) -> Result<FlushResult> {
    let name = &catalog.name;
    storage.create_dir_all(&layout::segments_dir(name)?)?;

    let stride = catalog.row_stride();
    let mut vectors = VectorBlockWriter::new(catalog.dimension, stride)
        .map_err(|e| from_format_at(e, &DbPath::root()))?;
    let mut directory = DirectoryWriter::new();
    let mut meta = MetaWriter::new();

    for row in memtable.live_rows() {
        let bytes = memtable.vector_bytes(row).ok_or_else(|| {
            crate::internal_error!("memtable row {:?} has no vector bytes", row.id)
        })?;
        vectors
            .push_row(bytes)
            .map_err(|e| from_format_at(e, &DbPath::root()))?;

        let record = MetaRecord {
            metadata: row
                .metadata
                .as_ref()
                .map(|m| vdb_format::Value::Map(m.as_map().clone())),
            content: row.content.clone(),
        };
        let (offset, len) = meta
            .push(&record)
            .map_err(|e| from_format_at(e, &DbPath::root()))?;
        directory
            .push(&row.id.to_bytes(), offset, len, row.inv_norm)
            .map_err(|e| from_format_at(e, &DbPath::root()))?;
    }

    let rows = vectors.rows();
    let tombstones = Tombstones::all_live(rows, 0);

    // All four files, each synced, before the caller commits a manifest naming any of them.
    write_segment_file(
        storage,
        name,
        segment_id,
        SegmentFile::Vectors,
        &vectors.finish(),
    )?;
    write_segment_file(
        storage,
        name,
        segment_id,
        SegmentFile::Directory,
        &directory.finish(),
    )?;
    write_segment_file(
        storage,
        name,
        segment_id,
        SegmentFile::Metadata,
        &meta.finish(),
    )?;
    write_segment_file(
        storage,
        name,
        segment_id,
        SegmentFile::Tombstones,
        &tombstones.encode(),
    )?;

    Ok(FlushResult {
        segment: SegmentRef {
            id: segment_id,
            rows,
            del_generation: 0,
        },
        pending_deletions: memtable.deleted_ids().into_iter().cloned().collect(),
    })
}

/// Rewrite the live rows of several segments into one new segment.
///
/// The rows are copied verbatim — same bytes, same cached norms — so compaction cannot change
/// what a search returns. Only the dead rows disappear.
///
/// Written but not committed: the caller commits a manifest naming the new segment and dropping
/// the old ones, and only then deletes the old files. A crash before the commit leaves the new
/// segment as an orphan; a crash after it leaves the old ones as orphans. Both are cleaned up on
/// the next open, and neither is ever visible as data.
///
/// # Errors
/// Any storage or format error.
pub fn compact_segments(
    storage: &dyn Storage,
    catalog: &Catalog,
    new_id: u64,
    sources: &[&SegmentData],
) -> Result<FlushResult> {
    let name = &catalog.name;
    storage.create_dir_all(&layout::segments_dir(name)?)?;

    let stride = catalog.row_stride();
    let mut vectors = VectorBlockWriter::new(catalog.dimension, stride)
        .map_err(|e| from_format_at(e, &DbPath::root()))?;
    let mut directory = DirectoryWriter::new();
    let mut meta = MetaWriter::new();

    for seg in sources {
        let block = seg.vectors()?;
        for row in 0..seg.rows() {
            if !seg.is_live(row) {
                continue;
            }
            let (Some(bytes), Some(id), Some(inv_norm)) =
                (block.row(row), seg.id_at(row), seg.inv_norm(row))
            else {
                return Err(crate::internal_error!(
                    "segment {} row {row} is live but incomplete",
                    seg.id
                ));
            };
            vectors
                .push_row(bytes)
                .map_err(|e| from_format_at(e, &DbPath::root()))?;
            let record = seg.meta_record(row)?;
            let (offset, len) = meta
                .push(&record)
                .map_err(|e| from_format_at(e, &DbPath::root()))?;
            directory
                .push(&id.to_bytes(), offset, len, inv_norm)
                .map_err(|e| from_format_at(e, &DbPath::root()))?;
        }
    }

    let rows = vectors.rows();
    let tombstones = Tombstones::all_live(rows, 0);
    write_segment_file(
        storage,
        name,
        new_id,
        SegmentFile::Vectors,
        &vectors.finish(),
    )?;
    write_segment_file(
        storage,
        name,
        new_id,
        SegmentFile::Directory,
        &directory.finish(),
    )?;
    write_segment_file(storage, name, new_id, SegmentFile::Metadata, &meta.finish())?;
    write_segment_file(
        storage,
        name,
        new_id,
        SegmentFile::Tombstones,
        &tombstones.encode(),
    )?;

    Ok(FlushResult {
        segment: SegmentRef {
            id: new_id,
            rows,
            del_generation: 0,
        },
        pending_deletions: Vec::new(),
    })
}

/// Delete every file belonging to a segment.
///
/// Used to clean up orphans from an interrupted flush. A missing file is not an error: the
/// crash may have happened between any two of the four writes.
///
/// # Errors
/// Any storage error other than absence.
pub fn remove_segment(storage: &dyn Storage, collection: &str, id: u64) -> Result<()> {
    for which in SegmentFile::ALL {
        let path = layout::segment_file(collection, id, which)?;
        if storage.exists(&path)? {
            storage.remove_file(&path)?;
        }
    }
    Ok(())
}

/// Segment ids present on disk, whether or not the manifest names them.
///
/// Recovery compares this against the manifest: ids here but not there are orphans from an
/// interrupted flush and are removed; ids there but not here are missing data and are reported.
///
/// # Errors
/// Any storage error.
pub fn list_segment_ids(storage: &dyn Storage, collection: &str) -> Result<Vec<u64>> {
    let dir = layout::segments_dir(collection)?;
    if !storage.exists(&dir)? {
        return Ok(Vec::new());
    }
    let mut ids: Vec<u64> = storage
        .list_dir(&dir)?
        .into_iter()
        .filter_map(|e| {
            let (stem, _) = e.name.split_once('.')?;
            stem.parse::<u64>().ok()
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// An open segment, with its four blocks read into memory.
///
/// Reading the whole segment is the right thing for the memory backend and for small
/// collections, and it is what `vdb-storage-os` will replace with a memory map for the vector
/// block specifically — that block is the one that gets large. The interface here does not
/// change when it does, because callers work through [`SegmentData::vectors`].
#[derive(Debug)]
pub struct SegmentData {
    /// The segment's id.
    pub id: u64,
    stride: usize,
    dtype: VectorDType,
    dimension: u32,
    vec_bytes: Vec<u8>,
    /// The metadata file's *payload*, header and trailer already stripped.
    ///
    /// Stored pre-opened because a filtered scan reads one record per candidate row, and
    /// re-validating the file header on each of them showed up plainly in the benchmarks.
    meta_payload: Vec<u8>,
    entries: Vec<RowEntry>,
    ids: Vec<DocId>,
    by_id: HashMap<DocId, u32>,
    tombstones: Tombstones,
}

impl SegmentData {
    /// Open a segment named by a manifest entry.
    ///
    /// # Errors
    /// [`CorruptionError::MissingSegment`] if a file the manifest promised is absent, or any
    /// other corruption or storage error.
    pub fn open(storage: &dyn Storage, catalog: &Catalog, seg: &SegmentRef) -> Result<Self> {
        let name = &catalog.name;
        let vec_bytes = require(storage, name, seg.id, SegmentFile::Vectors)?;
        let dir_bytes = require(storage, name, seg.id, SegmentFile::Directory)?;
        let meta_bytes = require(storage, name, seg.id, SegmentFile::Metadata)?;
        let meta_path = layout::segment_file(name, seg.id, SegmentFile::Metadata)?;
        let meta_payload = MetaBlock::open(&meta_bytes)
            .map_err(|e| from_format_at(e, &meta_path))?
            .payload()
            .to_vec();
        let del_bytes = require(storage, name, seg.id, SegmentFile::Tombstones)?;

        let dir_path = layout::segment_file(name, seg.id, SegmentFile::Directory)?;
        let directory = Directory::open(&dir_bytes).map_err(|e| from_format_at(e, &dir_path))?;
        let tombstones = Tombstones::decode(&del_bytes).map_err(|e| {
            from_format_at(
                e,
                &layout::segment_file(name, seg.id, SegmentFile::Tombstones)
                    .unwrap_or_else(|_| DbPath::root()),
            )
        })?;

        // The manifest, the directory and the bitmap must all agree about how many rows exist.
        // A disagreement means one of the three is from a different flush, which would make
        // every row index mean something different depending on which file answered.
        if directory.rows() != seg.rows || tombstones.rows != seg.rows {
            return Err(CorruptionError::InconsistentIndex {
                collection: name.clone(),
                detail: format!(
                    "manifest says {} rows, directory says {}, bitmap covers {}",
                    seg.rows,
                    directory.rows(),
                    tombstones.rows
                ),
            }
            .into());
        }

        // Decode the row table once. This is the per-segment half of the collection's id map,
        // and the memory it costs is the documented per-document overhead.
        let mut entries = Vec::with_capacity(directory.rows() as usize);
        let mut ids = Vec::with_capacity(directory.rows() as usize);
        let mut by_id = HashMap::with_capacity(directory.rows() as usize);
        for row in 0..directory.rows() {
            let entry = directory.entry(row).ok_or_else(|| {
                crate::internal_error!("directory entry {row} vanished after a successful open")
            })?;
            let raw = directory.id(row).ok_or_else(|| {
                crate::internal_error!("directory id {row} vanished after a successful open")
            })?;
            let id = DocId::from_bytes(catalog.id_kind, raw)?;
            if by_id.insert(id.clone(), row).is_some() {
                return Err(CorruptionError::InconsistentIndex {
                    collection: name.clone(),
                    detail: format!("duplicate id {:?} at row {row}", id.display()),
                }
                .into());
            }
            entries.push(entry);
            ids.push(id);
        }

        Ok(Self {
            id: seg.id,
            stride: catalog.row_stride(),
            dtype: catalog.dtype,
            dimension: catalog.dimension,
            vec_bytes,
            meta_payload,
            entries,
            ids,
            by_id,
            tombstones,
        })
    }

    /// Rows in the segment, live and dead alike.
    pub fn rows(&self) -> u32 {
        self.entries.len() as u32
    }

    /// Live rows.
    pub fn live_count(&self) -> u32 {
        self.tombstones.live_count()
    }

    /// Whether a row is live.
    pub fn is_live(&self, row: u32) -> bool {
        self.tombstones.is_live(row)
    }

    /// Find a document's row, whether or not it is live.
    pub fn row_of(&self, id: &DocId) -> Option<u32> {
        self.by_id.get(id).copied()
    }

    /// A row's id.
    pub fn id_at(&self, row: u32) -> Option<&DocId> {
        self.ids.get(row as usize)
    }

    /// A row's cached reciprocal norm.
    pub fn inv_norm(&self, row: u32) -> Option<f32> {
        self.entries.get(row as usize).map(|e| e.inv_norm)
    }

    /// The vector block, for scanning.
    ///
    /// # Errors
    /// [`CorruptionError`] if the block's header or extent is wrong.
    pub fn vectors(&self) -> Result<VectorBlock<'_>> {
        VectorBlock::open(&self.vec_bytes, self.stride)
            .map_err(|e| from_format_at(e, &DbPath::root()))
    }

    /// Mark a row dead. Returns whether it changed.
    pub fn kill(&mut self, row: u32) -> bool {
        self.tombstones.kill(row)
    }

    /// The tombstone bitmap, for persisting after deletions.
    pub fn tombstones(&self) -> &Tombstones {
        &self.tombstones
    }

    /// Rewrite the tombstone bitmap, bumping its generation.
    ///
    /// The only mutable part of a segment, and the only reason a delete does not rewrite
    /// hundreds of megabytes.
    ///
    /// # Errors
    /// Any storage error.
    pub fn persist_tombstones(&mut self, storage: &dyn Storage, collection: &str) -> Result<u32> {
        self.tombstones.generation = self.tombstones.generation.saturating_add(1);
        write_segment_file(
            storage,
            collection,
            self.id,
            SegmentFile::Tombstones,
            &self.tombstones.encode(),
        )?;
        Ok(self.tombstones.generation)
    }

    /// A row's stored record, metadata and content together.
    ///
    /// Used by compaction, which must carry both across verbatim.
    ///
    /// # Errors
    /// [`CorruptionError`] if the directory points outside the metadata file.
    pub fn meta_record(&self, row: u32) -> Result<MetaRecord> {
        let Some(entry) = self.entries.get(row as usize) else {
            return Ok(MetaRecord::default());
        };
        if entry.meta_len == 0 {
            return Ok(MetaRecord::default());
        }
        MetaBlock::from_payload(&self.meta_payload)
            .record(entry)
            .map_err(|e| from_format_at(e, &DbPath::root()))
    }

    /// Verify every checksum in this segment's files.
    ///
    /// Not done on open — reading a whole vector block to answer "may I open this?" would make
    /// startup unusable on a large collection — so it is a separate, explicit operation.
    ///
    /// # Errors
    /// [`CorruptionError::ChecksumMismatch`] naming the file that failed.
    pub fn verify_checksums(&self, storage: &dyn Storage, collection: &str) -> Result<()> {
        for which in SegmentFile::ALL {
            let path = layout::segment_file(collection, self.id, which)?;
            let Some(bytes) = read_file(storage, &path)? else {
                return Err(CorruptionError::MissingSegment {
                    collection: collection.to_owned(),
                    segment: self.id,
                }
                .into());
            };
            let kind = match which {
                SegmentFile::Vectors => FileKind::Vectors,
                SegmentFile::Directory => FileKind::Directory,
                SegmentFile::Metadata => FileKind::Metadata,
                SegmentFile::Tombstones => FileKind::Deleted,
            };
            vdb_format::verify_block(&bytes, kind).map_err(|e| from_format_at(e, &path))?;
        }
        Ok(())
    }

    /// Read just a row's metadata.
    ///
    /// A filtered scan calls this once per candidate, so it avoids assembling a `Document` and
    /// cloning the id the way [`SegmentData::document`] does.
    ///
    /// # Errors
    /// [`CorruptionError`] if the directory points outside the metadata file.
    pub fn metadata(&self, row: u32) -> Result<Metadata> {
        let Some(entry) = self.entries.get(row as usize) else {
            return Ok(Metadata::new());
        };
        if entry.meta_len == 0 {
            return Ok(Metadata::new());
        }
        let record = MetaBlock::from_payload(&self.meta_payload)
            .record(entry)
            .map_err(|e| from_format_at(e, &DbPath::root()))?;
        match record.metadata {
            Some(vdb_format::Value::Map(m)) => Ok(Metadata::from_map(m)),
            _ => Ok(Metadata::new()),
        }
    }

    /// Read a document out of the segment.
    ///
    /// Returns `None` for a dead row, so callers cannot accidentally resurrect a deleted
    /// document by asking for it by row.
    ///
    /// # Errors
    /// [`CorruptionError`] if the directory points outside the metadata file, or the vector
    /// block is short.
    pub fn document(&self, row: u32, include: Include) -> Result<Option<Document>> {
        if !self.is_live(row) {
            return Ok(None);
        }
        let Some(id) = self.ids.get(row as usize).cloned() else {
            return Ok(None);
        };
        let Some(entry) = self.entries.get(row as usize) else {
            return Ok(None);
        };

        let mut metadata = Metadata::new();
        let mut content = None;
        if include.metadata || include.content {
            let record = MetaBlock::from_payload(&self.meta_payload)
                .record(entry)
                .map_err(|e| from_format_at(e, &DbPath::root()))?;
            if include.metadata {
                if let Some(vdb_format::Value::Map(m)) = record.metadata {
                    metadata = Metadata::from_map(m);
                }
            }
            if include.content {
                content = record.content;
            }
        }

        let vector = if include.vector {
            let block = self.vectors()?;
            let bytes = block.row(row).ok_or_else(|| {
                crate::internal_error!("row {row} is in the directory but not the vector block")
            })?;
            Some(VectorView::raw(self.dtype, bytes, self.dimension)?.to_f32())
        } else {
            None
        };

        Ok(Some(Document {
            id,
            vector,
            metadata,
            content,
        }))
    }
}

fn require(
    storage: &dyn Storage,
    collection: &str,
    id: u64,
    which: SegmentFile,
) -> Result<Vec<u8>> {
    let path = layout::segment_file(collection, id, which)?;
    read_file(storage, &path)?.ok_or_else(|| {
        CorruptionError::MissingSegment {
            collection: collection.to_owned(),
            segment: id,
        }
        .into()
    })
}

fn write_segment_file(
    storage: &dyn Storage,
    collection: &str,
    id: u64,
    which: SegmentFile,
    bytes: &[u8],
) -> Result<()> {
    let path = layout::segment_file(collection, id, which)?;
    let mut file = storage.open_file(&path, OpenMode::Create)?;
    file.truncate(0)?;
    file.write_at(bytes, 0)?;
    file.sync_data()
}

fn read_file(storage: &dyn Storage, path: &DbPath) -> Result<Option<Vec<u8>>> {
    let Some(meta) = storage.metadata(path)? else {
        return Ok(None);
    };
    let file = storage.open_file(path, OpenMode::Read)?;
    let mut bytes = vec![0u8; meta.len as usize];
    let read = file.read_at(&mut bytes, 0)?;
    bytes.truncate(read);
    Ok(Some(bytes))
}
