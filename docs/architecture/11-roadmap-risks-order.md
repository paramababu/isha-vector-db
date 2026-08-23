# 11. Roadmap, Risks, Implementation Order

## 11.1 Roadmap

| Phase | Version | Contents | Exit criteria |
|---|---|---|---|
| **0** | — | Architecture (this document set), ADRs, repo skeleton, CI skeleton | Reviewed and agreed; skeleton CI green |
| **1** | 0.1 | Core engine: CRUD, batch, flat index, 3 metrics, filters, WAL+manifest+segments, recovery, memory+os storage, CLI, full test suite, benchmarks | All §8.2 cases pass; fault-injection sweep clean; benchmark baseline committed |
| **1.5** | 0.2 | C ABI frozen; `vdb.h` v1; compaction; `verify`/`repair`; Node SDK | ABI conformance tests pass; Node SDK e2e green on 4 platforms |
| **2** | 0.3 | React Native, Flutter, Android, iOS SDKs; parity tests; per-platform examples and docs | Every SDK passes the shared parity suite on a real device |
| **3** | 0.4 | HNSW **done** ([ADR-0015](../adr/0015-hnsw-index.md)); index selection guidance; recall gates **done**; on-disk id table for large collections | Recall@10 ≥ 0.95 at documented params — **met** (0.974 at 50k x 384, 0.992 at 10k x 128, ef 64); build time benchmarked — **met** (95s at 50k x 384) |
| **4** | 0.5 | Web/WASM SDK (OPFS + IndexedDB fallback); encryption codec; scalar quantization | Web e2e in 3 browsers; encryption round-trip + key-rotation tests |
| **5** | 1.0 | API freeze; stability commitment; format frozen with a migration path proven by an actual v1→v2 migration — **done**, see [ADR-0014](../adr/0014-metadata-offset-table.md) | Three consecutive releases with no breaking changes; production users |
| **Later** | 1.x | IVF/PQ, hybrid BM25 + fusion, secondary metadata indexes, interactive transactions, multi-vector documents, sparse vectors, Python/Go/C# bindings | — |

## 11.2 Major technical risks and mitigations

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| R1 | **Cross-platform build matrix collapses under its own weight** — six toolchains, each breaking independently | Highest. This is the most common way projects like this die. | Build the CI matrix in Phase 0, before the code. One C ABI, not six binding styles. Every platform that isn't in CI is officially unsupported. Cap the SDK count at what the team can actually keep green. |
| R2 | **Data loss from a subtle persistence bug** | Fatal to trust; unrecoverable reputationally | Fault-injection sweep at every I/O point on every push (§8.3); `verify(Full)`; golden files; conservative defaults; the `Batch` durability default explained in the docs. |
| R3 | **Corrupt-file parser turns into a crash or OOM** | Crashes in the host app; a security issue | `forbid(unsafe_code)` in the format crate; every length checked against remaining file size; continuous fuzzing; a committed corrupt-file corpus. |
| R4 | **Mobile memory pressure kills the host app during a scan** | App-store-visible instability | Chunked scans with a bounded working buffer; `madvise` after chunks on Darwin; measure RSS in benchmarks; quantization on the roadmap as the structural fix. |
| R5 | **WASM memory ceiling** caps browser datasets at ~1–2 M vectors | Web positioning | Stream from OPFS rather than loading into WASM memory; document the ceiling numerically; recommend server-side search above it. |
| R6 | **React Native churn** (JSI/TurboModule APIs move between RN versions) | Recurrent breakage | Keep the C++ shim thin and mechanical; test the two most recent RN minors in CI; publish a support matrix; evaluate Nitro Modules to move the churn into someone else's codegen. |
| R7 | **Float non-determinism across architectures** reorders near-ties | Confusing, hard-to-diagnose user reports | Decided up front (§6.3): id tie-breaks, epsilon comparison, an opt-in deterministic-kernel feature, and it is documented. |
| R8 | **Premature ANN work destabilizes the core** | Correctness debt that is expensive to unwind | Rule 3: HNSW does not start until Phase 3, gated on a benchmarked, frozen core. Flat is the permanent ground truth. |
| R9 | **API churn after bindings exist** — every core change becomes six changes | Velocity collapse | Freeze the C ABI at 0.2, before any mobile SDK. Additive-only after that; `vdb_abi_version` guards mismatches. |
| R10 | **Binary size rejection on mobile** | Adoption blocker | Size budgets enforced in CI from day one; feature-gate indexes and storage backends; `opt-level="z"`, LTO, `panic=abort`, strip. |
| R11 | **Format mistake discovered after users have data** | Forced migration, or living with the mistake forever | `format_version` in every file from day one; `MigrationManager` and a v1→v2 rehearsal *before* 1.0; generous reserved fields; `header_len` allowing additive growth. |
| R12 | **iOS Data Protection makes the DB unreadable in the background** | Intermittent failures nobody can reproduce | Expose the protection class in config, default documented, typed `PermissionDenied` error rather than a mystery I/O failure. |
| R13 | **Maintainer bandwidth / bus factor** on an OSS project with 6 SDKs | Slow death by unmaintained bindings | Per-SDK `CODEOWNERS`; an explicit "community-maintained, not core-supported" tier for SDKs without a maintainer; be willing to ship fewer SDKs well. |
| R14 | **Scope creep into a server database** | Loses the one thing that makes the project distinctive | The non-goals list (§1.4) is in the README and is quoted when closing issues. |
| R15 | **Benchmarks that flatter us** (desktop numbers, unrealistic dimensions, no recall) | Loss of credibility on first real use | Fixed methodology, device numbers for mobile claims, recall reported alongside every ANN latency, reproduction scripts published. |

