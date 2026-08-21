# 4. Core Interfaces

These signatures are the contract multiple developers implement against. Types are illustrative
Rust; the intent is the boundary shape, not the final token-for-token source.

## 4.1 Public API

```rust
pub struct Database { /* Arc<DbInner> */ }
pub struct Collection { /* Arc<CollInner> */ }
pub struct Snapshot { /* immutable, cheap to clone, pins segments */ }

impl Database {
    /// The only place platform knowledge enters the engine.
    pub fn open(config: DatabaseConfig, storage: Arc<dyn Storage>) -> Result<Database>;
    pub fn close(self) -> Result<()>;            // consuming: close-after-use is a type error
    pub fn is_open(&self) -> bool;

    pub fn create_collection(&self, spec: CollectionSpec) -> Result<Collection>;
    pub fn open_collection(&self, name: &str) -> Result<Collection>;
    pub fn get_or_create_collection(&self, spec: CollectionSpec) -> Result<Collection>;
    pub fn drop_collection(&self, name: &str) -> Result<()>;
    pub fn list_collections(&self) -> Result<Vec<CollectionInfo>>;

    pub fn stats(&self) -> Result<DatabaseStats>;
    pub fn flush(&self) -> Result<()>;           // durable checkpoint
    pub fn verify(&self, level: VerifyLevel) -> Result<VerifyReport>;
    pub fn format_version(&self) -> FormatVersion;
}

pub struct DatabaseConfig {
    pub path: DbPath,                 // logical path; interpreted by the Storage impl
    pub create_if_missing: bool,      // default true
    pub read_only: bool,
    pub durability: Durability,       // Full | Batch | Relaxed  (see §5.7)
    pub max_open_segments: usize,
    pub cache: CacheConfig,
    pub allow_format_migration: bool, // default false: never migrate without being asked
}

pub struct CollectionSpec {
    pub name: String,                 // [a-zA-Z0-9_-]{1,64}, validated: no path traversal
    pub dimension: u32,               // 1..=65_536, immutable after creation
    pub metric: Metric,               // immutable after creation
    pub dtype: VectorDType,           // v1: F32 only; enum exists so v2 adds variants additively
    pub index: IndexSpec,             // Flat { } in v1
    pub id_kind: IdKind,              // Str { max_len } | U64
}
```

`dimension` and `metric` are fixed at creation. Making them per-document would make every index
implementation and every kernel branch on them, for a use case (mixing embedding models in one
collection) that is better served by two collections.

```rust
impl Collection {
    // ---- writes (single-writer, serialized) ----
    pub fn insert(&self, doc: DocumentInput<'_>) -> Result<()>;          // errors on duplicate id
    pub fn upsert(&self, doc: DocumentInput<'_>) -> Result<UpsertOutcome>;
    pub fn update_vector(&self, id: &DocId, v: VectorView<'_>) -> Result<()>;
    pub fn update_metadata(&self, id: &DocId, patch: MetadataPatch) -> Result<()>;
    pub fn delete(&self, id: &DocId) -> Result<bool>;                    // false = not found
    pub fn write_batch(&self, batch: WriteBatch) -> Result<BatchReport>; // atomic, all-or-nothing

    // ---- reads (lock-free, snapshot-isolated) ----
    pub fn snapshot(&self) -> Snapshot;
    pub fn get(&self, id: &DocId) -> Result<Option<Document>>;
    pub fn get_many(&self, ids: &[DocId]) -> Result<Vec<Option<Document>>>;
    pub fn contains(&self, id: &DocId) -> Result<bool>;
    pub fn count(&self) -> Result<u64>;                                  // live documents
    pub fn scan(&self, opts: ScanOptions) -> Result<Cursor>;

    // ---- search ----
    pub fn search(&self, req: &SearchRequest<'_>) -> Result<SearchResponse>;

    // ---- maintenance ----
    pub fn rebuild_index(&self, spec: Option<IndexSpec>) -> Result<()>;
    pub fn compact(&self, opts: CompactOptions) -> Result<CompactReport>;
    pub fn stats(&self) -> Result<CollectionStats>;
}
```

Every read method also exists in a `*_at(&self, snap: &Snapshot, ...)` form; the plain form takes
an implicit fresh snapshot. Explicit snapshots are how a caller gets a consistent multi-query view.

## 4.2 Data model

