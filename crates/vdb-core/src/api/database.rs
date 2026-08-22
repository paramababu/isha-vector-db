//! The database handle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use vdb_format::{Catalog, CollectionEntry, Manifest};

use crate::api::collection::{CollInner, Collection};
use crate::api::{
    CollectionSpec, CompactOptions, CompactReport, DatabaseConfig, DatabaseStats, VerifyLevel,
    VerifyReport,
};
use crate::clock::Clock;
use crate::error::{ConflictError, CorruptionError, LifecycleError, NotFoundError, Result};
use crate::index::VectorIndex;
use crate::persistence::segment::{read_catalog, write_catalog};
use crate::persistence::{layout, ManifestStore};
use crate::storage::{FileLock, Storage};

/// Shared state behind every handle. Collections hold this; it does not hold them back, so
/// there is no reference cycle to leak.
#[derive(Debug)]
pub(crate) struct DbInner {
    /// The index every collection searches with.
    pub(crate) index: Arc<dyn VectorIndex>,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: DatabaseConfig,
    pub(crate) manifest: Mutex<ManifestStore>,
    pub(crate) collections: RwLock<HashMap<String, Arc<CollInner>>>,
    /// Held for the database's lifetime. Advisory: it prevents accidents — a second instance of
    /// the same app, a debug tool left open — and is not a security boundary.
    _lock: Mutex<Option<Box<dyn FileLock>>>,
    closed: AtomicBool,
}

impl DbInner {
    pub(crate) fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(LifecycleError::DatabaseClosed.into());
        }
        Ok(())
    }

    pub(crate) fn check_writable(&self, operation: &'static str) -> Result<()> {
        self.check_open()?;
        if self.config.read_only {
            return Err(LifecycleError::ReadOnly { operation }.into());
        }
        Ok(())
    }

    /// Rewrite one collection's manifest entry and commit.
    pub(crate) fn commit_collection(&self, entry: CollectionEntry) -> Result<()> {
        let mut store = self.manifest.lock().map_err(|_| {
            crate::internal_error!("the manifest lock was poisoned by a panicking writer")
        })?;
        let mut manifest = store.current().clone();
        match manifest
            .collections
            .iter_mut()
            .find(|c| c.name == entry.name)
        {
            Some(existing) => *existing = entry,
            None => manifest.collections.push(entry),
        }
        store.commit(self.storage.as_ref(), manifest, self.clock.now_ms())
    }

    pub(crate) fn manifest_snapshot(&self) -> Result<Manifest> {
        let store = self.manifest.lock().map_err(|_| {
            crate::internal_error!("the manifest lock was poisoned by a panicking writer")
        })?;
        Ok(store.current().clone())
    }
}

/// An open database.
///
/// Not `Clone`: a database has one owner, and [`Database::close`] consumes it. Share it with
/// `Arc<Database>` when several parts of an application need it — [`Collection`] handles are
/// cheap and independently shareable.
#[derive(Debug)]
pub struct Database {
    inner: Arc<DbInner>,
}

impl Database {
    /// Open, or create, a database on the given storage.
    ///
    /// This is the only place platform knowledge enters the engine: everything the database
    /// knows about the machine it is running on arrives through `storage` and `clock`.
    ///
    /// # Errors
    /// [`LifecycleError::DatabaseNotFound`] when nothing exists and `create_if_missing` is
    /// false, [`LifecycleError::DatabaseAlreadyOpen`] when another handle holds the lock,
    /// [`crate::error::CorruptionError`] when the manifest or a segment is damaged, or any
    /// storage error.
    pub fn open(
        storage: Arc<dyn Storage>,
        config: DatabaseConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        Self::open_with_index(
            storage,
            config,
            clock,
            Arc::new(crate::index::ExactScan::new()),
        )
    }

