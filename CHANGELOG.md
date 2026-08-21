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
- CI: `ci-format` refuses a golden-fixture change without a declared `FORMAT-CHANGE:` and a
  `FORMAT_VERSION` bump; `nightly` runs each fuzz target for an hour and reports coverage.

### Notes

- Storage format version 1 is defined but **not frozen**: it may still change without migration
  support until 0.1.
- The public API is not stable and will change without notice before 0.1.