```rust
pub enum DocId { Str(SmallStr), U64(u64) }        // external, user-supplied, unique per collection
pub struct RowId(u64);                            // internal, dense: (segment_id << 32) | row_index

pub struct DocumentInput<'a> {
    pub id: DocId,
    pub vector: VectorView<'a>,                   // borrowed: no copy until it hits the WAL
    pub metadata: Option<Metadata>,
    pub content: Option<&'a [u8]>,                // optional opaque payload (e.g. source text)
}

pub struct Document {
    pub id: DocId,
    pub vector: Option<Vec<f32>>,                 // omitted unless requested (it is the big field)
    pub metadata: Metadata,
    pub content: Option<Vec<u8>>,
}

pub enum VectorDType { F32 }                      // F16, I8, Binary, Sparse reserved for v2+
pub struct VectorView<'a> { pub dtype: VectorDType, pub data: &'a [u8], pub dim: u32 }
```

`VectorView` borrows raw bytes rather than taking `&[f32]` so that future dtypes are additive and
so bindings can pass an `ArrayBuffer`/`Float32List`/`ByteBuffer` with zero copies. A typed
constructor `VectorView::f32(&[f32])` keeps the common case ergonomic.

```rust
pub enum Value {
    Null, Bool(bool), I64(i64), F64(f64),
    Str(String), Bytes(Vec<u8>),
    Array(Vec<Value>), Map(BTreeMap<String, Value>),
}
pub struct Metadata(BTreeMap<String, Value>);     // BTreeMap: deterministic iteration + encoding
```

`BTreeMap` not `HashMap`: the encoded bytes must be a deterministic function of the logical value,
otherwise checksums and golden fixtures become flaky and dedup/compaction becomes unverifiable.

## 4.3 Search

```rust
pub struct SearchRequest<'a> {
    pub vector: VectorView<'a>,
    pub top_k: usize,                       // 1..=validation::MAX_TOP_K
    pub metric: Option<Metric>,             // None = collection default; overriding costs a rescan
    pub filter: Option<&'a Filter>,
    pub min_score: Option<f32>,             // inclusive; in score space (higher = better)
    pub include: Include,                   // { vector: bool, metadata: bool, content: bool }
    pub params: SearchParams,               // per-index knobs (ef_search, nprobe); ignored by Flat
}

pub struct SearchResponse {
    pub hits: Vec<Hit>,                     // sorted: score desc, then DocId asc (stable tie-break)
    pub stats: SearchStats,                 // scanned, filtered_out, index_kind, exact: bool
}

pub struct Hit {
    pub id: DocId,
    pub score: f32,                         // ALWAYS higher-is-better, whatever the metric
    pub distance: Option<f32>,              // metric-native distance when one is defined
    pub document: Option<Document>,
}

pub enum Metric { Cosine, L2, Dot }
```

### The scoring contract (write this in the API docs, it prevents a whole class of bug reports)

| Metric | `score` (ranked, thresholded on) | `distance` |
|---|---|---|
| `Cosine` | cosine similarity, `[-1, 1]` | `1 - similarity`, `[0, 2]` |
| `Dot` | dot product, unbounded | `None` |
| `L2` | `-squared_l2` | `sqrt(squared_l2)` (true Euclidean distance) |

One rule — *`score` is always higher-is-better* — means `top_k`, `min_score`, heap ordering and
tie-breaking have exactly one implementation, and no index has to know which metric inverts.
Squared L2 is used internally (no `sqrt` in the inner loop); the `sqrt` happens once per returned
hit.

**Tie-breaking is part of the contract:** equal scores are ordered by ascending `DocId`. Without
this, "deterministic behaviour" is unachievable, because heap order depends on insertion order.

### Filters

```rust
pub enum Filter {
    And(Vec<Filter>), Or(Vec<Filter>), Not(Box<Filter>),
    Eq(Field, Value), Ne(Field, Value),
    Gt(Field, Value), Gte(Field, Value), Lt(Field, Value), Lte(Field, Value),
    In(Field, Vec<Value>), Nin(Field, Vec<Value>),
    Exists(Field), IsNull(Field),
    StartsWith(Field, String), Contains(Field, Value),  // Contains: array membership
}
pub struct Field(String);   // dotted path: "user.plan" descends into Map values
```

Type-coercion rules are explicit and total (documented in `docs/api/filters.md`): comparisons
between different types are `false`, never an error; `I64`/`F64` compare numerically across the
boundary; missing fields are `Null` for `Eq(Null)`/`IsNull` and absent for `Exists`. A filter that
references a field no document has is legal and matches nothing. Filters never error at runtime;
they are validated once at construction (depth ≤ 32, ≤ 256 nodes) and then total.

## 4.4 `VectorIndex`