    /// Open with a specific index implementation.
    ///
    /// `vdb-core` cannot depend on an index crate — that inversion is what lets a mobile build
    /// ship only the indexes it needs, and it is why the core can forbid `unsafe` at all. So the
    /// accelerated scan in `vdb-index-flat`, whose SIMD kernels need `unsafe`, arrives this way:
    ///
    /// ```ignore
    /// Database::open_with_index(storage, config, clock, Arc::new(vdb_index_flat::FlatIndex::new()))
    /// ```
    ///
    /// [`Database::open`] uses the reference scan in the core, which is correct everywhere and
    /// slower. Every shipped SDK passes the accelerated one; an embedder auditing the core in
    /// isolation gets the safe one by default.
    ///
    /// # Errors
    /// As [`Database::open`].
    pub fn open_with_index(
        storage: Arc<dyn Storage>,
        config: DatabaseConfig,
        clock: Arc<dyn Clock>,
        index: Arc<dyn VectorIndex>,
    ) -> Result<Self> {
        config.validate()?;

        // Read-only handles take no lock, so a second process can inspect a database while an
        // application has it open. That is genuinely useful for debugging and cannot corrupt
        // anything, because a read-only handle never writes.
        let lock = if config.read_only {
            None
        } else {
            match storage.try_lock(&layout::lock()?) {
                Ok(l) => Some(l),
                Err(e) => {
                    return Err(LifecycleError::DatabaseAlreadyOpen {
                        path: storage.describe(),
                        holder: Some(e.to_string()),
                    }
                    .into())
                }
            }
        };

        let store = match ManifestStore::load(storage.as_ref())? {
            Some(store) => store,
            None => {
                if !config.create_if_missing {
                    return Err(LifecycleError::DatabaseNotFound {
                        path: storage.describe(),
                    }
                    .into());
                }
                // Both manifest slots are gone but collections are on disk: that is a damaged
                // database, not an empty directory. Creating a fresh manifest here would
                // present the user with an empty database and orphan everything they had, which
                // is the worst possible response to losing a 200-byte file.
                let collections = layout::collections_dir()?;
                if storage.exists(&collections)? && !storage.list_dir(&collections)?.is_empty() {
                    let scan = ManifestStore::scan(storage.as_ref())?;
                    return Err(CorruptionError::NoValidManifest {
                        path: collections,
                        slot_a: scan.a.describe(),
                        slot_b: scan.b.describe(),
                    }
                    .into());
                }
                storage.create_dir_all(&collections)?;
                let uuid = new_uuid(clock.as_ref());
                ManifestStore::create(storage.as_ref(), uuid, clock.now_ms())?
            }
        };

        let inner = Arc::new(DbInner {
            index,
            storage: Arc::clone(&storage),
            clock: Arc::clone(&clock),
            config,
            manifest: Mutex::new(store),
            collections: RwLock::new(HashMap::new()),
            _lock: Mutex::new(lock),
            closed: AtomicBool::new(false),
        });

        // Open every collection the manifest names. A collection that fails to open fails the
        // whole open: presenting a database with one collection silently missing would be worse
        // than refusing, because the application would carry on and write into the gap.
        let manifest = inner.manifest_snapshot()?;
        for entry in &manifest.collections {
            let catalog = read_catalog(inner.storage.as_ref(), &entry.name)?;
            let coll = CollInner::open(&inner, catalog, entry)?;
            inner
                .collections
                .write()
                .map_err(|_| crate::internal_error!("collection registry poisoned"))?
                .insert(entry.name.clone(), Arc::new(coll));
        }

        Ok(Self { inner })
    }

    /// Whether the handle is still usable.
    pub fn is_open(&self) -> bool {
        self.inner.check_open().is_ok()
    }

    /// Flush every collection and release the lock.
    ///
    /// Consumes the handle, so using a closed database is a compile error rather than a runtime
    /// one. Outstanding [`Collection`] handles remain valid objects but every operation on them
    /// returns [`LifecycleError::DatabaseClosed`].
    ///
    /// # Errors
    /// Any storage error during the final flush. The database is marked closed regardless, so a
    /// failed close cannot leave a handle in a half-open state.
    pub fn close(self) -> Result<()> {
        let result = if self.inner.config.read_only {
            Ok(())
        } else {
            self.flush()
        };
        self.inner.closed.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner._lock.lock() {
            *guard = None;
        }
        result
    }

    /// Create a collection.
    ///
    /// # Errors
    /// [`ConflictError::CollectionExists`], [`crate::error::ValidationError`] for a bad
    /// specification, or any storage error.
    pub fn create_collection(&self, spec: CollectionSpec) -> Result<Collection> {
        self.inner.check_writable("create a collection")?;
        spec.validate()?;

        if self.collection_exists(&spec.name)? {
            return Err(ConflictError::CollectionExists { name: spec.name }.into());
        }

        let catalog = Catalog {
            name: spec.name.clone(),
            dimension: spec.dimension,
            metric: spec.metric,
            dtype: spec.dtype,
            id_kind: spec.id_kind,
            index: spec.index,
            created_at_ms: self.inner.clock.now_ms(),
        };
        write_catalog(self.inner.storage.as_ref(), &catalog)?;
        self.inner
            .storage
            .create_dir_all(&layout::wal_dir(&spec.name)?)?;

        let entry = CollectionEntry {
            name: spec.name.clone(),
            segments: vec![],
            index_snapshot: None,
            last_applied_wal: 0,
            live_count: 0,
            total_rows: 0,
        };
        self.inner.commit_collection(entry.clone())?;

        let coll = Arc::new(CollInner::open(&self.inner, catalog, &entry)?);
        self.inner
            .collections
            .write()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?
            .insert(spec.name.clone(), Arc::clone(&coll));
        Ok(Collection::new(Arc::clone(&self.inner), coll))
    }

