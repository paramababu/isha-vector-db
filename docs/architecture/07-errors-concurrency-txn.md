# 7. Error Model, Concurrency, Transactions

## 7.1 Error taxonomy

One error type crosses the public API: `DbError`. It is a tree, every variant carries structured
context, and every variant maps to a stable numeric `ErrorCode` that survives the FFI boundary and
is part of the semver contract.

```rust
#[non_exhaustive]
pub enum DbError {
    Config(ConfigError),
    Lifecycle(LifecycleError),
    NotFound(NotFoundError),
    Conflict(ConflictError),
    Validation(ValidationError),
    Index(IndexError),
    Storage(StorageError),
    Serialization(SerializationError),
    Corruption(CorruptionError),
    Transaction(TransactionError),
    Unsupported(UnsupportedError),
    ResourceExhausted(ResourceError),
    Cancelled,
    Internal(InternalError),   // a bug in us; always includes a location
}
```

Representative leaves, showing the level of context expected:

```rust
pub enum ValidationError {
    InvalidVectorDimension { collection: String, expected: u32, actual: u32 },
    InvalidVectorData { reason: NonFiniteKind, index: usize },   // NaN/Inf at position i
    InvalidDocumentId { reason: IdRejection, len: usize, max: usize },
    InvalidCollectionName { name: String, reason: NameRejection },
    MetadataTooLarge { field: String, size: usize, max: usize },
    MetadataDepthExceeded { depth: usize, max: usize },
    TopKOutOfRange { requested: usize, max: usize },
    FilterTooComplex { nodes: usize, max: usize },
}

pub enum CorruptionError {
    BadMagic { path: DbPath, expected: [u8;8], found: [u8;8] },
    ChecksumMismatch { path: DbPath, offset: u64, expected: u32, found: u32 },
    TruncatedFile { path: DbPath, expected_len: u64, actual_len: u64 },
    NoValidManifest { path: DbPath, slot_a: SlotState, slot_b: SlotState },
    MissingSegment { collection: String, segment: u64 },
    UnsupportedFormatVersion { found: u16, min_readable: u16, current: u16 },
    InconsistentIndex { collection: String, detail: String },   // recoverable: rebuild
}
```

Rules, all enforceable in review:

1. **No stringly-typed errors.** `DbError::Internal { msg }` is the only variant carrying free text,
   and reaching it is a bug to be fixed, not a control-flow tool.
2. **Every error names the thing.** Which collection, which id, which path, which offset, expected
   vs actual. An error a developer cannot act on without a debugger is a defective error.
3. **`#[non_exhaustive]` everywhere**, so adding variants is not a breaking change.
4. **Recoverability is explicit**: `fn recoverability(&self) -> Recoverability { Retryable |
   Fatal | NeedsRepair | UserError }`. SDKs use this to decide what to surface and whether a retry
   is meaningful, instead of each SDK re-deriving the classification by matching on codes.
5. **Never swallow.** No `let _ = ...` on a fallible call outside tests; clippy-enforced. Errors
   during cleanup that cannot be returned (a failed close inside a drop path) go to an injected
   `Observer` hook, never to `eprintln!`.
6. **Panics are bugs, not errors.** Predictable failure is `Result`. A panic means an invariant
   broke, and at the FFI boundary it is caught and converted to `VDB_ERR_INTERNAL` so the host app
   does not die.

### Stable error codes across the ABI

`ErrorCode` is a `u32` with reserved ranges (`1xxx` config, `2xxx` lifecycle, `3xxx` not-found,
`4xxx` validation, `5xxx` storage, `6xxx` corruption, `7xxx` index, `8xxx` transaction,
`9xxx` internal). Codes are **append-only and never reused**; the table lives in
`docs/api/error-codes.md` and is generated from the source so it cannot drift. Every SDK maps codes
to idiomatic errors (`VdbError` subclasses in TS, sealed classes in Kotlin, Swift `Error` enum,
Dart exceptions), preserving code, message, and a structured field map.

## 7.2 Concurrency model

**One writer, many readers, single process.**

- Writes take a `Mutex` inside the collection. A write is short (validate → WAL append → memtable
  update), so contention is bounded and the model stays trivially reasonable about.
- Reads take **no locks**. `Collection::snapshot()` clones an `Arc<SnapshotState>` holding the
  manifest view, the live-set bitmap and segment handles. Segments are immutable, so a reader
  scanning a segment cannot race a writer. Compaction produces new segments and drops old ones only
  when the last snapshot referencing them is released (`Arc` refcount = epoch reclamation, for free).
- The memtable is the one mutable structure a reader touches. It is behind an `arc-swap`-style
  pointer: the writer publishes a new immutable memtable snapshot on each commit; readers take the
  current pointer. This costs one atomic load per read and removes reader/writer contention.
- Cross-process: an advisory `LOCK` file. Honest framing — this prevents accidents (two instances
  of the same app, a debug tool left open), not adversaries, and NFS/some Android storage volumes
  do not implement locks reliably. Documented as such.
- Cancellation is cooperative via `Budget`, checked on a fixed candidate stride. No thread is ever
  killed; there is no thread to kill.

Verification: `loom` on the memtable publish/consume protocol, plus a stress test with N reader
threads and one writer asserting snapshot isolation (every read sees a state that existed at some
commit point — never a mix).

## 7.3 Snapshot isolation semantics

A `Snapshot` sees the collection as of the commit that was current when it was taken. Concurrent
writes are invisible to it. Snapshots are cheap (an `Arc` clone) but pin segment files, so an app
holding one for hours blocks space reclamation — documented, and `CollectionStats` reports
`pinned_segments` so it is diagnosable rather than mysterious.

## 7.4 Transactions: what v1 actually gives you

v1 ships **atomic batches**, not interactive transactions:

```rust
let mut b = WriteBatch::new();
b.upsert(doc1); b.upsert(doc2); b.delete(&id3);
collection.write_batch(b)?;   // all applied, or none
```

The batch is written as one WAL frame group with a single commit record. Recovery applies a group
only if the commit record is present and its CRC validates, so a crash mid-batch rolls the whole
group back.

Why not interactive `begin/commit/rollback` in v1: it requires either write-locking for the
transaction's lifetime (an app that forgets to commit deadlocks its own database — a terrible
failure mode for an embedded library) or full MVCC with a version chain per row and conflict
detection. Both are substantial, and neither is needed by the target workload, which is
"batch-ingest embeddings, then query". Atomic batches cover it.

The roadmap keeps the door open: `Database::transaction()` returning a `Transaction` handle with
single-collection scope, optimistic conflict detection, and a mandatory timeout. The WAL frame
format already reserves a transaction-id field, so adding it is not a format break.

## 7.5 Resource limits (`validation::limits`)

Every limit lives in one table, is documented, is part of the public contract, and each has a test
asserting the boundary and the error:

| Limit | Default |
|---|---|
| `MAX_DIMENSION` | 65,536 |
| `MAX_DOC_ID_LEN` | 512 bytes |
| `MAX_COLLECTION_NAME_LEN` | 64 (charset `[A-Za-z0-9_-]`, rejects `.`/`..`/separators) |
| `MAX_METADATA_BYTES` | 64 KiB per document |
| `MAX_METADATA_DEPTH` | 16 |
| `MAX_CONTENT_BYTES` | 1 MiB |
| `MAX_TOP_K` | 10,000 |
| `MAX_BATCH_OPS` | 100,000 |
| `MAX_FILTER_NODES` | 256 (depth 32) |

Collection-name validation is a **security** control, not just hygiene: names become path
components, so `../../etc` must be rejected in `validation`, before any storage backend sees it.
