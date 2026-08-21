# 6. Index & Search Architecture

## 6.1 The abstraction, and what it deliberately does not assume

`VectorIndex` (§4.4) is the only thing the engine knows about indexing. It makes exactly three
assumptions, all of which hold for flat, IVF, HNSW, PQ and DiskANN:

1. An index maps `RowId → vector` and can produce approximate-or-exact nearest neighbours.
2. It can be built from a `VectorSource` stream and snapshotted to / restored from opaque blocks.
3. It respects a `LiveSet` and an optional row predicate at query time.

It does **not** assume the index fits in memory, that `add` is cheap, that deletes are supported
natively, or that results are exact — `is_exact()` is surfaced in `SearchStats` so callers can tell
whether they got ground truth.

Indexes register themselves in an `IndexRegistry` keyed by `IndexKind`, so adding HNSW later
touches: the new crate, one registry entry, one `IndexSpec` variant, one `SearchParams` variant.
No change to the engine, the format, or any SDK. That is the test of whether this abstraction is
real, and it is worth writing a throwaway second index early (even a bad one, e.g. random
projection LSH) purely to prove the seam holds before it calcifies.

## 6.2 `FlatIndex` (v1)

Exact brute force. Correctness baseline for everything that follows, and genuinely the right choice
below ~100k vectors on device.

- Vectors are **not** copied into the index. The index holds `(segment_id, mmap_or_buffer)` handles
  and scans the `.vec` blocks directly, so index memory is `O(rows)` for the norm cache, not
  `O(rows*dim)`.
- Cosine: pre-computed inverse L2 norms are stored per row in the snapshot (`4 B/row`), turning
  cosine into `dot(q,v) * inv_norm_v * inv_norm_q`. Raw values stay un-normalized so `get()`
  returns exactly what was inserted — normalizing on write would be a silent, lossy data mutation.
- Kernels: `dot_f32`, `l2sq_f32`, dispatched at runtime to AVX2+FMA (x86_64), NEON (aarch64),
  `simd128` (wasm), or a scalar fallback. Every SIMD kernel is differential-tested against the
  scalar one on random inputs with a tight epsilon.
- Selection: a bounded binary max-heap of size `k` (`O(n log k)`), switching to a partial sort when
  `k > n/8`. Ties broken by `DocId` ascending inside the heap comparator, not in a post-sort.
- Filter interaction is planned, not fixed:
  - **selectivity < ~5%** → evaluate the filter first into a `RowId` bitmap, then score only those
    rows (skipping most of the memory traffic);
  - **otherwise** → score while streaming and test the predicate per candidate before heap
    insertion, which keeps the scan sequential.
  Selectivity is estimated from cheap per-segment metadata statistics; a wrong estimate costs
  performance, never correctness.
- `save`/`load` write only the norm cache and row map. The snapshot is a cache: if it is missing or
  its CRC fails, the index is rebuilt from the segments and the open succeeds with a warning. An
  index must never be the reason a database fails to open — it is derived data.

Cost model: one scan of `n * dim * 4` bytes. At 100k × 768 that is ~307 MB, roughly 15–40 ms on
modern mobile hardware, memory-bandwidth-bound. Do not quote these numbers publicly until
`benchmarks/` produces them on real devices (rule 15).

## 6.3 Determinism and floating point — the subtle part

"Deterministic behaviour" is a stated requirement, and naive SIMD breaks it. Different lane counts
sum in a different order, and float addition is not associative, so AVX2 and NEON can return scores
that differ in the last few ULPs. When two documents are near-tied, that reorders results across
architectures — a genuinely confusing bug for someone comparing iOS and Android output.

The position taken here:

1. **Ranking is deterministic everywhere** because ties break on `DocId`, and near-ties are made
   deterministic by comparing scores with a fixed relative epsilon before falling back to the id.
2. **Scores are bit-identical for a given target triple and build**, and may differ by a few ULPs
   across architectures. This is documented explicitly rather than hidden.
3. A `deterministic-kernels` cargo feature forces a fixed chunked pairwise summation (chunk size
   8, identical schedule in every backend), making scores bit-identical across architectures at
   roughly 10–20% throughput cost. Off by default, on in the cross-platform conformance tests.

Deciding this up front is much cheaper than discovering it when a user files "same data, different
top-10 on iPhone vs Pixel".

## 6.4 HNSW (Phase 3) — the constraints it must respect

Not implemented in v1, but the seams must accommodate it:

- Parameters `M`, `ef_construction`, `ef_search` in `IndexSpec`/`SearchParams`.
- Build is single-threaded-deterministic by default with a **seeded** RNG stored in the snapshot,
  so a rebuild reproduces the same graph. Optional parallel build is explicitly non-deterministic
  and must be labelled as such.
- Deletes: HNSW cannot truly remove nodes; the `LiveSet` filter handles them, with a documented
  degradation as the dead ratio grows and a rebuild triggered by `compact()`.
- Filtered search needs oversampling (`ef_search = max(ef, k/selectivity)`) plus a fallback to a
  flat scan when selectivity is below a threshold — an unrestricted filtered HNSW search can
  otherwise return far fewer than `k` results, which reads as a correctness bug to users.
- Graph snapshots must be mmap-able and loadable without a full deserialize, or mobile startup
  time regresses badly.
- Recall must be measured against `FlatIndex` ground truth in CI (§8.6) and gated — an ANN index
  without a recall gate is a random-result generator waiting to happen.

## 6.5 Roadmap beyond HNSW

`IVF` (cheap build, good for 1M+), `PQ`/`SQ` quantization (the real answer to mobile memory: int8
scalar quantization cuts 768-dim vectors from 3 KB to 768 B with ~1% recall loss), binary vectors
with Hamming, sparse vectors for hybrid retrieval, and DiskANN much later. `VectorDType` and
`IndexSpec` already have the room for all of these; none require a public API break.