## 11.3 Recommended implementation order

Ordered so that each step is independently testable and the riskiest, hardest-to-change decisions
are validated earliest.

**Phase 0 — foundation (no engine code yet)**
1. Repo skeleton, workspace, licence, MSRV, lint config, `deny.toml`, PR/issue templates.
2. `ci-rust.yml` + `ci-core-purity.yml` green on an empty workspace. CI before code, always.
3. This architecture set + ADRs 0001–0012 merged and reviewed.

**Phase 1 — core (the long pole; roughly two-thirds of total effort)**
4. `error` module: the full `DbError` tree and `ErrorCode` table. Everything else returns these, so
   it is first.
5. `util`: crc32c, varint, bitmap, ordered float — small, pure, fully unit-tested.
6. `storage` traits + `isha-vector-db-storage-memory` + the storage conformance suite in `isha-vector-db-testkit`.
   *Now the whole engine can be developed and tested with no filesystem.*
7. `isha-vector-db-format`: file headers, manifest, WAL frames, segment blocks, metadata encoding. Property
   tests, fuzz targets, and the first golden fixtures. **Freeze format v1 here and review it as a
   group** — this is the least reversible decision in the project.
8. `vector`, `metadata`, `document`, `validation` with the limits table.
9. Memtable + write path + WAL append + replay. Fault-injection sweep turned on at this point,
   before there is enough code for bugs to hide in.
10. Segment writer/reader + dual-slot manifest commit + full recovery. Extend the sweep.
11. `catalog`, `Database`/`Collection` public API, snapshots, `close` semantics.
12. `search`: metrics, scalar kernels, `TopK` with tie-breaking, the scoring contract.
13. `isha-vector-db-index-flat` scalar-only, plus the index conformance suite. **First end-to-end search.**
14. `filter` AST, evaluator, and the planner (bitmap vs streaming).
15. `isha-vector-db-storage-os` (positional I/O, locks, platform fsync quirks); run the entire existing test
    suite against it unchanged — this is the payoff for step 6, and the proof that the storage
    abstraction is honest.
16. SIMD kernels behind runtime dispatch, differential-tested against scalar.
17. `stats`, `verify`, `compact`.
18. `isha-vector-db-cli`: `inspect`/`verify`/`dump`/`compact`/`bench`. Invaluable for every debugging session
    from here on; building it early pays for itself.
19. `benchmarks/`: harness + first committed baseline. **Ship 0.1.**

**Phase 1.5 — freeze the boundary**
20. Throwaway second index (LSH or IVF-flat) purely to prove the `VectorIndex` seam, then delete it.
21. `isha-vector-db-ffi`: the C ABI, `cbindgen` header, `catch_unwind` wrappers, ABI conformance tests, ASAN.
    **Freeze `vdb.h`.**
22. `isha-vector-db-node` + `sdk/node` (a shared sdk/typescript package remains planned). Node first: the fastest feedback loop of any
    binding, and it shakes out ABI ergonomics before four harder platforms depend on them. **0.2.**

**Phase 2 — SDKs, in this order and for these reasons**
23. **Android** (`cargo-ndk` is the least painful native pipeline; emulator CI is cheap).
24. **iOS** (XCFramework pipeline is fiddly; do it while the ABI is fresh).
25. **Flutter** (needs both 23 and 24 to exist; `ffigen` then makes it fast).
26. **React Native** (highest integration complexity; benefits from everything learned above).
27. Parity test suite + per-platform examples + docs. **0.3.**

**Phase 3 — HNSW.** 28. Implementation, 29. recall gates and benchmarks, 30. filtered-search
oversampling and the flat fallback. **0.4.**

**Phase 4 — Web.** 31. `vdb-storage-opfs` against the conformance suite, 32. `vdb-wasm` + worker
RPC + `@isais-logic/isha-vector-db-web`, 33. IndexedDB fallback, 34. browser e2e matrix. **0.5.**

**Phase 5 — 1.0.** 35. Rehearse a v1→v2 migration end to end (even if v2 is a trivial change) to
prove `MigrationManager` works before anyone needs it. 36. API review and freeze. 37. Documentation
completeness pass. 38. Stability commitment. **1.0.**

### Two ordering choices worth defending

- **CI and the error model come before any feature.** Both are things that are nearly impossible to
  retrofit: a codebase written without structured errors ends up with `String` errors in a thousand
  places, and a project whose CI arrives late accumulates platform breakage it never pays down.
- **Node before mobile.** It is tempting to start with the platform the product needs most. But the
  first binding is where all the ABI design mistakes surface, and finding them in a five-second
  `npm test` loop rather than a five-minute Gradle/Xcode loop is worth several weeks.
