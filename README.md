# vdb

An **embedded, offline-first vector database**. One Rust core with no I/O of its own, one frozen
C ABI, and thin SDKs for React Native, Flutter, Android, iOS, Node.js and the web.

Think SQLite, not Milvus: a library your application links against to keep vectors and metadata
on local disk and search them. No server, no network, no daemon.

> **Status: Phase 0 → Phase 1.** The architecture is settled and the core is under construction.
> Nothing here is released yet, the API will change, and the on-disk format is not frozen.
> Do not put data you care about in it.

## Why this exists

Running similarity search on-device — for local RAG, semantic search over a user's own notes,
offline recommendations — currently means either shipping a server-shaped database into a phone
or writing a bespoke brute-force loop. This aims at the missing middle: something small and
correct that you can embed anywhere and trust with a user's data.

## Goals

- **Correct before fast.** Crash safety, checksums and recovery come before benchmark numbers.
- **Genuinely platform-independent.** The core compiles and passes its full test suite without
  knowing what an operating system is. Platform code lives strictly in adapters.
- **Small.** Target under 1.5 MB stripped per architecture. Mobile app-size budgets are real.
- **Honest.** No performance claims without a reproducible benchmark; no security claims without
  an implementation. There is no encryption in v1, and this README will say so until there is.

## Non-goals