    /// Open an existing collection.
    ///
    /// # Errors
    /// [`NotFoundError::Collection`] if there is no such collection.
    pub fn open_collection(&self, name: &str) -> Result<Collection> {
        self.inner.check_open()?;
        let registry = self
            .inner
            .collections
            .read()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?;
        match registry.get(name) {
            Some(coll) => Ok(Collection::new(Arc::clone(&self.inner), Arc::clone(coll))),
            None => Err(NotFoundError::Collection {
                name: name.to_owned(),
            }
            .into()),
        }
    }

    /// Open a collection, creating it if it does not exist.
    ///
    /// If it does exist, its specification must match: silently returning a collection with a
    /// different dimension or metric than the caller asked for would produce results that look
    /// plausible and are wrong.
    ///
    /// # Errors
    /// [`ConflictError::CollectionExists`] if an existing collection's shape differs from the
    /// specification, or any other error from creating or opening.
    pub fn get_or_create_collection(&self, spec: CollectionSpec) -> Result<Collection> {
        if self.collection_exists(&spec.name)? {
            let existing = self.open_collection(&spec.name)?;
            let cat = existing.catalog();
            if cat.dimension != spec.dimension
                || cat.metric != spec.metric
                || cat.dtype != spec.dtype
                || cat.id_kind != spec.id_kind
            {
                return Err(ConflictError::CollectionExists { name: spec.name }.into());
            }
            return Ok(existing);
        }
        self.create_collection(spec)
    }

    /// Whether a collection exists.
    ///
    /// # Errors
    /// [`LifecycleError::DatabaseClosed`].
    pub fn collection_exists(&self, name: &str) -> Result<bool> {
        self.inner.check_open()?;
        Ok(self
            .inner
            .collections
            .read()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?
            .contains_key(name))
    }

