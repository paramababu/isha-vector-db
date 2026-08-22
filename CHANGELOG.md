# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Four things are versioned independently: the **library** (SemVer), the **storage format** (an
integer), the **ABI** (an integer), and the **SDK packages**. Entries state which changed.

## [Unreleased]

### Changed

- **Storage format: version 2.** Metadata maps of eight fields or more now carry a table of
  `u16` offsets that field lookup binary-searches instead of walking
  ([ADR-0014](docs/adr/0014-metadata-offset-table.md)). Records below eight fields are
  byte-identical to v1.

  Measured on a sixteen-field corpus with the scalar kernel as a control: a filter naming the
  last key of a record is **4.7× faster**, one naming the first key is **1.54× slower**, and the
  average across fields improves **2.6×**. The regression on the best case is real — a binary
  search always pays `log2(n)` probes where a walk could get lucky on the first comparison. What
  the table buys is that lookup cost no longer depends on *which* field a filter names.

  `MIN_READABLE_VERSION` stays 1, so this release reads v1 databases. Two tests hold that:
  `this_build_can_read_every_v1_fixture` reads the untouched `testdata/v1/` structures, and
  `a_database_written_by_v1_still_opens` opens a complete database in `testdata/db-v1/` that was
  written by the v1 encoder — through recovery, verification and filtered search. A v1 build
  cannot read v2 files and says so, because every file header carries its own version.

  **The ABI did not change.** `vdb_abi_version()` is still 1 while `vdb_format_version()` returns
  2; no signature moved and no SDK needed an edit. This is the first exercise of that separation,
  and of the format-migration machinery generally.

### Added

- **A graph index (`vdb-index-hnsw`), Phase 3.** Approximate nearest neighbours over a
  hierarchical navigable small world graph, supplied to `Database::open_with_index` exactly as
  the flat index is, so `vdb-core` still knows nothing about it
  ([ADR-0015](docs/adr/0015-hnsw-index.md)).

  Measured against the NEON flat scan on the same corpus, `ef_search` 64: **3.5× faster at
  recall 1.000** on 5,000 × 128, and **12.8× faster at recall 0.974** on 50,000 × 384. Recall
  against the beam width at 10,000 × 128: 0.905 at ef 16, 0.960 at 32, 0.992 at 64, 1.000 at 128.

  Building takes about eighty seconds for 50,000 × 384. That was a serious limitation while the
  graph lived only in memory; **it is now persisted**, so reopening a database restores the graph
  in **40.9 ms instead of rebuilding it in 80.1 s — 1,958× faster**
  ([ADR-0016](docs/adr/0016-index-snapshots.md)). **Writes now extend the graph rather than rebuilding it**: when the rows
  already in the graph are still the leading rows of the source — the ordinary shape of a write
  to an append-only store — the new ones are appended. A batch build *is* sequential insertion,
  so appending in source order produces exactly the graph a full rebuild would, and a test pins
  that equivalence rather than assuming it. Compaction renumbers rows and correctly forces a
  rebuild.

  **Two concurrency defects were found by testing this**, having survived three commits of
  reasoning about the locking. A search could return results silently missing the newest
  documents, because `prepare` releases the lock before `search` takes it again and a concurrent
  writer can swap the graph in between; `search` now verifies the graph covers the source and
  falls back to the exact scan if not. And a build finishing for N rows could overwrite a graph
  another thread had already extended to N+5, since the guard asked only whether the existing
  graph was valid for N — which the newer one also fails.

  `VectorIndex::prepare` takes an `IndexSnapshots` — `load` and `store` — implemented by the
  engine over storage. **A snapshot is a cache, not data**, which is what let this ship without an
  on-disk format bump, without a migration, and without crash protection: anything stale,
  truncated, corrupt or unrecognised is discarded and the index rebuilds. Every single-byte flip
  across a snapshot is tested to produce a rebuild and identical answers.

  `VectorIndex` gained one defaulted method, `prepare(source, metric)`, so an index has somewhere
  to build that is not inside a `&self` search. Existing implementations are unaffected.

  A filtered search traverses *through* rejected rows and returns only accepted ones; when the
  graph cannot find `top_k` acceptable rows it hands over to the exact scan. That case is not
  exotic — a filter that correlates with position in the vector space, such as a category that
  happens to cluster, leaves the beam in a neighbourhood containing almost nothing that qualifies.
  Returning eight results where ten exist is a wrong answer, not an approximate one.