No server or network protocol. No multi-process writers. No embedding generation — you bring the
vectors. No SQL. Full list and reasoning in
[docs/architecture/01-scope.md](docs/architecture/01-scope.md#14-explicit-non-goals-v1).

## Architecture

Start with [ARCHITECTURE.md](ARCHITECTURE.md) for the ten decisions everything follows from, then
[docs/architecture/](docs/architecture/README.md) for the detail and [docs/adr/](docs/adr/README.md)
for the decision records.

```text
Application → Platform SDK → Binding → Stable C ABI → Public API
            → Database core → Persistence → Storage trait → Storage impl → Host filesystem
```

Nothing at or below the public API knows which platform it is on. That is enforced by
[`scripts/check-core-purity.sh`](scripts/check-core-purity.sh) on every push, not by convention.

## Current state

| Component | State |
|---|---|
| Architecture, ADRs | Done |
| `vdb-core` error model, paths, utilities, storage traits | Done |
| `vdb-storage-memory` + storage conformance suite | Done |
| `vdb-format` — on-disk format v1, golden fixtures, fuzz targets | Done |
| Data model: vectors, metadata, documents, ids, limits | Done |
| Write path: memtable, WAL, replay, crash-sweep suite | Done |
| Segment flush, manifest commit, full reopen | Done |
| `Database`/`Collection` public API, CRUD, batches | Done |
| Search: metrics, top-K, exact scan | Done |
| Metadata filters | Done |
| `vdb-storage-os`: the real filesystem backend | Done |
| Compaction, verification, `vdb` CLI | Done |
| Benchmark harness and committed baseline | Done |
| SIMD kernels (NEON, AVX2, simd128) | Done |
| C ABI (`vdb.h`), frozen and guarded | Done |
| Node SDK (`@vdb/node`) | Done |
| Android SDK (JNI + Java API) | Done |
| iOS, Flutter, React Native | Next |
| `vdb-index-flat`, search, filters | Not started |
| `vdb-storage-os` | Not started |
| C ABI, SDKs, HNSW, web | Later phases |

Roadmap and ordering: [docs/architecture/11-roadmap-risks-order.md](docs/architecture/11-roadmap-risks-order.md).

## Building

```rust
use std::sync::Arc;
use vdb_core::{Database, DatabaseConfig, CollectionSpec, DocumentInput, ManualClock};
use vdb_core::vector::VectorView;
use vdb_storage_os::OsStorage;

let db = Database::open(
    Arc::new(OsStorage::open("/path/to/db")?),   // or MemoryStorage for tests
    DatabaseConfig::default(),
    Arc::new(ManualClock::default()),
)?;

let docs = db.create_collection(CollectionSpec::new("docs", 4, Metric::Cosine))?;
docs.insert(DocumentInput::new("a", VectorView::f32(&[1.0, 0.0, 0.0, 0.0])))?;
docs.insert(DocumentInput::new("b", VectorView::f32(&[0.0, 1.0, 0.0, 0.0])))?;

let results = docs.search(&SearchRequest::new(
    VectorView::f32(&[0.9, 0.1, 0.0, 0.0]),
    10,
))?;
assert_eq!(results.hits[0].id, DocId::from("a"));

db.close()?;
```

Scores are always higher-is-better, whatever the metric; ties break on ascending id.
Searches can be narrowed with a [metadata filter](docs/api/filters.md):

```rust
let cheap_tools = Filter::eq("category", Value::Str("tools".into()))
    .and(Filter::lt("price", Value::F64(50.0)));

let results = docs.search(
    &SearchRequest::new(VectorView::f32(&query), 10).with_filter(&cheap_tools),
)?;
```

## Building

```bash
cargo test --workspace          # everything
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
./scripts/check-core-purity.sh  # the architectural guard
```

Rust 1.78 or newer. The workspace has no third-party dependencies.

## Node.js

```bash
./scripts/build-node.sh          # builds sdk/node/vdb.node
cd sdk/node && npm test
```

```js
const vdb = require('@vdb/node');

using db = vdb.open('/path/to/db');
const docs = db.collection('docs', { dimension: 384, metric: 'cosine' });

docs.upsert('a', embedding, { category: 'tools', price: 25 });
const hits = docs.search(query, 10);   // [{ id, score, distance }]
```

Synchronous, because the engine is — run a large search in a worker if it would block your event
loop. `using` closes the database even when an exception unwinds past your cleanup.

## Android

```bash
./scripts/test-java.sh      # JNI + Java API on a desktop JVM, no device needed
./scripts/build-android.sh  # arm64-v8a, armeabi-v7a, x86_64 into sdk/android/src/main/jniLibs
```

```java
try (Database db = Vdb.open(context.getNoBackupFilesDir() + "/vectors");
     Collection docs = db.collection("docs", 384, Vdb.Metric.COSINE)) {
    docs.upsert("a", embedding);
    for (Collection.Hit hit : docs.search(query, 10)) {
        Log.d("vdb", hit.id() + " " + hit.score());
    }
}
```

The API is Java, so it is usable from Kotlin unchanged; a Kotlin coroutine layer comes next.
Libraries are linked for 16 KB pages, which Android 15 requires on some devices.

## The `vdb` tool

```bash
cargo run -p vdb-cli --example demo -- /tmp/vdb-demo   # build something to look at
cargo run -p vdb-cli -- stats   /tmp/vdb-demo
cargo run -p vdb-cli -- verify  /tmp/vdb-demo --full
cargo run -p vdb-cli -- compact /tmp/vdb-demo
cargo run -p vdb-cli -- get     /tmp/vdb-demo products doc-0999
```

`stats`, `inspect`, `verify` and `get` open read-only and take no lock, so they work on a
database an application currently has open. `verify` exits `3` when it finds damage, distinct
from `1` for "could not run", so a script can tell the difference.

## Performance

Measured, not asserted — see [benchmarks/](benchmarks/README.md) for the method and
[benchmarks/results/](benchmarks/results/) for committed baselines.

At 50,000 documents × 384 dimensions, on an Apple M-series laptop:

| workload | result |
|---|---|
| insert, one at a time | 35,300/s |
| search, k=10 | p50 6.2 ms (3.6× the portable reference) |
| get by id | p50 750 ns |
| cold open | 33.7 ms |
| storage overhead | 4.6% above the raw vectors |

**These are a reference point on one machine, not a claim about performance in general.** Mobile
numbers must come from mobile hardware and do not exist yet.

Search uses the SIMD kernels in `vdb-index-flat`, which are 3.6× faster than the portable
reference in `vdb-core` on this machine. `Database::open` gives you the reference — correct
everywhere, `unsafe`-free, slower; `Database::open_with_index` takes the accelerated one, which
is what every shipped SDK will pass.

Metadata filtering is roughly free — a filter passing 10% of documents costs about what an
unfiltered scan does. It does not yet make search *faster*; see
[docs/api/filters.md](docs/api/filters.md#what-filtering-costs).

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). The short version: every feature needs tests, every
storage-format change needs a version bump, and no performance claim ships without a benchmark.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