    /// Drop a collection and everything in it. Irreversible.
    ///
    /// # Errors
    /// [`NotFoundError::Collection`], or any storage error.
    pub fn drop_collection(&self, name: &str) -> Result<()> {
        self.inner.check_writable("drop a collection")?;
        if !self.collection_exists(name)? {
            return Err(NotFoundError::Collection {
                name: name.to_owned(),
            }
            .into());
        }

        // Manifest first, files second. A crash in between leaves an unreferenced directory,
        // which is recoverable waste; the other order would leave the manifest pointing at
        // files that no longer exist, which is corruption.
        let mut manifest = self.inner.manifest_snapshot()?;
        manifest.collections.retain(|c| c.name != name);
        {
            let mut store = self.inner.manifest.lock().map_err(|_| {
                crate::internal_error!("the manifest lock was poisoned by a panicking writer")
            })?;
            store.commit(
                self.inner.storage.as_ref(),
                manifest,
                self.inner.clock.now_ms(),
            )?;
        }
        self.inner
            .collections
            .write()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?
            .remove(name);

        let dir = layout::collection_dir(name)?;
        if self.inner.storage.exists(&dir)? {
            self.inner.storage.remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Describe every collection.
    ///
    /// # Errors
    /// [`LifecycleError::DatabaseClosed`].
    pub fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        self.inner.check_open()?;
        let registry = self
            .inner
            .collections
            .read()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?;
        let mut out: Vec<CollectionInfo> = registry
            .values()
            .map(|c| CollectionInfo {
                name: c.catalog.name.clone(),
                dimension: c.catalog.dimension,
                metric: c.catalog.metric,
            })
            .collect();
        // Sorted, so listing is deterministic rather than dependent on hash order.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Flush every collection's buffered writes into segments and commit.
    ///
    /// # Errors
    /// Any storage error.
    pub fn flush(&self) -> Result<()> {
        self.inner.check_open()?;
        if self.inner.config.read_only {
            return Ok(());
        }
        let names: Vec<String> = self
            .inner
            .collections
            .read()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?
            .keys()
            .cloned()
            .collect();
        for name in names {
            self.open_collection(&name)?.flush()?;
        }
        Ok(())
    }

    /// Check the database's integrity.
    ///
    /// Reports rather than repairs. Deciding what to discard is not a choice a library should
    /// make silently on someone's behalf; a report is what makes the choice possible.
    ///
    /// # Errors
    /// Any storage error that prevents verification from running at all. Damage found *by*
    /// verification appears in the report — stopping at the first fault would leave the caller
    /// unable to tell a single bad block from a wholly lost database.
    pub fn verify(&self, level: VerifyLevel) -> Result<VerifyReport> {
        self.inner.check_open()?;
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut collections = Vec::new();

        let manifest = self.inner.manifest_snapshot()?;
        for entry in &manifest.collections {
            let coll = self.open_collection(&entry.name)?;
            let result = coll.verify(level, &mut errors, &mut warnings)?;

            // The manifest's counters are a cache of what the segments actually hold. A
            // disagreement is not itself data loss, but it means one of the two is stale, and
            // every decision made from the cheap number — whether to compact, what to report as
            // a document count — is then wrong.
            if entry.live_count != result.live_documents {
                warnings.push(format!(
                    "{}: manifest says {} live documents, segments hold {}",
                    entry.name, entry.live_count, result.live_documents
                ));
            }
            if entry.total_rows != result.total_rows {
                errors.push(format!(
                    "{}: manifest says {} rows, segments hold {}",
                    entry.name, entry.total_rows, result.total_rows
                ));
            }
            collections.push(result);
        }

        // Directories under `collections/` that the manifest does not name. A dropped collection
        // whose deletion was interrupted leaves one, and so does a manual copy gone wrong.
        let dir = layout::collections_dir()?;
        if self.inner.storage.exists(&dir)? {
            let named: Vec<&str> = manifest
                .collections
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            for entry in self.inner.storage.list_dir(&dir)? {
                if !named.contains(&entry.name.as_str()) {
                    warnings.push(format!(
                        "collections/{} is on disk but named by no manifest",
                        entry.name
                    ));
                }
            }
        }

        Ok(VerifyReport {
            level,
            collections,
            errors,
            warnings,
        })
    }

    /// Compact every collection.
    ///
    /// # Errors
    /// Any storage error.
    pub fn compact(&self, options: CompactOptions) -> Result<CompactReport> {
        self.inner.check_writable("compact")?;
        let names: Vec<String> = self
            .inner
            .collections
            .read()
            .map_err(|_| crate::internal_error!("collection registry poisoned"))?
            .keys()
            .cloned()
            .collect();
        let mut total = CompactReport::default();
        for name in names {
            let report = self.open_collection(&name)?.compact(options)?;
            total.segments_rewritten += report.segments_rewritten;
            total.segments_created += report.segments_created;
            total.rows_reclaimed += report.rows_reclaimed;
        }
        Ok(total)
    }

    /// Database-wide counters.
    ///
    /// # Errors
    /// [`LifecycleError::DatabaseClosed`].
    pub fn stats(&self) -> Result<DatabaseStats> {
        self.inner.check_open()?;
        let manifest = self.inner.manifest_snapshot()?;
        let mut live = 0u64;
        let mut total = 0u64;
        let names: Vec<String> = {
            let registry = self
                .inner
                .collections
                .read()
                .map_err(|_| crate::internal_error!("collection registry poisoned"))?;
            registry.keys().cloned().collect()
        };
        for name in &names {
            let stats = self.open_collection(name)?.stats()?;
            live += stats.live_documents;
            total += stats.total_rows;
        }
        Ok(DatabaseStats {
            format_version: vdb_format::FORMAT_VERSION,
            manifest_sequence: manifest.sequence,
            collections: names.len(),
            live_documents: live,
            total_rows: total,
            read_only: self.inner.config.read_only,
            durable_sync: self.inner.storage.capabilities().durable_sync,
        })
    }
}

/// A collection, as described by [`Database::list_collections`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectionInfo {
    /// The collection's name.
    pub name: String,
    /// Its vector dimension.
    pub dimension: u32,
    /// Its similarity metric.
    pub metric: vdb_format::Metric,
}

/// Build a database identifier without a random source.
///
/// The core has no entropy: it cannot read `/dev/urandom` any more than it can open a file. The
/// uuid exists to detect a file copied between two databases, not to be unguessable, so mixing
/// the clock with a process-local counter is sufficient. If a genuine random source is wanted
/// later, it arrives the same way everything else does — injected.
fn new_uuid(clock: &dyn Clock) -> [u8; 16] {
    use core::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = clock.now_ms();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    // A cheap mix, so two databases created in the same millisecond still differ.
    let mixed = now
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(31)
        .wrapping_add(seq.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&now.to_le_bytes());
    out[8..].copy_from_slice(&mixed.to_le_bytes());
    out
}
