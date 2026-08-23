# isha-vector-db in Rust

The engine's own language. No FFI, no marshalling, and the full API rather than the subset the C
ABI exposes.

## Install

```toml
[dependencies]
isha-vector-db-core = "0.0.1"
isha-vector-db-storage-os = "0.0.1"     # the filesystem backend
isha-vector-db-index-flat = "0.0.1"     # SIMD exact search
```

Rust 1.78 or newer.

`isha-vector-db-core` performs no I/O and knows nothing about any platform, so it cannot open a file on its
own — you choose a storage backend and hand it in. That is what keeps the engine portable, and it
is why there are three crates rather than one.

## Your first database

```rust
use std::sync::Arc;
use isha_vector_db_core::api::{CollectionSpec, Database, DatabaseConfig, SearchRequest};
use isha_vector_db_storage_os::{OsStorage, SystemClock};
use isha_vector_db_core::document::DocumentInput;
use isha_vector_db_core::vector::VectorView;
use isha_vector_db_core::Metric;

fn main() -> isha_vector_db_core::Result<()> {
    let storage = Arc::new(OsStorage::open("./my-notes")?);
    let db = Database::open(storage, DatabaseConfig::default(), Arc::new(SystemClock))?;

    let notes = db.create_collection(CollectionSpec::new("notes", 4, Metric::Cosine))?;

    notes.upsert(DocumentInput::new("note-1", VectorView::f32(&[1.0, 0.0, 0.0, 0.0])))?;
    notes.upsert(DocumentInput::new("note-2", VectorView::f32(&[0.9, 0.1, 0.0, 0.0])))?;
    notes.flush()?;

    let hits = notes.search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0, 0.0, 0.0]), 2))?;
    for hit in hits.hits {
        println!("{} {}", hit.id, hit.score);
    }

    db.close()
}
```

`SystemClock` comes from `isha-vector-db-storage-os`, not the core. The core takes a `Clock` rather than
reading one because a deterministic test needs a controllable clock — `ManualClock` is what the
test suite uses — and because `SystemTime::now()` panics outright on `wasm32-unknown-unknown`.

## Choosing an index

The default is an exact scan. Hand in a different index at open time:

```rust
use isha_vector_db_index_hnsw::HnswIndex;

let db = Database::open_with_index(
    storage,
    DatabaseConfig::default(),
    Arc::new(SystemClock),
    Arc::new(HnswIndex::new()),
)?;
```

At 50,000 × 384 the graph index is about 12× faster than the SIMD scan, at 0.974 recall. It is
**approximate** — `is_exact()` reports false and the engine passes that on. Below a few tens of
thousands of vectors the exact scan is the better answer: nothing to build, nothing to invalidate,
and no recall to reason about.

The graph is persisted, so reopening restores it in ~41 ms rather than rebuilding for ~80 s.

## Batches

```rust
use isha_vector_db_core::WriteBatch;

let mut batch = WriteBatch::with_capacity(1000);
for (id, vector) in documents {
    batch.upsert(DocumentInput::new(id, VectorView::f32(vector)));
}
notes.write_batch(batch)?;
notes.flush()?;
```

A batch is atomic: all of it is applied or none of it is. This is the fast path for bulk import
and the one the benchmarks measure.

## Durability

```rust
use isha_vector_db_core::persistence::Durability;

DatabaseConfig::default().durability(Durability::Full)     // sync every write
DatabaseConfig::default().durability(Durability::Batch)    // the default
DatabaseConfig::default().durability(Durability::Relaxed)  // bulk import
```

`Batch` loses nothing to a process crash; only power loss can cost you the last batch. `Full` is
markedly slower on flash and is rarely the right trade on a device.

## Metadata and filters

```rust
use isha_vector_db_core::filter::Filter;
use isha_vector_db_core::metadata::{Metadata, Value};

let mut meta = Metadata::new();
meta.insert("kind", Value::Str("meeting".into()));
meta.insert("year", Value::I64(2026));
notes.upsert(DocumentInput::new("note-1", vector).with_metadata(meta))?;

let hits = notes.search(
    &SearchRequest::new(query, 10)
        .with_filter(&Filter::and(vec![
            Filter::eq("kind", Value::Str("meeting".into())),
            Filter::gte("year", Value::I64(2026)),
        ])),
)?;
```

## Storage backends

| Crate | For |
|---|---|
| `isha-vector-db-storage-os` | A real filesystem. What you want. |
| `isha-vector-db-storage-memory` | Tests, and power-loss simulation without a disk. |
| `isha-vector-db-storage-web` | WebAssembly, through host functions the embedder supplies. |

Implementing your own means the `Storage`, `File` and `FileLock` traits, and
`isha_vector_db_testkit::storage_conformance` is a 25-check suite that will tell you whether you got it
right. It has caught real bugs in the backends shipped here.

## Errors

`isha_vector_db_core::Result<T>` over `DbError`, a tree of about forty-five leaves each carrying a stable
numeric code.

```rust
match notes.upsert(document) {
    Err(e) if e.code() == ErrorCode::DIMENSION_MISMATCH => { /* ... */ }
    Err(e) if e.recoverability() == Recoverability::Retryable => { /* ... */ }
    other => other?,
}
```

[The full list](../api/error-codes.md) is generated from the code, so it cannot drift.

## Things that catch people out

**One handle per database.** `Database::open` takes an advisory lock; a second open in the same or
another process fails with `DatabaseAlreadyOpen`.

**`DatabaseConfig` is `#[non_exhaustive]`.** Use the builders, not a struct literal — that is
deliberate, so that adding a field later is not a breaking change.

**Search is synchronous and blocking.** There is no async API. Run it on a blocking thread pool if
you are inside an async runtime; `tokio::task::spawn_blocking` is the usual answer.
