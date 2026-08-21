# 10. CI/CD, Security, Performance

## 10.1 CI/CD architecture

Workflows are split by cost so the fast ones gate every PR and the slow ones run nightly.

| Workflow | Trigger | Jobs |
|---|---|---|
| `ci-rust.yml` | every push/PR | `fmt --check`; `clippy -D warnings`; build+test on linux/macos/windows × stable+MSRV; `--no-default-features` and `--all-features` builds; doc build with `-D warnings`; `cargo-deny` (licenses, advisories, duplicate/banned deps) |
| `ci-core-purity.yml` | every push/PR | asserts `vdb-core` and `vdb-format` link no I/O: no `std::fs`/`net`/`thread`/`time` imports, `forbid(unsafe_code)` present, dependency allow-list respected. **The mechanical guarantee behind rule 1.** |
| `ci-format.yml` | PR touching format/testdata | golden-file tests; a diff in `testdata/` fails unless the PR body contains `FORMAT-CHANGE:` with a rationale and a version bump |
| `ci-crossbuild.yml` | every PR | `cargo build` for aarch64-android, armv7-android, aarch64-ios, ios-sim, wasm32; artifact size budgets enforced |
| `ci-wasm.yml` | PR touching wasm/web | `wasm-pack test --headless` in Chrome + Firefox; bundle-size budget; OPFS integration tests |
| `ci-node.yml` | PR touching node | napi build matrix; `vitest` e2e; import test in ESM and CJS |
| `ci-mobile.yml` | PR touching android/ios SDK | Android instrumented tests on an emulator; iOS XCTest on a simulator; Swift/Kotlin lint |
| `ci-flutter.yml` | PR touching flutter | `dart analyze`, `flutter test`, integration test on an emulator |
| `ci-rn.yml` | PR touching react-native | build the example app for both platforms; Detox/Maestro smoke test |
| `nightly.yml` | schedule | `cargo-fuzz` (1 h per target); miri on unsafe crates; ASAN/TSAN on the FFI; `loom`; soak test; full benchmark suite; coverage report |
| `bench.yml` | PR label `perf` + nightly | benchmark suite on a fixed self-hosted runner; compares against `benchmarks/results/baseline.json` |
| `release.yml` | tag `v*` | build all native artifacts; assemble XCFramework + AAR; publish crates, npm packages, pub.dev, Maven Central, CocoaPods; attach checksums + SBOM + provenance; verify the changelog was updated |
| `semver.yml` | PR to main | `cargo-semver-checks` on published crates; ABI check: the committed `vdb.h` must be regenerable and any change must bump `vdb_abi_version` |

**Merge protection:** `ci-rust`, `ci-core-purity`, `ci-crossbuild`, `semver` and the relevant
platform workflow must pass. No admin bypass on the format and semver gates — those are exactly the
gates people are tempted to bypass under deadline pressure, and exactly the ones whose breach is
irreversible once users have data on disk.

**Benchmark regression policy:** benchmarks are noisy on shared runners, so they run on a fixed
self-hosted machine, report p50/p95, and fail only on a > 20% regression sustained over three runs.
**Recall**, by contrast, is deterministic and gated hard: any drop below the declared threshold for
an index fails the build.

**Supply chain:** dependencies pinned via a committed `Cargo.lock`; `cargo-deny` blocks unvetted
licenses and advisories; Dependabot grouped weekly; publishing uses trusted publishing / OIDC where
the registry supports it; every release artifact ships a SHA-256 and an SBOM.

## 10.2 Versioning strategy

Four independently versioned things, and conflating them is a classic source of pain:

| Thing | Scheme | Rule |
|---|---|---|
| **Library version** | SemVer | pre-1.0: minor = breaking. Post-1.0 the public API is stable. |
| **Storage format version** | integer, `1`, `2`, … | bumped only for breaking layout changes; `MIN_READABLE` documented per release |
| **ABI version** | integer | bumped on any `vdb.h` change; bindings check at load |
| **SDK package versions** | SemVer, kept in lockstep on major+minor | patch versions may diverge for SDK-only fixes |

A compatibility matrix (library version × format versions readable/writable × ABI) lives in
`docs/api/compatibility.md` and is generated, not hand-maintained.

**API stability commitment**, stated in the README: `0.x` = the API may change, with migration
notes each minor; `1.0` = no breaking changes to the public API or the ability to read format
versions ≥ the one shipped with 1.0, for the life of the 1.x line. Nothing is marked `1.0` until
the benchmark suite, the fault-injection suite and at least three SDKs are green — a 1.0 that has
to be retracted costs more trust than a long 0.x.

## 10.3 Security

