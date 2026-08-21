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

### Notes

- Storage format: not yet defined. Nothing persists.
- The public API is not stable and will change without notice before 0.1.