- **The web SDK (`sdk/web`), running the engine in WebAssembly on OPFS.** Phase 4.

  `vdb-storage-web` is a new storage backend that calls a hand-written table of host imports, so
  `vdb-core` still knows nothing about any platform. There is no `wasm-bindgen` and no bundler:
  the SDK drives the same `include/vdb.h` every other binding uses, and the toolchain is
  `cargo build --target wasm32-unknown-unknown`. The module is 362 KB.

  The engine calls storage synchronously, but OPFS sync access handles are *obtained*
  asynchronously, so a file cannot be opened when the engine asks. The OPFS adapter opens a pool
  of handles at start-up and assigns them to paths on demand, each slot carrying a header naming
  the path it holds so the mapping is recovered on reload with no separate index to keep
  consistent.

  Tested: the 25-check storage conformance suite and a full engine test run against the backend
  natively, through a Rust implementation of the same host imports; 7 JavaScript tests run the
  real WebAssembly module; and `sdk/web/test/browser.html` runs the engine in a Worker against
  **real OPFS in a headless browser**, writing, searching, deleting and then reloading the page
  to confirm the data survived.

  That browser check found something the Node suite structurally could not.
  `createSyncAccessHandle()` exists **only** on a Worker thread, and the first version of the
  page ran on the main thread and failed with "is not a function". A stand-in written from the
  same assumptions as the adapter agrees with them by construction — including the assumption
  that the method exists — so no amount of testing against it would have surfaced this.

  Two exports outside the C ABI, `vdb_wasm_alloc` and `vdb_wasm_free`: JavaScript cannot allocate
  in the module's linear memory. The header-drift guard now carries a named, self-checking
  exemption list rather than being widened.

  Found along the way: `SystemTime::now()` **panics** on `wasm32-unknown-unknown`, which arrived
  as `RuntimeError: unreachable` with no message. The wasm build now installs a panic hook that
  reports the message to the embedder, and takes the clock from the host.


- Architecture documentation: eleven design documents plus decision records
  (`docs/architecture/`, `docs/adr/`).
- `vdb-core`: structured error model with stable numeric codes and recoverability
  classification; `DbPath` with construction-time traversal prevention; CRC-32C, canonical
  LEB128 varints, and a population-counted bitmap; the `Storage`/`File`/`FileLock` traits and
  the capability model.
- `vdb-storage-memory`: the reference storage backend, with power-loss simulation so durability
  can be tested without a filesystem.
- `vdb-testkit`: the 25-check storage conformance suite and a seeded deterministic RNG.
- CI: formatting, clippy at deny-warnings, a stable and MSRV test matrix across three operating
  systems, cross-compilation to Android, iOS, macOS and wasm32, and the core-purity guard.

- `vdb-format`: on-disk format **version 1**. A 32-byte self-checksumming header on every file;
  a canonical metadata value codec that rejects unsorted keys, non-minimal varints and hostile
  nesting; the dual-slot manifest with its crash-safe slot-selection rule; the write-ahead log,
  which distinguishes a torn tail from real corruption and makes batches all-or-nothing; and the
  four segment blocks. Golden fixtures in `testdata/v1/` and six `cargo-fuzz` targets.
- `vdb-core` data model: `VectorView` (borrowed until the write-ahead log, with both a native
  `&[f32]` and a raw-bytes variant so bindings copy nothing), `Metadata` with dotted-path lookup
  and total resolution semantics, `DocId`/`RowId`, `DocumentInput`/`Document`, and the single
  limits table with validation for collection names, ids, dimensions, `top_k` and batch size.
- `vdb-core` write path: an arena-backed `Memtable` with deterministic flush ordering; a
  `WalWriter` that writes each transaction group in a single append so a commit record can never
  become durable separately from what it commits; replay that distinguishes a torn tail from
  damage; and `Durability` (`Full`/`Batch`/`Relaxed`, defaulting to `Batch`).
