# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Four things are versioned independently: the **library** (SemVer), the **storage format** (an
integer), the **ABI** (an integer), and the **SDK packages**. Entries state which changed.

## [Unreleased]

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
- CI: `ci-format` refuses a golden-fixture change without a declared `FORMAT-CHANGE:` and a
  `FORMAT_VERSION` bump; `nightly` runs each fuzz target for an hour and reports coverage.

### Notes

- Storage format version 1 is defined but **not frozen**: it may still change without migration
  support until 0.1.
- The public API is not stable and will change without notice before 0.1.
