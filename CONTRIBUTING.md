# Contributing

Thanks for considering it. This document is short on ceremony and specific about the few rules
that actually matter for a database.

## Getting set up

```bash
rustup toolchain install stable
cargo test --workspace
./scripts/check-core-purity.sh
```

No other tooling is required. The workspace has no third-party dependencies, so there is nothing
to vendor and no lockfile drama.

## Before you open a pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
./scripts/check-core-purity.sh
```

CI runs all of these plus a cross-compilation matrix. Nothing merges red.

## The engineering rules

These are not style preferences. Each one exists because breaking it is expensive to unwind
after users have data on disk.

1. **Do not mix platform-specific code into the core.** `vdb-core` and `vdb-format` perform no
   I/O, forbid `unsafe`, and have no dependencies. `scripts/check-core-purity.sh` enforces all
   three.
2. **Do not over-engineer the MVP.** A smaller correct thing beats a larger nearly-correct thing.
3. **Do not implement an approximate index before the exact one is right.** Flat search is the
   ground truth every other index is measured against.
4. **Do not trade correctness for benchmark numbers.**
5. **Do not leak internals through public APIs.**
6. **Do not swallow errors.** No `let _ =` on a fallible call outside tests.
7. **Do not use unstructured errors for predictable failures.** Add a variant to `DbError` with
   the context a developer needs, and a code in `error/code.rs`.
8. **Every feature needs tests**, including its error paths.
9. **Every storage-format change needs a version bump** and a note in the PR body.
10. **Every public API change must consider backward compatibility.**
11. **Avoid dependencies.** Adding one to `vdb-core` or `vdb-format` needs an ADR and will be
    rejected by CI until that ADR exists.
12. **Prefer small, well-defined interfaces.**
13. **A `#[non_exhaustive]` type that callers construct needs a constructor or builders.**
    `#[non_exhaustive]` keeps adding a field from being a breaking change, but it also stops any
    code outside this crate from writing a struct literal. If an SDK, a third-party storage
    backend or a third-party index has to *build* the type, the constructor is part of the API,
    not sugar on top of it. This has caught us three times already — `StorageCapabilities`,
    `DatabaseConfig`, `IndexStats`.
14. **Keep the core deterministic.** Seeded RNG, injected clock, stable tie-breaking.
15. **Document architectural decisions.** If a review argument about *why* takes more than one
    round trip, write an ADR (`docs/adr/0000-template.md`).
16. **Benchmark before making performance claims.**

## Writing tests

New engine behaviour needs, at minimum:

- the happy path;
- every error path it can produce;
- the boundary values of any limit it touches;
- for anything that touches disk, a case that crashes partway through and reopens.

The behavioural checklist in
[docs/architecture/08-testing.md §8.2](docs/architecture/08-testing.md) is the review checklist.
Coverage numbers are published but are not a merge gate — a test that executes code without
asserting anything is worse than no test, because it looks like protection.

Tests use a seeded RNG (`vdb_testkit::Rng`) so a failure is reproducible from the seed.

## Adding a storage backend or an index

Implement the trait, then make its conformance suite pass:

```rust
#[test]
fn my_backend_is_conformant() {
    vdb_testkit::storage_conformance(&|| Box::new(MyStorage::new())).assert_ok();
}
```

The suite checks that the capabilities you *declare* are the capabilities you *have*. Claiming
one you do not have is the failure mode that loses data in the field, so that check is not
optional and not skippable.

## Commit messages and PRs

Conventional-ish prefixes (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `chore:`).
Explain *why* in the body. If you changed the on-disk format, say `FORMAT-CHANGE:` and explain
what migrates.

## Code of conduct

By participating you agree to [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
