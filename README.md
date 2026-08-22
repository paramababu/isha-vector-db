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
| CLI, benchmarks, then the C ABI | Next |
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

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). The short version: every feature needs tests, every
storage-format change needs a version bump, and no performance claim ships without a benchmark.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