- `vdb-testkit`: `FaultyStorage`, injecting crashes, torn writes, `ENOSPC`, transient errors and
  dropped syncs at a chosen I/O operation.
- The crash sweep: five fault classes swept across every mutating I/O operation in a workload,
  asserting after each that recovery yields one of the legal committed prefixes.
- `vdb-core` persistence: `layout` (every path in the database built in one place, validated
  once), `ManifestStore` (dual-slot commit that never overwrites the slot it would fall back
  to), and segment flush/read-back including tombstone rewriting, orphan detection and
  cross-file row-count consistency checks.
- The crash sweep extended over the full write → flush → commit → checkpoint cycle, asserting
  both that recovery lands on a legal state and that the database stays usable afterwards.
- **Public API**: `Database` (open/close, create/open/drop/list collections, flush, stats) and
  `Collection` (insert/upsert/delete/get/contains/count/ids, atomic `write_batch`, flush,
  stats), plus `DatabaseConfig`/`CollectionSpec` builders, `WriteBatch`, and an injected
  `Clock`. Reads and writes go through the log before becoming visible, and the memtable
  auto-flushes at a configurable threshold.
- **Search**: `Collection::search` with cosine, L2 and inner product; bounded top-K selection;
  the scoring contract (`score` always higher-is-better, `distance` where the metric defines
  one, ties broken by ascending id); per-query metric override, score thresholds and
  `Include` control; and a cooperative `Budget` for cancellation and scan ceilings. Unflushed
  writes are searchable, and a buffered overwrite shadows its flushed copy.
- `vdb_core::index`: the `VectorIndex` trait, `VectorSource`, `LiveSet`, `RowPredicate`,
  `Budget`, and `ExactScan` — the always-available reference implementation.
- `vdb-index-flat`: the home of the future SIMD kernels, delegating to the reference today, with
  the differential and exactness suite that will validate them.
- **Metadata filters**: a typed expression tree (`Eq`/`Ne`/`Gt`/`Gte`/`Lt`/`Lte`/`In`/`Nin`/
  `Exists`/`IsNull`/`StartsWith`/`Contains`/`And`/`Or`/`Not`) over dotted field paths, wired
  into `SearchRequest`. Evaluation is total — a type mismatch is `false`, never an error — and
  the rules are documented in `docs/api/filters.md`. `top_k` counts matches rather than
  candidates, and `SearchStats` reports selectivity.
- **`vdb-storage-os`**: the filesystem backend. Positional I/O with short-read/short-write
  loops, `F_FULLFSYNC` on Darwin (where plain `fsync` does not flush the device cache),
  directory syncing so a rename is durable, and `flock`-based advisory locking that the kernel
  releases when a process dies. Passes the same conformance suite as the in-memory backend,
  unchanged, and runs the crash sweep against a real disk.
- **Compaction**: `Collection::compact` / `Database::compact` rewrite segments whose rows are
  mostly tombstones, reclaiming their space. Explicit rather than automatic, because rewriting
  hundreds of megabytes is a decision about I/O and battery the application is better placed to
  make.
- **Verification**: `Database::verify` at `Quick`, `Checksums` or `Full`. Reports rather than
  repairs, and never stops at the first fault — a verification that gives up cannot tell you how
  bad things are.
- **`vdb` CLI**: `stats`, `inspect`, `verify`, `compact`, `get`, `version`. Read-only by default,
  so it works on a database an application has open; distinct exit codes for damage versus
  failure to run.
- `Storage::describe` so errors name the actual location rather than the backend's type name.
- **Benchmark harness** (`vdb-bench`) with a committed baseline in `benchmarks/results/`.
  Clustered rather than uniform data, queries drawn from the corpus, documented percentile
  convention, and a refusal to write JSON from a debug build. Measures insert, search, filtered
  search, id lookup, cold open, recovery, compaction, storage amplification and peak memory.
- **SIMD kernels** in `vdb-index-flat`: NEON, AVX2+FMA with runtime detection, and `simd128`,
  each differential-tested against the portable reference at every vector length. 4.9× faster
  search on the benchmark machine. Reached via `Database::open_with_index`, so `vdb-core` keeps
  `forbid(unsafe_code)` and the layering that lets a build ship only the indexes it needs.
