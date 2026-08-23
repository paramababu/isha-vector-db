# 1. Scope: Requirements, Goals, Non-Goals, MVP

> Working codename: **`vdb`**. Pick and verify the real name (crates.io + npm + pub.dev + GitHub org
> all free) *before* the first public tag. Renaming after v0.1 is published is expensive.

## 1.1 What this project is

An **embedded, offline-first vector database**: a library that a single application process links
against to store vectors + metadata on local disk and run similarity search over them, with no
server, no network, and no background daemon.

The closest well-known analogues are SQLite (embedded, file-backed, single-writer) and LanceDB /
usearch / sqlite-vec — *not* Pinecone, Milvus, Qdrant or Weaviate, which are network services.
Every architectural decision below follows from "embedded like SQLite", not "distributed like
Milvus".

## 1.2 Functional requirements (v1 contract)

| Area | Requirement |
|---|---|
| Lifecycle | `open`, `close`, idempotent close, crash-safe reopen |
| Collections | create, drop, list, describe; per-collection dimension + metric fixed at creation |
| Writes | insert, upsert, update (vector and/or metadata), delete by id, batch variants |
| Reads | get by id, get many by id, exists, count, scan/iterate with cursor |
| Search | top-K by cosine / L2 / dot product, with metadata filter and score threshold |
| Metadata | typed values (null, bool, i64, f64, string, bytes, array, map), filterable |
| Persistence | durable across process kill and device power loss |
| Integrity | checksums on every persisted block; corruption detected on read, never silently ignored |
| Index | pluggable; brute-force (flat) in v1; build, save, load, rebuild |
| Atomicity | a batch either fully applies or does not apply at all |
| Stats | counts, dimension, disk bytes, memory bytes, segment/index state |
| Errors | structured, typed, machine-matchable, carrying enough context to debug |
| Determinism | same inputs → same result order, on the same build |

## 1.3 Non-functional requirements

- **Platform-independent core.** The engine compiles and passes its full test suite with zero
  knowledge of the host OS. All I/O goes through injected traits.
- **Small.** Target < 1.5 MB stripped per-architecture native library for the MVP feature set.
  Mobile app-size budgets are real and are a common reason libraries get rejected.
- **Predictable.** No GC pauses, no hidden background threads, no surprise allocations on the
  search hot path.
- **Deterministic.** Given the same database bytes and the same query, the same build returns the
  same ordered results. (See §6 on cross-architecture float caveats — this is subtler than it looks.)
- **Dependency-light.** Every third-party crate in `isha-vector-db-core` / `isha-vector-db-format` must be justified in an
  ADR. The on-disk format has **zero** third-party codecs (see ADR-0004).

## 1.4 Explicit non-goals (v1)

Writing these down is as important as the goals; they are what keep the MVP finishable.

1. **Not a server.** No network protocol, no auth, no multi-tenancy, no replication, no sharding.
2. **Not multi-process.** One process writes and reads a database directory at a time. Enforced by
   an advisory lock file; not a security boundary.
3. **Not an embedding model.** The database never produces embeddings. Callers bring their own
   vectors. (This keeps us out of the ML-runtime dependency swamp entirely.)
4. **Not SQL.** The filter language is a small typed AST, not a query language with a parser.
5. **No full-text / hybrid search in v1.** BM25 + fusion is on the roadmap, not the MVP.
6. **No encryption in v1.** The storage layer is *designed* so encryption can be added as a codec
   (§10.3), but v1 ships unencrypted and the README will say so in plain words.
7. **No GPU, no distributed index, no DiskANN in v1.**
8. **No big-endian support.** All on-disk integers and floats are little-endian. Every target we
   care about (x86_64, aarch64, wasm32) is little-endian. Documented as a format invariant so a
   future port knows exactly what to do.
9. **No interactive multi-statement transactions in v1.** Atomic batches only — see §7.4 for why.
10. **No automatic schema migration of user metadata.** We version the *storage format*, not the
    user's metadata shape.

## 1.5 MVP scope (Phase 1 — "the core is correct")

**In:**

- `isha-vector-db-core`: Database, Collection, Document, Vector, Metadata, Filter, Snapshot, error model
- `isha-vector-db-format`: v1 on-disk format, encoders/decoders, golden fixtures, fuzz targets
- `isha-vector-db-storage-memory` (reference + test double), `isha-vector-db-storage-os` (std::fs + optional mmap)
- `isha-vector-db-index-flat`: exact brute-force search, SIMD-accelerated with a scalar reference path
- CRUD + batch, cosine/L2/dot, top-K, metadata filtering, threshold
- WAL + dual-slot manifest + segment files + crash recovery
- `isha-vector-db-cli`: `inspect`, `verify`, `compact`, `bench` — indispensable for debugging the format
- Test suite: unit, integration, property, fuzz, fault-injection recovery, golden-format
- Benchmark harness producing real numbers (no claims without them)
- Docs: architecture (this set), format spec, API reference, getting started

**Out of the MVP** (deliberately): HNSW, any platform binding, WASM, encryption, quantization,
compaction scheduling beyond a manual `compact()`, secondary metadata indexes.

The MVP has **no bindings at all**. That is intentional: bindings multiply the cost of every core
change, so the core API must stop moving first. Phase 2 starts the day the C ABI is frozen.
