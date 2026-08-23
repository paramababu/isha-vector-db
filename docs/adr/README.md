# Architecture Decision Records

One file per decision, immutable once accepted. Superseding a decision means a *new* ADR that
references the old one — never editing history. Template: `docs/adr/0000-template.md`.

Rule 14 of the engineering rules ("document architectural decisions") is operationalized here: if
a code review argument takes more than one round trip about *why* something is the way it is, the
answer belongs in an ADR.

| # | Decision | Status | Detail |
|---|---|---|---|
| 0001 | Rust for the core engine | Accepted | [02-technology](../architecture/02-technology.md) |
| 0002 | Cargo workspace with per-concern crates; storage/index as separate crates | Accepted | [03-system-design](../architecture/03-system-design.md#33-repository-structure-and-why-it-differs-from-the-sketch-in-the-brief) |
| 0003 | Append-only WAL + immutable segments + dual-slot manifest | Accepted | [05-storage](../architecture/05-storage-and-persistence.md) |
| 0004 | Hand-written on-disk encoding; no third-party codec in the format | Accepted | below |
| 0005 | Four files per segment, columnar `.vec` block | Accepted | [05-storage §5.2](../architecture/05-storage-and-persistence.md#52-directory-layout) |
| 0006 | Single writer, lock-free snapshot readers; no threads in core | Accepted | [07-errors-concurrency-txn §7.2](../architecture/07-errors-concurrency-txn.md#72-concurrency-model) |
| 0007 | Atomic batches instead of interactive transactions in v1 | Accepted | [07 §7.4](../architecture/07-errors-concurrency-txn.md#74-transactions-what-v1-actually-gives-you) |
| 0008 | `score` is always higher-is-better; ties break on DocId | Accepted | [04 §4.3](../architecture/04-core-interfaces.md#43-search) |
| 0009 | One hand-written C ABI as the single interop contract | Accepted | [09 §9.0](../architecture/09-platform-bindings.md#90-the-c-abi-one-contract-six-consumers) |
| 0010 | OPFS primary, IndexedDB fallback, engine in a Worker | Accepted; both built and browser-verified | [09 §9.6](../architecture/09-platform-bindings.md#96-web--wasm), [sdk/web](../../sdk/web/README.md) |
| 0011 | React Native: JSI/TurboModules, New Architecture only; Nitro to be evaluated | Accepted; bridge and JS layer built and tested, JSI glue and packaging unverified | [09 §9.1](../architecture/09-platform-bindings.md#91-react-native), [sdk/react-native](../../sdk/react-native/README.md) |
| 0012 | No encryption in v1; `BlockCodec` seam reserved | Accepted | [10 §10.3](../architecture/10-ci-security-performance.md#103-security) |
| 0013 | Deterministic ranking, architecture-dependent score ULPs, opt-in deterministic kernels | Accepted | [06 §6.3](../architecture/06-index-and-search.md#63-determinism-and-floating-point--the-subtle-part) |
| 0014 | Field offset table in maps of eight fields or more (format v2) | Accepted | [ADR-0014](0014-metadata-offset-table.md) |
| 0015 | HNSW graph index, chosen at open time, held in memory | Accepted | [ADR-0015](0015-hnsw-index.md) |
| 0016 | An index snapshot is a cache, not data | Accepted | [ADR-0016](0016-index-snapshots.md) |

## ADR-0004 in full (it is the one most likely to be questioned)

**Context.** Metadata and manifests need a serialization format. `serde` + `ciborium`/`postcard`/
`bincode` would be less code than writing our own.

**Decision.** `vdb-format` implements its own encoders and decoders. No third-party serialization
crate appears in `vdb-core` or `vdb-format`.

**Reasoning.**
1. The on-disk bytes are a **published, versioned contract with users' data**. A dependency's minor
   release changing its encoding — which is entirely within its rights, and has happened to real
   projects — would silently break every existing database. We must own the bytes.
2. Generic codecs encode what is convenient for the codec, not what is fast to read. We want a
   fixed-stride `f32` block that can be `mmap`'d and scanned with zero decoding; a generic format
   will not give us that.
3. Fuzzing our own ~600 lines is tractable and we can fix what we find immediately.
4. Rule 11: avoid unnecessary dependencies — and a dependency in the layer that owns user data is
   the least necessary kind.

**Cost.** ~600 lines of encoder/decoder plus fuzz targets. Accepted, and the cost is front-loaded
into a phase where it is cheap.

**Note.** `serde` derives *are* allowed on public config/result types behind an optional feature,
for user convenience. That is API sugar, not the storage format, and the distinction is enforced by
`ci-core-purity`.
