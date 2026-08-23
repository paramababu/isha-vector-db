# Architecture Overview

An embedded, offline-first vector database — SQLite-shaped, not Milvus-shaped — with one Rust core
and thin SDKs for React Native, Flutter, Android, iOS, Node.js and the Web.

Full detail lives in [`docs/architecture/`](docs/architecture/README.md); decisions in
[`docs/adr/`](docs/adr/README.md). This page is the map and the summary of what was decided.

---

## The ten decisions everything else follows from

| # | Decision | Why |
|---|---|---|
| 1 | **Rust core**, one C ABI, WASM for the browser | Only way to serve six platforms from one implementation without a GC or a runtime; and we are parsing untrusted files, where memory safety is not optional |
| 2 | **The core does no I/O** — a `Storage` trait is injected at `Database::open` | The single mechanism that makes "platform-independent" true rather than aspirational; enforced by a CI job, not by convention |
| 3 | **Append-only WAL + immutable segments + dual-slot manifest** | Crash safety on mobile, where the process dies without warning; dual-slot avoids depending on atomic `rename`, which OPFS cannot promise |
| 4 | **Columnar `.vec` block, fixed stride, mmap-able** | Brute-force search is memory-bandwidth-bound; the layout *is* the performance work |
| 5 | **Single writer, lock-free snapshot readers, zero threads in the core** | Concurrency you can reason about; each platform brings its own async model |
| 6 | **Atomic batches, not interactive transactions, in v1** | Covers the real workload; interactive txns mean MVCC or app-level deadlocks |
| 7 | **`score` is always higher-is-better; ties break on `DocId`** | One ordering rule for every metric and every index — and the only way "deterministic" is achievable |
| 8 | **Flat index only until the core is frozen and benchmarked** | Rule 3. Flat is also the permanent ground truth every ANN index is measured against |
| 9 | **Own the on-disk bytes** — no third-party serialization in the format crate | A dependency's encoding change would silently break users' existing databases |
| 10 | **Freeze the C ABI before writing any mobile SDK** | After bindings exist, every core change costs six changes |

---

## Layers

```text
Application
   ↓
Platform SDK        TS / Dart / Kotlin / Swift — idiomatic, async, owns threading
   ↓
Binding             JSI · dart:ffi · JNI · Swift/C · N-API · wasm-bindgen
   ↓
Stable C ABI        vdb.h — frozen contract, ~35 functions, opaque handles
   ↓
Public Rust API     Database · Collection · Snapshot · Query · DbError
   ↓
Database Core       catalog · write path · search · filter · index · segments
   ↓
Persistence         manifest · WAL · recovery · compaction · migration
   ↓
Storage trait       Storage · File · FileLock · StorageCapabilities
   ↓
Storage impls       memory · os (fs + mmap) · opfs (wasm)
   ↓
Host filesystem
```

Nothing at or below the Public Rust API knows which platform it is running on.

## Crates

```text
isha-vector-db-format ← isha-vector-db-core ← { isha-vector-db-index-flat, isha-vector-db-storage-memory, isha-vector-db-storage-os, vdb-storage-opfs, isha-vector-db-testkit }
                     ↖ isha-vector-db-ffi → sdk/{react-native, flutter, android, ios}
                     ↖ isha-vector-db-node → sdk/node
                     ↖ vdb-wasm → sdk/web
                     ↖ isha-vector-db-cli
```

Indexes and storage backends depend on the core, never the reverse — which is what lets a mobile
build contain only `flat + os` and a web build only `flat + opfs`.

## On-disk shape

```text
<db-root>/
├── LOCK  MANIFEST-A  MANIFEST-B          # dual-slot root, highest valid sequence wins
└── collections/<name>/
    ├── CATALOG                            # dimension, metric, dtype, index spec (immutable)
    ├── wal/NNNNNN.wal                     # CRC'd frames; torn tail truncated on replay
    ├── segments/NNNNNN.{vec,dir,meta,del} # immutable; .del is the only mutable part
    └── index/flat-NNNNNN.idx              # derived cache; rebuildable, never fatal if lost
```

Every file opens with a 32-byte header: magic + kind, `format_version`, flags, `header_len`,
`payload_len`, header CRC. No length field is ever trusted before it is checked against the file
size.

## What v1 is and is not

**Is:** open/close, collections, insert/upsert/update/delete/get/count/scan, atomic batches,
cosine/L2/dot top-K with metadata filters and thresholds, durable persistence with crash recovery,
checksummed corruption detection, a structured error tree, a CLI, and a benchmark suite.

**Is not:** a server, multi-process, an embedding model, SQL, encrypted, HNSW, or in the browser.
Those are phases 3–5 or explicit non-goals — see [§1.4](docs/architecture/01-scope.md#14-explicit-non-goals-v1).

## Delivery order

```text
Phase 0  architecture + CI skeleton                     ← no engine code yet, deliberately
Phase 1  core engine, flat index, persistence, tests    → 0.1   (≈ two-thirds of total effort)
Phase 1.5 freeze the C ABI, Node SDK                    → 0.2
Phase 2  Android → iOS → Flutter → React Native         → 0.3
Phase 3  HNSW with recall gates                         → 0.4
Phase 4  Web: WASM + OPFS (+ IndexedDB fallback)        → 0.5
Phase 5  migration rehearsal, API freeze                → 1.0
```

Full step-by-step ordering: [§11.3](docs/architecture/11-roadmap-risks-order.md#113-recommended-implementation-order).

## The three things most likely to go wrong

1. **The six-platform build matrix collapsing** — mitigated by building CI in Phase 0 and by
   having exactly one ABI. Any platform not in CI is officially unsupported.
2. **A persistence bug losing data** — mitigated by the fault-injection sweep (§8.3), which crashes
   the database at *every* I/O operation index and asserts recovery, on every push.
3. **API churn after bindings exist** — mitigated by freezing the ABI at 0.2, before mobile.

Full register: [§11.2](docs/architecture/11-roadmap-risks-order.md#112-major-technical-risks-and-mitigations).