Threat model, written honestly: the adversary is **not** a remote attacker (there is no network).
It is (a) corruption from crashes and flaky flash, (b) a malicious or malformed *database file*
handed to the app, and (c) another app or a person with the device reading data at rest.

| Concern | Position in v1 |
|---|---|
| **Malformed/hostile database file** | Treated as fully untrusted input. Every decoder is bounds-checked, never allocates on an unvalidated length, and is continuously fuzzed. This is the highest-value security work in the project. |
| **Path traversal** | Collection names validated against a strict charset before becoming path components; the `Storage` impl additionally rejects any resolved path escaping the db root. Defence in depth, because one of these will be bypassed someday. |
| **Partial writes / torn state** | WAL + dual-slot manifest + per-block CRC32C (§5). |
| **Silent corruption (bit rot)** | CRC on every block, verified on read; `verify(Full)` for a deep scan. |
| **Integer overflow in offset math** | `checked_*` arithmetic in all format code; overflow checks enabled in release for `vdb-format`. |
| **Zip-bomb-style resource exhaustion** | Every allocation derived from file contents is capped by the actual file size and by the §7.5 limits. |
| **Encryption at rest** | **Not implemented in v1, and the README says so in exactly those words.** Designed for: a `BlockCodec` trait (`encode`/`decode` per block, with room for an authentication tag) sits between persistence and storage; AES-256-GCM with a per-block nonce derived from `(file_id, block_no, generation)` is the intended v0.5 implementation. Keys never enter the core — the SDK obtains them from the platform keystore (Android Keystore, iOS Keychain/Secure Enclave, DPAPI, libsecret) and passes bytes, which are `Zeroize`d. |
| **Secure deletion** | Genuinely not achievable on flash/COW filesystems: TRIM, wear levelling and APFS snapshots mean overwriting a file does not erase the data. Documented as a limitation. `compact()` removes deleted records from live files; real erasure requires full-disk encryption plus key destruction, which is the platform's job. Promising more than this would be dishonest. |
| **Sensitive data in embeddings** | Worth stating loudly: embeddings are **not** anonymized. Inversion attacks can reconstruct substantial portions of the source text from a vector. Treat a vector database as containing the underlying content, apply the same retention and access rules, and do not assume "it's just numbers". |
| **Logging** | The library never logs document content, ids, metadata values, or vectors. Errors reference field *names* and sizes, never values. An injected `Observer` gives the host app control over everything emitted. |
| **Permissions** | Storage-permission and Data-Protection behaviour is per-platform (§9.3, §9.4); failures surface as typed `StorageError::PermissionDenied`, never as a generic I/O error. |
| **Vulnerability handling** | `SECURITY.md` with a private reporting channel (GitHub Security Advisories), a 90-day disclosure policy, and a commitment to patch releases for the current minor and the previous one. |

## 10.4 Performance & benchmarking

**No performance claim ships without a reproducible benchmark.** (Rule 15, and it is the rule most
often broken by projects in this space.)

`benchmarks/` contains a harness that emits machine-readable JSON, committed per release into
`benchmarks/results/`, so trends are visible in git history.

**Workloads**

| Benchmark | Parameters |
|---|---|
| Insert throughput | 1k / 10k / 100k / 1M docs; single vs batch(1k); durability Full/Batch/Relaxed |
| Search latency | p50/p95/p99 at k = 1/10/100, n = 1k/10k/100k/1M, dim = 128/384/768/1536 |
| Filtered search | selectivity 0.1% / 1% / 10% / 50% / 100% |
| Cold start | open + index load time vs collection size |
| Warm reopen | second open, page cache warm |
| Recovery | reopen after a simulated kill with a full WAL |
| Memory | peak RSS during ingest, steady-state during search, mmap vs buffered |
| Storage | bytes on disk per document vs raw vector bytes (the amplification factor) |
| Index build | flat snapshot; later HNSW build time and recall@10 |
| Compaction | throughput and space reclaimed at various dead ratios |

**Datasets:** synthetic (seeded Gaussian and clustered, so anyone can reproduce without a download)
plus standard sets (SIFT-1M, GloVe-100, a 768-dim text-embedding sample) fetched by script, with
recall measured against exact ground truth.

**Where they run:** CI on a fixed self-hosted x86_64 runner for trend tracking, plus a documented
manual procedure on real devices (a mid-range Android phone, a recent iPhone, an M-series Mac) for
release notes. Mobile numbers must come from mobile hardware; extrapolating from a desktop x86 run
is how libraries end up with published numbers nobody can reproduce.

**Reported honestly:** every published number carries device, OS, dataset, dimension, count, metric,
durability mode, build profile, and whether it is p50 or mean. Comparisons against other databases,
if made at all, use their recommended configuration and link a reproduction script.
