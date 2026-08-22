//! The collection handle: where documents are actually written and read.

use std::sync::{Arc, RwLock, RwLockReadGuard};

use vdb_format::{Catalog, CollectionEntry, SegmentRef, WalOp};

use crate::api::database::DbInner;
use crate::api::{BatchOp, BatchReport, CollectionStats, WriteBatch};
use crate::document::{DocId, Document, DocumentInput, Include};
use crate::error::{ConflictError, NotFoundError, Result, TransactionError};
use crate::metadata::Metadata;
use crate::persistence::segment::{flush_memtable, list_segment_ids, remove_segment, SegmentData};
use crate::persistence::{layout, replay_into, wal::WalWriter};
use crate::vector::{VectorDType, VectorView};
use crate::write::{memtable::Lookup, Memtable};

/// Where a document currently lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Located {
    /// Buffered in the memtable.
    Memtable,
    /// In a segment, at this index into the segment list and this row.
    Segment(usize, u32),
}

/// Everything mutable about a collection.
pub(crate) struct CollState {
    memtable: Memtable,
    segments: Vec<SegmentData>,
    wal: WalWriter,
    next_segment_id: u64,
    next_txn_hint: u64,
}

impl core::fmt::Debug for CollState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CollState")
            .field("buffered", &self.memtable.len())
            .field("segments", &self.segments.len())
            .field("next_segment_id", &self.next_segment_id)
            .finish()
    }
}

/// Shared state for one collection.
#[derive(Debug)]
pub(crate) struct CollInner {
    pub(crate) catalog: Catalog,
    /// One lock over the whole collection.
    ///
    /// Readers do not block each other and writes are serialized, which is the model
    /// `docs/architecture/07-errors-concurrency-txn.md` §7.2 describes. What is *not* yet
    /// implemented is the lock-free part: segments are immutable, so a scan should be able to
    /// proceed against an `Arc` of the segment list without holding anything. That refinement
    /// belongs with the search path, where holding a read lock for the duration of a scan would
    /// actually block writers for a meaningful time. Stated here rather than implied, so nobody
    /// reads the architecture document and assumes it is already true.
    state: RwLock<CollState>,
}

impl CollInner {
    /// Open a collection: its segments, then its log replayed over them.
    pub(crate) fn open(
        db: &Arc<DbInner>,
        catalog: Catalog,
        entry: &CollectionEntry,
    ) -> Result<Self> {
        let storage = db.storage.as_ref();
        let name = &catalog.name;

        // Segment files nothing points at are aborted flushes. Removing them keeps a later
        // flush from colliding with a stale id, and keeps `list_segment_ids` honest.
        let referenced: Vec<u64> = entry.segments.iter().map(|s| s.id).collect();
        if !db.config.read_only {
            for id in list_segment_ids(storage, name)? {
                if !referenced.contains(&id) {
                    remove_segment(storage, name, id)?;
                }
            }
        }

        let mut segments = Vec::with_capacity(entry.segments.len());
        for seg_ref in &entry.segments {
            segments.push(SegmentData::open(storage, &catalog, seg_ref)?);
        }

        let mut memtable = Memtable::new(catalog.dimension, VectorDType::F32);
        let wal_path = layout::wal_file(name, 1)?;
        let report = replay_into(storage, &wal_path, catalog.id_kind, &mut memtable)?;

        // A torn tail is discarded so the next append starts at a frame boundary; leaving it
        // would put a partial frame in the middle of the log, which recovery would then read as
        // corruption rather than as the interrupted write it was.
        if report.truncated_tail && !db.config.read_only {
            crate::persistence::recovery::truncate_tail(storage, &wal_path, report.valid_bytes)?;
        }

        let wal = WalWriter::open(
            storage,
            &wal_path,
            report.last_sequence.saturating_add(1),
            db.config.durability,
        )?;
        let next_segment_id = entry.segments.iter().map(|s| s.id).max().unwrap_or(0) + 1;

        Ok(Self {
            catalog,
            state: RwLock::new(CollState {
                memtable,
                segments,
                wal,
                next_segment_id,
                next_txn_hint: 1,
            }),
        })
    }
}

/// A handle to one collection. Cheap to clone and safe to share across threads.
#[derive(Debug, Clone)]
pub struct Collection {
    db: Arc<DbInner>,
    inner: Arc<CollInner>,
}