- `vdb-format` now fails to compile on a big-endian target, with a message explaining what a port
  would involve — the invariant is enforced rather than documented.
- **Lazy filter field lookup**: `vdb_format::find_path` walks an encoded metadata map and decodes
  only the named field, skipping the rest without allocating. Filtered search went from 1.5×
  slower than an unfiltered scan to break-even. No format change — the existing sorted-key
  encoding already supports an early stop.
- **The C ABI** (`vdb-ffi`, `include/vdb.h`): 25 functions covering lifecycle, collections,
  documents, search and metadata. Opaque handles, pointer-plus-length strings, zero-copy
  vectors, out-parameter errors with stable codes, `catch_unwind` at every entry point, and a
  null check on every pointer. Guarded by a bidirectional header/implementation drift test, an
  ABI behaviour suite, a real C program compiled and run in CI on Linux and macOS, a
  version-bump gate, and a 1.5 MB size budget.
- **Node SDK** (`@vdb/node`): an N-API addon plus a thin JavaScript layer, with TypeScript
  declarations that are the canonical shape every other JavaScript SDK will mirror. Supports
  `using` for scope-based closing. Tested on Node 18 through 22 from one binary, which is the
  claim N-API exists to make.
- **Android SDK**: a JNI shim and a Java API (`Vdb`, `Database`, `Collection`), both
  `AutoCloseable`. Cross-compiles to arm64-v8a, armeabi-v7a and x86_64, linked for 16 KB pages.
  The JNI boundary is tested on a desktop JVM, so the loop is seconds rather than an emulator.
- CI: `ci-android` runs the Java suite on Linux and macOS, cross-compiles every ABI, and checks
  both the page alignment and the per-ABI size budget.
- **iOS/macOS SDK**: a Swift package over the C ABI with no Objective-C layer, plus
  `build-xcframework.sh` producing device, simulator and macOS slices as a static library.
  Tested on macOS rather than in a simulator, so the loop is under a second.
- `measure-ios-size.sh` reports what linking the engine actually adds to an application — 662 KB
  dead-stripped — rather than the misleading size of the static archive.
- **Filters across the C ABI**: a postfix builder (`vdb_filter_*`) expressing any filter at any
  depth in eight functions, plus `vdb_search_filtered`. An unbalanced builder is refused rather
  than interpreted.
- **Filters in Swift**: an `indirect enum` tree with `&&`, `||` and `!`, flattened to postfix on
  the way down, so the stack is invisible to callers and an unbalanced sequence is
  unconstructible. Metadata can now be written from Swift too.
- **Filters in Node**: a query object — `{ category: 'tools', price: { $lt: 50 } }` — with
  `$and`/`$or`/`$not`, comparison operators, `$in`/`$nin`, `$exists`, `$startsWith` and
  `$contains`. Node binds the engine directly, so it takes the shape a JavaScript developer
  would write rather than the postfix builder the C bindings use.
- **Filters and metadata in Java**: `Metadata.of().set(...)` for writing, and a `Filter` tree
  with `and`/`or`/`not` that flattens to postfix on the way down. Plain classes rather than
  sealed interfaces or records, because this ships to Android where those need a recent API
  level or desugaring.
- **Compaction, verification and stats in every SDK**: `vdb_compact`, `vdb_verify`,
  `vdb_collection_stats` and `vdb_collection_flush` in the C ABI, with idiomatic wrappers in
  Swift, Java and Node. Until now only Rust and the CLI could reclaim space or check integrity,
  which was backwards for the platforms where storage is scarcest.
- Filter field lookup compares keys as bytes rather than validating UTF-8, cutting roughly 12%
  off a metadata lookup on the hot path of a filtered scan.
- Benchmarks gained a filter selectivity sweep and a first-key/last-key comparison, which
  together separate lookup cost from scan cost and size the remaining work.
- CI: `ci-format` refuses a golden-fixture change without a declared `FORMAT-CHANGE:` and a
  `FORMAT_VERSION` bump; `nightly` runs each fuzz target for an hour and reports coverage.

### Notes

- The storage format is defined but **not frozen**: it may still change without migration
  support until 0.1.
- The public API is not stable and will change without notice before 0.1.
