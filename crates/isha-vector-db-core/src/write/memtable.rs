//! The in-memory buffer of writes not yet folded into a segment.
//!
//! Everything written goes into the log first and here second. The memtable is what makes a
//! write visible to readers immediately without waiting for a segment flush, and it is what
//! recovery rebuilds by replaying the log.
//!
//! Vectors live in one contiguous arena rather than a `Vec` per document: a million small
//! allocations would fragment the heap and cost more in allocator overhead than the vectors
//! themselves on a 128-dimensional collection.
//!
//! Overwriting a document leaves its old bytes in the arena as garbage. That is deliberate —
//! reclaiming them would mean either compacting the arena (moving every later row, invalidating
//! offsets) or a free list (complexity for a structure whose whole lifetime ends at the next
//! flush). The wasted space is bounded by the flush threshold.

use std::collections::HashMap;

use crate::document::{DocId, RowId};
use crate::error::Result;
use crate::metadata::Metadata;
use crate::vector::{VectorDType, VectorView};

/// What the memtable knows about a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Present, at this row of the memtable.
    Row(u32),
    /// Deleted here. The tombstone must be kept even if the document was never in the
    /// memtable, because it may exist in a segment underneath.
    Deleted,
}

/// One buffered document.
#[derive(Debug, Clone, PartialEq)]
pub struct MemRow {
    /// The document's id.
    pub id: DocId,
    /// Where its vector starts in the arena.
    pub offset: usize,
    /// Cached reciprocal norm, computed once on write rather than on every search.
    pub inv_norm: f32,
    /// Its metadata, if any.
    pub metadata: Option<Metadata>,
    /// Its content, if any.
    pub content: Option<Vec<u8>>,
}

/// What a lookup found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lookup<'a> {
    /// The document is present in the memtable.
    Present(&'a MemRow),
    /// The document was deleted here, so any older copy in a segment is shadowed.
    Deleted,
}

/// A buffer of pending writes for one collection.
#[derive(Debug)]
pub struct Memtable {
    dimension: u32,
    dtype: VectorDType,
    stride: usize,
    arena: Vec<u8>,
    rows: Vec<MemRow>,
    index: HashMap<DocId, Slot>,
    tombstones: usize,
    metadata_bytes: usize,
}

impl Memtable {
    /// An empty memtable for a collection of the given shape.
    pub fn new(dimension: u32, dtype: VectorDType) -> Self {
        Self {
            dimension,
            dtype,
            stride: dtype.row_stride(dimension),
            arena: Vec::new(),
            rows: Vec::new(),
            index: HashMap::new(),
            tombstones: 0,
            metadata_bytes: 0,
        }
    }

    /// Vector dimension.
    pub fn dimension(&self) -> u32 {
        self.dimension
    }

    /// Live documents buffered here. Excludes tombstones and superseded versions.
    pub fn len(&self) -> usize {
        self.index.len() - self.tombstones
    }

    /// Whether nothing at all is buffered, tombstones included.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Tombstones buffered here.
    pub fn tombstones(&self) -> usize {
        self.tombstones
    }

    /// Approximate heap footprint, used to decide when to flush.
    ///
    /// Approximate on purpose: an exact figure would mean walking every metadata value on every
    /// write. This tracks the parts that actually dominate — the vector arena and encoded
    /// metadata — and ignores per-entry map overhead.
    pub fn byte_size(&self) -> usize {
        self.arena.len() + self.metadata_bytes + self.rows.len() * 64
    }

    /// Insert or replace a document from raw stored bytes.
    ///
    /// # Errors
    /// [`crate::error::ValidationError`] if the vector's length does not match the collection's
    /// stride.
    pub fn put_bytes(
        &mut self,
        id: DocId,
        vector: &[u8],
        metadata: Option<Metadata>,
        content: Option<Vec<u8>>,
    ) -> Result<RowId> {
        let view = VectorView::raw(self.dtype, vector, self.dimension)?;
        self.put_view(id, view, metadata, content)
    }