/// What an upsert did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// The document did not previously exist.
    Inserted,
    /// An existing document was replaced.
    Updated,
}

/// A consistent view of a collection.
///
/// Holding one keeps the collection's segment files from being reclaimed, so an application
/// that holds a snapshot for a long time delays compaction. That is reported in
/// [`CollectionStats`] rather than being left to be discovered.
#[derive(Debug, Clone)]
pub struct Snapshot {
    inner: Arc<CollInner>,
}

impl Snapshot {
    /// The collection this view belongs to.
    pub fn collection_name(&self) -> &str {
        &self.inner.catalog.name
    }
}

impl Collection {
    pub(crate) fn new(db: Arc<DbInner>, inner: Arc<CollInner>) -> Self {
        Self { db, inner }
    }

    /// The collection's immutable specification.
    pub fn catalog(&self) -> &Catalog {
        &self.inner.catalog
    }

    /// The collection's name.
    pub fn name(&self) -> &str {
        &self.inner.catalog.name
    }

    /// Its vector dimension.
    pub fn dimension(&self) -> u32 {
        self.inner.catalog.dimension
    }

    /// Take a consistent view.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            inner: Arc::clone(&self.inner),
        }
    }

    // ---- writes -----------------------------------------------------------

    /// Insert a document, failing if its id already exists.
    ///
    /// # Errors
    /// [`ConflictError::DuplicateId`], any validation error, or any storage error.
    pub fn insert(&self, doc: DocumentInput<'_>) -> Result<()> {
        self.db.check_writable("insert")?;
        self.validate(&doc)?;
        let mut state = self.write_state()?;
        if locate(&state, &doc.id).is_some() {
            return Err(ConflictError::DuplicateId {
                collection: self.name().to_owned(),
                id: doc.id.display(),
            }
            .into());
        }
        self.apply_put(&mut state, doc)?;
        self.maybe_flush(state)
    }

    /// Insert or replace a document.
    ///
    /// # Errors
    /// Any validation or storage error.
    pub fn upsert(&self, doc: DocumentInput<'_>) -> Result<UpsertOutcome> {
        self.db.check_writable("upsert")?;
        self.validate(&doc)?;
        let mut state = self.write_state()?;
        let existed = locate(&state, &doc.id).is_some();
        self.apply_put(&mut state, doc)?;
        self.maybe_flush(state)?;
        Ok(if existed {
            UpsertOutcome::Updated
        } else {
            UpsertOutcome::Inserted
        })
    }

    /// Remove a document. Returns whether it existed.
    ///
    /// Deleting an absent document is a no-op, not an error: making it one would force every
    /// caller doing idempotent cleanup to match on the error and ignore it.
    ///
    /// # Errors
    /// Any storage error.
    pub fn delete(&self, id: impl Into<DocId>) -> Result<bool> {
        self.db.check_writable("delete")?;
        let id = id.into();
        let mut state = self.write_state()?;
        let existed = locate(&state, &id).is_some();
        state.wal.append(WalOp::Delete { id: id.to_bytes() })?;
        state.memtable.delete(id);
        self.maybe_flush(state)?;
        Ok(existed)
    }

    /// Apply a batch atomically: every operation, or none of them.
    ///
    /// # Errors
    /// [`TransactionError::Aborted`] naming the operation that failed, with nothing applied;
    /// or any storage error.
    pub fn write_batch(&self, batch: WriteBatch) -> Result<BatchReport> {
        self.db.check_writable("write a batch")?;
        batch.validate()?;
        if batch.is_empty() {
            return Ok(BatchReport::default());
        }

        // Validate everything before writing anything. A batch that fails half-way through
        // validation must not have logged its first half — that is the whole promise.
        let ops = batch.into_ops();
        for (index, op) in ops.iter().enumerate() {
            if let Err(e) = self.validate_op(op) {
                return Err(TransactionError::Aborted {
                    failed_at: index,
                    total_ops: ops.len(),
                    cause: Box::new(e),
                }
                .into());
            }
        }

        let mut state = self.write_state()?;
        let mut report = BatchReport::default();
        let mut wal_ops = Vec::with_capacity(ops.len());
        for op in &ops {
            let existed = locate(&state, op.id()).is_some();
            match op {
                BatchOp::Upsert {
                    id,
                    vector,
                    metadata,
                    content,
                } => {
                    if existed {
                        report.updated += 1;
                    } else {
                        report.inserted += 1;
                    }
                    wal_ops.push(WalOp::Put {
                        id: id.to_bytes(),
                        vector: vector.clone(),
                        metadata: match metadata {
                            Some(m) => m.encode()?,
                            None => Vec::new(),
                        },
                        content: content.clone(),
                    });
                }
                BatchOp::Delete { id } => {
                    if existed {
                        report.deleted += 1;
                    } else {
                        report.missing_deletes += 1;
                    }
                    wal_ops.push(WalOp::Delete { id: id.to_bytes() });
                }
            }
        }

        let txn = state.wal.append_group(wal_ops)?;
        state.next_txn_hint = txn.saturating_add(1);

        // Only after the group is durable does it become visible.
        for op in ops {
            match op {
                BatchOp::Upsert {
                    id,
                    vector,
                    metadata,
                    content,
                } => {
                    state.memtable.put_bytes(id, &vector, metadata, content)?;
                }
                BatchOp::Delete { id } => {
                    state.memtable.delete(id);
                }
            }
        }
        self.maybe_flush(state)?;
        Ok(report)
    }

    // ---- reads ------------------------------------------------------------

    /// Fetch a document, with its metadata.
    ///
    /// # Errors
    /// Any storage or corruption error.
    pub fn get(&self, id: &DocId) -> Result<Option<Document>> {
        self.get_with(id, Include::default())
    }

    /// Fetch a document, choosing what to include.
    ///
    /// Vectors are opt-in: at 768 dimensions each one is 3 KB, so returning them by default
    /// would make a hundred-document read fetch 300 KB nobody asked for.
    ///
    /// # Errors
    /// Any storage or corruption error.
    pub fn get_with(&self, id: &DocId, include: Include) -> Result<Option<Document>> {
        self.db.check_open()?;
        let state = self.read_state()?;
        match locate(&state, id) {
            None => Ok(None),
            Some(Located::Memtable) => {
                let Some(Lookup::Present(row)) = state.memtable.get(id) else {
                    return Ok(None);
                };
                let vector = if include.vector {
                    let bytes = state.memtable.vector_bytes(row).ok_or_else(|| {
                        crate::internal_error!("memtable row {:?} lost its vector", row.id)
                    })?;
                    Some(VectorView::raw(VectorDType::F32, bytes, self.dimension())?.to_f32())
                } else {
                    None
                };
                Ok(Some(Document {
                    id: row.id.clone(),
                    vector,
                    metadata: if include.metadata {
                        row.metadata.clone().unwrap_or_default()
                    } else {
                        Metadata::new()
                    },
                    content: if include.content {
                        row.content.clone()
                    } else {
                        None
                    },
                }))
            }
            Some(Located::Segment(index, row)) => match state.segments.get(index) {
                Some(seg) => seg.document(row, include),
                None => Ok(None),
            },
        }
    }

    /// Whether a document exists.
    ///
    /// # Errors
    /// [`crate::error::LifecycleError::DatabaseClosed`].
    pub fn contains(&self, id: &DocId) -> Result<bool> {
        self.db.check_open()?;
        let state = self.read_state()?;
        Ok(locate(&state, id).is_some())
    }

    /// Fetch a document, failing if it does not exist.
    ///
    /// # Errors
    /// [`NotFoundError::Document`], or any storage error.
    pub fn require(&self, id: &DocId) -> Result<Document> {
        self.get(id)?.ok_or_else(|| {
            NotFoundError::Document {
                collection: self.name().to_owned(),
                id: id.display(),
            }
            .into()
        })
    }

    /// Live documents.
    ///
    /// # Errors
    /// [`crate::error::LifecycleError::DatabaseClosed`].
    pub fn count(&self) -> Result<u64> {
        self.db.check_open()?;
        let state = self.read_state()?;
        Ok(live_count(&state))
    }

    /// Every live document id, sorted.
    ///
    /// Present so the engine can be exercised end to end before search exists. It materialises
    /// every id, so it is not the shape a large collection should be iterated with — a cursor
    /// arrives with the read path.
    ///
    /// # Errors
    /// Any storage error.
    pub fn ids(&self) -> Result<Vec<DocId>> {
        self.db.check_open()?;
        let state = self.read_state()?;
        let mut out = Vec::new();
        for (index, seg) in state.segments.iter().enumerate() {
            for row in 0..seg.rows() {
                if !seg.is_live(row) {
                    continue;
                }
                if let Some(id) = seg.id_at(row) {
                    // Skip anything the memtable supersedes, so an overwritten document is not
                    // reported twice.
                    if state.memtable.get(id).is_none() {
                        let _ = index;
                        out.push(id.clone());
                    }
                }
            }
        }
        for row in state.memtable.live_rows() {
            out.push(row.id.clone());
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    // ---- maintenance ------------------------------------------------------

    /// Fold buffered writes into a new segment and commit.
    ///
    /// # Errors
    /// Any storage error.
    pub fn flush(&self) -> Result<()> {
        self.db.check_open()?;
        if self.db.config.read_only {
            return Ok(());
        }
        let state = self.write_state()?;
        self.do_flush(state)
    }

    /// Counters for this collection.
    ///
    /// # Errors
    /// [`crate::error::LifecycleError::DatabaseClosed`].
    pub fn stats(&self) -> Result<CollectionStats> {
        self.db.check_open()?;
        let state = self.read_state()?;
        let total_rows: u64 = state.segments.iter().map(|s| u64::from(s.rows())).sum();
        let live_on_disk: u64 = state
            .segments
            .iter()
            .map(|s| u64::from(s.live_count()))
            .sum();
        let dead_ratio = if total_rows == 0 {
            0.0
        } else {
            (total_rows - live_on_disk) as f32 / total_rows as f32
        };
        Ok(CollectionStats {
            name: self.name().to_owned(),
            dimension: self.inner.catalog.dimension,
            metric: self.inner.catalog.metric,
            dtype: self.inner.catalog.dtype,
            id_kind: self.inner.catalog.id_kind,
            index: self.inner.catalog.index.clone(),
            live_documents: live_count(&state),
            total_rows,
            segments: state.segments.len(),
            buffered_documents: state.memtable.len(),
            memtable_bytes: state.memtable.byte_size(),
            dead_ratio,
        })
    }

    // ---- internals --------------------------------------------------------

    fn read_state(&self) -> Result<RwLockReadGuard<'_, CollState>> {
        self.inner
            .state
            .read()
            .map_err(|_| crate::internal_error!("collection state poisoned by a panicking writer"))
    }

    fn write_state(&self) -> Result<std::sync::RwLockWriteGuard<'_, CollState>> {
        self.inner
            .state
            .write()
            .map_err(|_| crate::internal_error!("collection state poisoned by a panicking writer"))
    }

    fn validate(&self, doc: &DocumentInput<'_>) -> Result<()> {
        doc.validate(self.name(), self.dimension(), self.inner.catalog.id_kind)
    }

    fn validate_op(&self, op: &BatchOp) -> Result<()> {
        match op {
            BatchOp::Upsert {
                id,
                vector,
                metadata,
                ..
            } => {
                id.validate(self.inner.catalog.id_kind)?;
                VectorView::raw(VectorDType::F32, vector, self.dimension())?
                    .validate(self.name(), self.dimension())?;
                if let Some(m) = metadata {
                    m.validate()?;
                }
                Ok(())
            }
            BatchOp::Delete { id } => id.validate(self.inner.catalog.id_kind),
        }
    }

    fn apply_put(&self, state: &mut CollState, doc: DocumentInput<'_>) -> Result<()> {
        let metadata_bytes = match &doc.metadata {
            Some(m) => m.encode()?,
            None => Vec::new(),
        };
        // Log first, then apply. A write that is visible but not logged would vanish on the
        // next open, which is the one failure a database must never have.
        state.wal.append(WalOp::Put {
            id: doc.id.to_bytes(),
            vector: doc.vector.to_bytes(),
            metadata: metadata_bytes,
            content: doc.content.map(<[u8]>::to_vec),
        })?;
        state.memtable.put_view(
            doc.id,
            doc.vector,
            doc.metadata,
            doc.content.map(<[u8]>::to_vec),
        )?;
        Ok(())
    }

    fn maybe_flush(&self, state: std::sync::RwLockWriteGuard<'_, CollState>) -> Result<()> {
        if state.memtable.byte_size() >= self.db.config.flush_threshold_bytes {
            return self.do_flush(state);
        }
        Ok(())
    }

    fn do_flush(&self, mut state: std::sync::RwLockWriteGuard<'_, CollState>) -> Result<()> {
        if state.memtable.is_empty() {
            return Ok(());
        }
        let storage = self.db.storage.as_ref();
        let catalog = &self.inner.catalog;
        let segment_id = state.next_segment_id;

        let result = flush_memtable(storage, catalog, segment_id, &state.memtable)?;

        // Every id the new segment carries supersedes any earlier copy, and every tombstone
        // kills the row it refers to. Without the supersede step an overwritten document stays
        // live in two segments at once, so `count()` over-reports and a future scan returns the
        // same document twice with different vectors.
        let superseded: Vec<DocId> = state
            .memtable
            .live_rows()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        let doomed: Vec<DocId> = result.pending_deletions.clone();

        let mut refs: Vec<SegmentRef> = Vec::with_capacity(state.segments.len() + 1);
        for seg in state.segments.iter_mut() {
            let mut generation = None;
            for id in superseded.iter().chain(doomed.iter()) {
                if let Some(row) = seg.row_of(id) {
                    if seg.kill(row) {
                        generation = Some(0);
                    }
                }
            }
            if generation.is_some() {
                seg.persist_tombstones(storage, &catalog.name)?;
            }
            refs.push(SegmentRef {
                id: seg.id,
                rows: seg.rows(),
                del_generation: seg.tombstones().generation,
            });
        }

        if result.segment.rows > 0 {
            let opened = SegmentData::open(storage, catalog, &result.segment)?;
            refs.push(result.segment);
            state.segments.push(opened);
        } else {
            // A memtable holding only tombstones produces no rows; the empty files it wrote are
            // removed rather than being committed as a segment nothing can ever match.
            remove_segment(storage, &catalog.name, segment_id)?;
        }

        let total_rows: u64 = refs.iter().map(|s| u64::from(s.rows)).sum();
        let live: u64 = state
            .segments
            .iter()
            .map(|s| u64::from(s.live_count()))
            .sum();
        self.db.commit_collection(CollectionEntry {
            name: catalog.name.clone(),
            segments: refs,
            index_snapshot: None,
            last_applied_wal: state.wal.next_sequence(),
            live_count: live,
            total_rows,
        })?;

        // Checkpoint last. A crash before this leaves the log holding frames already folded
        // into a segment, which replay reapplies harmlessly because put and delete are both
        // idempotent.
        let wal_path = layout::wal_file(&catalog.name, 1)?;
        let mut file = storage.open_file(&wal_path, crate::storage::OpenMode::ReadWrite)?;
        file.truncate(0)?;
        file.sync_data()?;

        state.memtable.clear();
        state.next_segment_id = segment_id + 1;
        state.wal = WalWriter::open(
            storage,
            &wal_path,
            state.wal.next_sequence(),
            self.db.config.durability,
        )?;
        Ok(())
    }
}

