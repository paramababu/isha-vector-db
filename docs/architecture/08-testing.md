# 8. Testing Architecture

Reliability is the product. The test architecture is therefore designed before the code, and it is
what justifies the "production-quality" claim — not the feature list.

## 8.1 Layers

| Layer | Where | What it proves | Runs |
|---|---|---|---|
| Unit | `crates/*/src/**/#[cfg(test)]` | Module-local logic, boundaries, error paths | every push, seconds |
| Integration | `crates/vdb-core/tests/` | Public API behaviour only (no internals) | every push |
| Property | `proptest` in core + format | Invariants over generated inputs | every push |
| Fuzz | `crates/vdb-format/fuzz/` | Decoders never panic/OOM/hang on any bytes | nightly + on format PRs |
| Fault injection | `vdb-testkit::FaultyStorage` | Crash at any I/O point → recoverable | every push |
| Golden format | `testdata/` | Byte-level format stability across versions | every push |
| Concurrency | stress + `loom` | Snapshot isolation, no data races | every push (loom: nightly) |
| Conformance | `vdb-testkit::suites` | Every `Storage`/`VectorIndex` impl obeys the contract | per impl |
| Cross-platform | CI matrix | Same results on linux/mac/win/android/ios/wasm | every push |
| Stress / soak | `benchmarks/` | Large datasets, long runs, memory stability | nightly |
| Recall | `crates/vdb-index-hnsw/tests/recall.rs`, and `hnsw_recall_at_10` in the benchmark suite | ANN quality vs exact ground truth | per index PR |
| SDK e2e | `sdk/*/test` | Each binding round-trips real data | per SDK change |

## 8.2 Non-negotiable behavioural cases

Every one of these gets a named test, and the list goes in `CONTRIBUTING.md` as the review
checklist for any PR touching the engine:

**Lifecycle** — open non-existent with/without `create_if_missing`; open twice (lock); close then
use (must be a compile error via consuming `close`, plus a runtime check via the FFI handle);
reopen after clean close; reopen after simulated kill; open read-only then attempt a write; open a
path that is a file; open with no write permission.

**CRUD** — insert/get round-trip preserving exact float bits; duplicate id → `DuplicateId`; upsert
overwrites; update non-existent → `DocumentNotFound`; delete twice; get after delete; count after
mixed ops; batch with a failure in the middle leaves *nothing* applied; empty batch; batch at the
size limit and one over.

**Validation** — wrong dimension (both directions); zero-length vector; NaN and ±Inf components;
empty id; id at and over the length limit; ids with NUL, newlines, and 4-byte UTF-8; collection
names `..`, `a/b`, `""`, 65 chars; metadata over size; metadata nested past depth; `top_k` of 0 and
of `MAX+1`.

**Search** — empty collection returns zero hits (not an error); `k` > document count; all
documents filtered out; exact duplicate vectors (tie-break order is stable and asserted); the
query vector itself is its own nearest neighbour with score 1.0 for cosine; each metric verified
against an independently computed reference; `min_score` boundary is inclusive; filter combinations
(nested and/or/not, missing fields, type mismatches); search on a collection with only deleted
documents; search while a write is in flight.

**Persistence** — write, kill, reopen, verify every document; kill during flush; kill during
compaction; kill during migration; truncate each file type at every 512-byte boundary and assert
the error is a `CorruptionError` and never a panic; flip a bit in each file type and assert the
checksum catches it; delete a segment file; delete both manifests; a manifest from the future;
an empty file; a zero-byte database directory; a directory containing unrelated files.

**Concurrency** — N readers + 1 writer for M seconds with no torn reads; a snapshot held across a
compaction still returns the correct data; a snapshot taken before an insert does not see it.

**Scale** — 1, 2, 1k, 100k documents; a 1-dimensional collection; a 4096-dimensional collection;
one document with 64 KiB of metadata; 100k documents where 99% are deleted.

## 8.3 Fault injection: the centrepiece

`vdb-testkit::FaultyStorage` wraps any `Storage` and can, at a chosen operation index, inject:
a torn write (first N bytes only), `ENOSPC`, `EIO`, a silently dropped `sync_data`, a process
"crash" (all subsequent ops fail and the handle is poisoned), a truncated file, or bit rot.

The driver test is:

```text
for op_index in 0..total_ops_of_the_workload:
    run workload with a crash injected at op_index
    reopen the database
    assert: open succeeds (or fails with a CorruptionError that names the file)
    assert: the state equals the state after some prefix of committed operations
    assert: no committed batch is partially applied
    assert: verify(Full) passes
```

Because `vdb-storage-memory` makes this run without touching a disk, the whole sweep executes in
seconds and runs on every push. This single test class is worth more than any amount of hand-written
"does it save?" testing, and it is the thing that catches the bugs that would otherwise be found by
a user losing their data on a subway platform.

## 8.4 Conformance suites

`vdb-testkit` exports `storage_conformance(impl)` and `index_conformance(impl)` — generic suites
every implementation must pass, including ones written by third parties. The storage suite verifies
positional I/O semantics, append offsets, truncation, sync, locking, and that each declared
capability actually behaves as declared (e.g. if you claim `atomic_rename`, a rename is observed as
all-or-nothing). The index suite verifies exactness where claimed, `LiveSet` respect, filter
respect, save/load round-trip fidelity, and result-ordering determinism.

This is how "the abstraction is real" gets enforced mechanically rather than by hope.

## 8.5 Determinism and golden tests

- Every test uses a seeded RNG (`SmallRng::seed_from_u64`) and an injected clock; failures are
  reproducible from the seed printed on failure.
- `testdata/v1/` holds databases generated once and committed. A test opens each and asserts exact
  query results. Regenerating them requires an explicit `--bless` flag and a PR explanation.
- Snapshot tests over encoded bytes for each format structure, so an accidental layout change fails
  loudly at the exact byte offset.

## 8.6 Coverage policy

`cargo-llvm-cov` in CI, published to the PR. Targets: **≥ 90% lines in `vdb-core` and
`vdb-format`**, ≥ 80% elsewhere. Explicitly **not** a merge gate on the number, because coverage
gates reward tests that execute code without asserting anything. The merge gate is the review
checklist in §8.2. Coverage is used to *find* untested error paths — an uncovered `Err` branch is
the signal worth acting on.

## 8.7 Test execution budget

Pre-commit hook: `fmt` + `clippy` + unit tests (< 30 s). Pull request: everything except fuzz, soak
and device tests (< 10 min). Nightly: fuzz (1 h/target), soak, full device matrix, benchmarks,
miri, loom. Keeping the PR loop under ten minutes is a design constraint on the test suite itself;
a slow suite gets skipped, and a skipped suite is worthless.