    /// Insert or replace a document.
    ///
    /// # Errors
    /// [`crate::error::ValidationError`] on a dimension mismatch.
    pub fn put_view(
        &mut self,
        id: DocId,
        vector: VectorView<'_>,
        metadata: Option<Metadata>,
        content: Option<Vec<u8>>,
    ) -> Result<RowId> {
        vector.check_dimension("<memtable>", self.dimension)?;
        let offset = self.arena.len();
        vector.write_bytes(&mut self.arena);

        self.metadata_bytes += metadata.as_ref().map_or(0, |m| m.len() * 32);
        self.metadata_bytes += content.as_ref().map_or(0, Vec::len);

        let row = MemRow {
            id: id.clone(),
            offset,
            inv_norm: vector.inv_norm(),
            metadata,
            content,
        };
        let index = self.rows.len() as u32;
        self.rows.push(row);
        if self.index.insert(id, Slot::Row(index)) == Some(Slot::Deleted) {
            // Re-inserting a deleted document clears its tombstone.
            self.tombstones -= 1;
        }
        Ok(RowId::new(u32::MAX, index))
    }

    /// Record a deletion.
    ///
    /// Returns whether this changed anything here. It always records a tombstone, even for a
    /// document the memtable has never seen: the document may exist in a segment underneath,
    /// and the tombstone is what shadows it.
    pub fn delete(&mut self, id: DocId) -> bool {
        match self.index.insert(id, Slot::Deleted) {
            Some(Slot::Deleted) => false,
            Some(Slot::Row(_)) => {
                self.tombstones += 1;
                true
            }
            None => {
                self.tombstones += 1;
                true
            }
        }
    }