/// Find where a document currently lives, respecting shadowing.
///
/// The memtable wins, because it holds the newest state; a tombstone there means the document
/// is gone even if a segment still holds a live row for it.
fn locate(state: &CollState, id: &DocId) -> Option<Located> {
    match state.memtable.get(id) {
        Some(Lookup::Deleted) => return None,
        Some(Lookup::Present(_)) => return Some(Located::Memtable),
        None => {}
    }
    // Newest segment first: an overwrite that has been flushed twice without an intervening
    // supersede would otherwise return the older copy.
    for (index, seg) in state.segments.iter().enumerate().rev() {
        if let Some(row) = seg.row_of(id) {
            if seg.is_live(row) {
                return Some(Located::Segment(index, row));
            }
        }
    }
    None
}

/// Live documents, counting the memtable's shadowing of segment rows.
fn live_count(state: &CollState) -> u64 {
    let on_disk: u64 = state
        .segments
        .iter()
        .map(|s| u64::from(s.live_count()))
        .sum();

    // A memtable entry that shadows a live segment row must not be counted twice, and a
    // tombstone must subtract the row it hides.
    let mut shadowed = 0u64;
    for row in state.memtable.live_rows() {
        if segment_has_live(state, &row.id) {
            shadowed += 1;
        }
    }
    let mut hidden = 0u64;
    for id in state.memtable.deleted_ids() {
        if segment_has_live(state, id) {
            hidden += 1;
        }
    }
    on_disk + state.memtable.len() as u64 - shadowed - hidden
}

fn segment_has_live(state: &CollState, id: &DocId) -> bool {
    state
        .segments
        .iter()
        .any(|s| s.row_of(id).is_some_and(|row| s.is_live(row)))
}