```rust
pub trait VectorIndex: Send + Sync {
    fn kind(&self) -> IndexKind;
    fn metric(&self) -> Metric;
    fn dimension(&self) -> u32;
    fn len(&self) -> usize;
    fn is_exact(&self) -> bool;                 // Flat = true; ANN = false → surfaced in SearchStats

    fn add(&mut self, row: RowId, v: &[f32]) -> Result<()>;
    fn remove(&mut self, row: RowId) -> Result<()>;
    fn search(&self, ctx: &SearchCtx<'_>, out: &mut TopK) -> Result<()>;

    fn build(&mut self, src: &dyn VectorSource, opts: &BuildOptions) -> Result<()>;
    fn save(&self, w: &mut dyn BlockWriter) -> Result<IndexManifestEntry>;
    fn load(r: &dyn BlockReader, e: &IndexManifestEntry) -> Result<Self> where Self: Sized;
    fn stats(&self) -> IndexStats;
}

pub struct SearchCtx<'a> {
    pub query: &'a [f32],
    pub top_k: usize,
    pub live: &'a LiveSet,                      // tombstone bitmap; index must respect it
    pub filter: Option<&'a CompiledFilter<'a>>, // `fn(RowId) -> bool` + optional prebuilt bitmap
    pub params: &'a SearchParams,
    pub budget: &'a Budget,                     // cooperative cancellation + max scanned rows
}

pub trait VectorSource: Send + Sync {           // how build() streams data without owning storage
    fn len(&self) -> usize;
    fn dimension(&self) -> u32;
    fn for_each(&self, f: &mut dyn FnMut(RowId, &[f32]) -> Result<()>) -> Result<()>;
}
```

Notes on the shape:

- `load` carries `where Self: Sized`, so the trait stays object-safe while still having a
  constructor-like method. Concrete loading goes through `IndexRegistry`, which maps
  `IndexKind` → `fn(&dyn BlockReader, &IndexManifestEntry) -> Result<Box<dyn VectorIndex>>`.
  That registry is also the extension point for third-party indexes.
- `save`/`load` take `BlockWriter`/`BlockReader`, not files. An index never learns what a path is.
- `Budget` gives cooperative cancellation (checked every N candidates), which is what makes a
  long search interruptible from a mobile UI without threads inside the core.
- `LiveSet` is passed in rather than maintained inside the index, so deletes are O(1) everywhere
  and only compaction pays the cost.

## 4.5 `Storage`

```rust
pub trait Storage: Send + Sync {
    fn capabilities(&self) -> StorageCapabilities;
    fn open_file(&self, path: &DbPath, mode: OpenMode) -> Result<Box<dyn File>>;
    fn remove_file(&self, path: &DbPath) -> Result<()>;
    fn rename(&self, from: &DbPath, to: &DbPath) -> Result<()>;   // only if caps.atomic_rename
    fn create_dir_all(&self, path: &DbPath) -> Result<()>;
    fn remove_dir_all(&self, path: &DbPath) -> Result<()>;
    fn list_dir(&self, path: &DbPath) -> Result<Vec<DirEntry>>;
    fn metadata(&self, path: &DbPath) -> Result<Option<FileMeta>>;
    fn sync_dir(&self, path: &DbPath) -> Result<()>;              // no-op where meaningless
    fn try_lock(&self, path: &DbPath) -> Result<Box<dyn FileLock>>;
}

pub trait File: Send + Sync {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;
    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()>;
    fn append(&mut self, buf: &[u8]) -> Result<u64>;              // returns start offset
    fn truncate(&mut self, len: u64) -> Result<()>;
    fn len(&self) -> Result<u64>;
    fn sync_data(&mut self) -> Result<()>;                        // must be a real durability point
    fn map_readonly(&self) -> Result<Option<Box<dyn MappedRegion>>>; // None if unsupported
}

pub struct StorageCapabilities {
    pub atomic_rename: bool,
    pub mmap: bool,
    pub durable_sync: bool,      // false ⇒ engine downgrades durability and says so in stats
    pub sparse_files: bool,
    pub max_file_size: Option<u64>,
    pub prefers_few_large_files: bool,   // true for OPFS/IndexedDB
}
```

`read_at`/`write_at`/`append` rather than a `Seek`+`Read`+`Write` cursor: positional I/O is the
only shape that is safe to call from multiple reader threads on one handle, and it maps cleanly
onto `pread`/`pwrite`, `FileHandle.read(at:)` and OPFS `FileSystemSyncAccessHandle.read(buf, {at})`.

`capabilities()` exists because the browser cannot do everything a POSIX filesystem can. The engine
adapts its commit protocol to the declared capabilities rather than each storage backend faking
POSIX semantics badly. Any capability an implementation reports must be honestly implemented —
`vdb-testkit` ships a conformance suite (§8.4) that every backend must pass.