    /// Look a document up.
    pub fn get(&self, id: &DocId) -> Option<Lookup<'_>> {
        match self.index.get(id)? {
            Slot::Deleted => Some(Lookup::Deleted),
            Slot::Row(i) => self.rows.get(*i as usize).map(Lookup::Present),
        }
    }

    /// A document's vector bytes.
    pub fn vector_bytes(&self, row: &MemRow) -> Option<&[u8]> {
        self.arena
            .get(row.offset..row.offset.checked_add(self.stride)?)
    }

    /// Live rows, in insertion order.
    ///
    /// Insertion order rather than hash order, so a flush produces byte-identical segments for
    /// an identical sequence of writes. Determinism here is what makes golden segment fixtures
    /// and compaction verification possible at all.
    pub fn live_rows(&self) -> Vec<&MemRow> {
        let mut indices: Vec<u32> = self
            .index
            .values()
            .filter_map(|s| match s {
                Slot::Row(i) => Some(*i),
                Slot::Deleted => None,
            })
            .collect();
        indices.sort_unstable();
        indices
            .iter()
            .filter_map(|i| self.rows.get(*i as usize))
            .collect()
    }

    /// Ids deleted here, sorted for determinism.
    pub fn deleted_ids(&self) -> Vec<&DocId> {
        let mut ids: Vec<&DocId> = self
            .index
            .iter()
            .filter(|(_, s)| matches!(s, Slot::Deleted))
            .map(|(id, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Discard everything, keeping the allocated capacity for the next batch.
    pub fn clear(&mut self) {
        self.arena.clear();
        self.rows.clear();
        self.index.clear();
        self.tombstones = 0;
        self.metadata_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::Value;

    fn table() -> Memtable {
        Memtable::new(2, VectorDType::F32)
    }

    fn put(m: &mut Memtable, id: &str, v: [f32; 2]) {
        m.put_view(DocId::from(id), VectorView::f32(&v), None, None)
            .unwrap();
    }

    #[test]
    fn an_empty_memtable_reports_itself_empty() {
        let m = table();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.tombstones(), 0);
        assert!(m.live_rows().is_empty());
        assert!(m.get(&DocId::from("nope")).is_none());
    }

    #[test]
    fn put_then_get_round_trips_including_the_vector_bytes() {
        let mut m = table();
        let mut meta = Metadata::new();
        meta.insert("k", Value::I64(7));
        m.put_view(
            DocId::from("a"),
            VectorView::f32(&[3.0, 4.0]),
            Some(meta.clone()),
            Some(b"content".to_vec()),
        )
        .unwrap();

        match m.get(&DocId::from("a")) {
            Some(Lookup::Present(row)) => {
                assert_eq!(row.id, DocId::from("a"));
                assert_eq!(row.metadata.as_ref(), Some(&meta));
                assert_eq!(row.content.as_deref(), Some(b"content".as_slice()));
                assert!(
                    (row.inv_norm - 0.2).abs() < 1e-6,
                    "norm should be cached on write"
                );
                let bytes = m.vector_bytes(row).unwrap();
                assert_eq!(
                    VectorView::raw(VectorDType::F32, bytes, 2)
                        .unwrap()
                        .to_f32(),
                    vec![3.0, 4.0]
                );
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn overwriting_supersedes_without_growing_the_live_count() {
        let mut m = table();
        put(&mut m, "a", [1.0, 0.0]);
        put(&mut m, "a", [0.0, 1.0]);
        assert_eq!(m.len(), 1);
        assert_eq!(m.live_rows().len(), 1);

        match m.get(&DocId::from("a")) {
            Some(Lookup::Present(row)) => {
                let bytes = m.vector_bytes(row).unwrap();
                assert_eq!(
                    VectorView::raw(VectorDType::F32, bytes, 2)
                        .unwrap()
                        .to_f32(),
                    vec![0.0, 1.0],
                    "the later write must win"
                );
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    /// A tombstone for a document the memtable never held is the case that matters: it shadows
    /// a copy sitting in a segment underneath.
    #[test]
    fn deleting_an_unknown_document_still_records_a_tombstone() {
        let mut m = table();
        assert!(m.delete(DocId::from("never-seen")));
        assert_eq!(m.tombstones(), 1);
        assert_eq!(m.len(), 0);
        assert_eq!(m.get(&DocId::from("never-seen")), Some(Lookup::Deleted));
        assert_eq!(m.deleted_ids(), vec![&DocId::from("never-seen")]);
    }

    #[test]
    fn deleting_twice_reports_no_further_change() {
        let mut m = table();
        put(&mut m, "a", [1.0, 1.0]);
        assert!(m.delete(DocId::from("a")));
        assert!(!m.delete(DocId::from("a")));
        assert_eq!(m.tombstones(), 1);
        assert_eq!(m.len(), 0);
        assert!(m.live_rows().is_empty());
    }

    #[test]
    fn reinserting_a_deleted_document_clears_its_tombstone() {
        let mut m = table();
        put(&mut m, "a", [1.0, 0.0]);
        m.delete(DocId::from("a"));
        assert_eq!(m.tombstones(), 1);
        put(&mut m, "a", [0.0, 1.0]);
        assert_eq!(m.tombstones(), 0);
        assert_eq!(m.len(), 1);
        assert!(matches!(m.get(&DocId::from("a")), Some(Lookup::Present(_))));
    }

    /// Determinism: the same sequence of writes must always flush in the same order, or
    /// segment bytes stop being reproducible.
    #[test]
    fn live_rows_are_returned_in_insertion_order_not_hash_order() {
        let ids: Vec<String> = (0..50).map(|i| format!("doc-{i:03}")).collect();
        let mut first = table();
        for id in &ids {
            first
                .put_view(
                    DocId::from(id.clone()),
                    VectorView::f32(&[1.0, 2.0]),
                    None,
                    None,
                )
                .unwrap();
        }
        let order: Vec<DocId> = first.live_rows().iter().map(|r| r.id.clone()).collect();
        let expected: Vec<DocId> = ids.iter().cloned().map(DocId::from).collect();
        assert_eq!(order, expected);

        // And the same again from a separately built table.
        let mut second = table();
        for id in &ids {
            second
                .put_view(
                    DocId::from(id.clone()),
                    VectorView::f32(&[1.0, 2.0]),
                    None,
                    None,
                )
                .unwrap();
        }
        let order2: Vec<DocId> = second.live_rows().iter().map(|r| r.id.clone()).collect();
        assert_eq!(order, order2);
    }

    #[test]
    fn a_wrong_dimension_is_refused() {
        let mut m = table();
        assert!(m
            .put_view(DocId::from("a"), VectorView::f32(&[1.0]), None, None)
            .is_err());
        assert!(m
            .put_bytes(DocId::from("a"), &[0u8; 4], None, None)
            .is_err());
        assert!(m.put_bytes(DocId::from("a"), &[0u8; 8], None, None).is_ok());
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn byte_size_grows_with_the_arena_and_resets_on_clear() {
        let mut m = table();
        let empty = m.byte_size();
        for i in 0..100 {
            put(&mut m, &format!("d{i}"), [1.0, 2.0]);
        }
        assert!(m.byte_size() > empty + 100 * 8);
        m.clear();
        assert_eq!(m.byte_size(), empty);
        assert!(m.is_empty());
    }
}
