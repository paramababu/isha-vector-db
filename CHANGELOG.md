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
