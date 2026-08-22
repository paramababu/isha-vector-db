# Benchmarks

```bash
cargo run --release -p vdb-bench -- --standard
cargo run --release -p vdb-bench -- --quick --json results/local.json
```

Rule 15 of the [engineering rules](../CONTRIBUTING.md) is "benchmark before making performance
claims". This directory is what makes that rule followable: results are committed as JSON, so a
regression shows up as a diff in git history rather than as a vague sense that things got slower.

## Method

**Scales.** `--quick` (5,000 documents × 128 dims) is for a pull request; `--standard`
(50,000 × 384) is a realistic on-device corpus with a sentence-embedding dimension;
`--large` (250,000 × 768) is where a flat scan starts to hurt, which is the number that says when
an approximate index becomes worth building.

**Data is clustered, not uniform.** Uniform random vectors in high dimensions are all nearly
equidistant, so every ranking becomes a coin flip and real differences in the scan disappear into
noise. The corpus is sixteen Gaussian clusters, from a seeded generator, so two runs measure the
same work.

**Queries are perturbed corpus vectors**, not random points — that is what a nearest-neighbour
lookup actually looks like.

**Percentiles are nearest-rank inclusive**: `index = ceil(p/100 × n) − 1`. Stated because the
conventions genuinely differ and results computed under different ones cannot be compared.

**A short warm-up runs first** (5% of iterations, capped at 50) so the first samples are not
measuring page faults, and the count is recorded in the output because a warm-up changes what the
number means.

**Debug builds refuse to write JSON.** A debug build is roughly an order of magnitude slower and
not uniformly so, and a number that lands in a README is very hard to un-publish.

## What is not measured yet

- **Recall**, because there is no approximate index yet. When HNSW lands, every latency figure
  for it must be reported beside its recall, or the comparison is meaningless.
- **Mobile hardware.** Everything here is desktop. Mobile numbers must come from mobile devices;
  extrapolating from a laptop is how libraries end up with published figures nobody can
  reproduce. `docs/architecture/10-ci-security-performance.md` §10.4 has the intended procedure.
- **Concurrency.** Single-threaded throughout.

## Baseline

`results/baseline-standard.json`, on an Apple M-series laptop. **It is a reference point for
detecting change on the same machine, not a claim about performance in general** — a comparison
across different hardware is meaningless, which is why every result records its target.

Headline figures at 50,000 documents × 384 dimensions:

| workload | result |
|---|---|
| insert, one at a time | 35,300/s (p50 11.7 µs) |
| insert, batched 1,000 | 40,400/s |
| search, k=10 (NEON) | p50 2.6 ms, p99 3.3 ms |
| search, k=10 (scalar reference) | p50 12.5 ms |
| search, k=10, 10% filter | p50 18.9 ms — **slower**, see below |
| get by id | p50 750 ns |
| cold open | 33.7 ms |
| recovery, 20k unflushed | 33.8 ms |
| compaction, 70% dead | 290,000 rows/s |
| storage amplification | 1.046× |

## What the baseline says

**The scan is memory-bandwidth-bound, as designed.** 50,000 × 384 dimensions is 76.8 MB per
query. The scalar reference does it in 12.5 ms (≈6 GB/s); the NEON kernels in `vdb-index-flat`
do it in 2.6 ms (≈30 GB/s), a **4.9× speedup** — measured against the committed baseline, which
is the whole reason the baseline was committed first.

Both figures are reported, and both are run every time, because the accelerated kernel is only
trustworthy while it agrees with the reference. The differential tests check that agreement at
every vector length; this checks it is still worth having.

**k barely matters.** k=1 and k=100 differ by 1%, because the heap is noise next to reading 76 MB.
Selection is not where the time goes.

**Filtering makes search slower, not faster.** Decoding each candidate's metadata costs more than
the distance computation it avoids. This contradicts what the filter documentation originally
claimed; the claim has been corrected and the fix — decoding only the fields a filter references —
is now a measured priority rather than a speculative one. See
[docs/api/filters.md](../docs/api/filters.md#what-filtering-costs).

**Batching helps less than expected** — 40,400/s against 35,300/s. With `Durability::Batch` a
single insert already avoids an fsync, so a batch mainly saves per-operation validation and log
framing. Batching's real value is atomicity, not throughput, and the documentation should say so.

**Storage overhead is 4.6%** over the raw vectors, at 1,606 bytes per document for a 1,536-byte
vector. The columnar layout is doing its job.
