# ADR-0015: A graph index, chosen at open time and held in memory

- **Status**: Accepted
- **Date**: 2026-08-22
- **Format version**: unchanged (v2). The graph is not persisted.

## Context

The exact scan compares the query against every live vector. With the SIMD kernels that is
single-digit milliseconds at 50,000 vectors, and it is the right answer for most embedded
corpora: trivially correct, cache-friendly, nothing to build, nothing to invalidate.

It is linear, and no kernel work changes that. Past a few hundred thousand vectors an on-device
search stops feeling instant, and the roadmap has always had a graph index as Phase 3 — held back
deliberately until the exact path was correct and benchmarked, so that the comparison would mean
something.

## Decision

`vdb-index-hnsw` implements `VectorIndex` with a hierarchical navigable small world graph. It is
supplied to `Database::open_with_index`, exactly as `vdb-index-flat` is, so `vdb-core` still knows
nothing about it.

`VectorIndex` gained one defaulted method, `prepare(source, metric)`, called before each search.
An exact scan does nothing there. A graph index builds itself. The alternative — building inside
`search` — makes a `&self` method that callers reasonably expect to be cheap unpredictably
expensive, and buries a multi-second cost inside a query's latency.

Three properties are not negotiable and are tested as such:

- **Determinism.** Levels come from hashing each row's identity with the parameter seed rather
  than from a running random number generator, candidate ordering breaks ties on node index, and
  rows are inserted in the source's stable order. The same data builds the same graph.
- **Approximate is declared.** `is_exact()` returns false and the engine reports it. An
  approximate result presented as exact is a correctness bug wearing a performance costume.
- **Filters and deletes are exact.** The graph traverses *through* rows a filter rejects but
  returns only rows it accepts. When it cannot find `top_k` acceptable rows, the exact scan takes
  over — see below.

## Consequences

Measured on this machine, clustered corpora, `ef_search` 64, against the NEON flat scan on the
same data:

| corpus | flat | hnsw | | recall@10 |
|---|---|---|---|---|
| 5,000 × 128 | 160 µs | 46 µs | **3.5× faster** | 1.000 |
| 50,000 × 384 | 6.11 ms | 476 µs | **12.8× faster** | 0.974 |

Recall against `ef_search`, 10,000 × 128: 0.905 at ef 16, 0.960 at 32, **0.992 at 64**, 1.000 at
128. The knob behaves, and the default is on the flat part of the curve.

**Building is slow: 95 seconds for 50,000 × 384.** That is the main cost of this decision and it
is not hidden — the benchmark reports it separately rather than letting it inflate a first
query. It is single-threaded and unoptimised beyond the two fixes measurement demanded (see
below). A comparable C++ implementation is perhaps three times faster.

**The graph was not persisted** when this was written: it lived in memory and was rebuilt on
every reopen, a roughly eighty-second wait before the first query on a 50,000-vector collection.
That is **superseded by [ADR-0016](0016-index-snapshots.md)**, which persists it and restores in
40.9 ms. The format bump anticipated here turned out to be unnecessary, because a snapshot is a
cache rather than data.

`IndexSpec` in the catalog still records only `Flat`, because nothing about the collection's
on-disk form has changed.

## What measurement changed

Two performance fixes were made only because the benchmark demanded them, and both are recorded
because the numbers are the justification:

- Scoring went through `vdb-core`'s scalar `Scorer`, while the flat scan it was being compared
  against used NEON. Routing graph distances through `vdb-index-flat`'s kernels took the 5,000-
  vector build from 4.49s to 1.56s.
- `search_layer` allocated and zeroed a `visited` array on every call — once per node per layer
  while building, once per query afterwards. Generation stamps took the 50,000-vector build from
  185s to 95s and the query from 913 µs to 476 µs.

## The bug worth recording

Pruning a node's neighbour list to the closest `max` is the obvious implementation and it
fragments the graph. On clustered data a node's nearest neighbours are all in its own cluster, so
the links that bridge clusters are precisely the ones truncation discards. The graph looked
healthy by every count available — full degree on every node, the right number of layers, the
right population per layer — while only **167 of 2,000 nodes were reachable** from the entry
point, and the sole outward symptom was recall of 0.46 that did not improve as the beam widened.

Pruning with the same diversity heuristic used for selection fixes it. `every_node_is_reachable_
from_the_entry_point` is now a permanent test, because no other measurement caught this.

## Alternatives considered

- **IVF / clustering.** Simpler to build and much cheaper to update, with worse recall at the same
  latency. Worth revisiting specifically because its build cost is a fraction of this one.
- **Building inside `search`.** Rejected above.
- **Persisting the graph now.** The right end state, and a format change; keeping it separate
  keeps this change reviewable and lets the format bump carry a migration test of its own.
- **Skipping filtered rows during traversal.** Faster and wrong: it disconnects the graph exactly
  when the filter is most selective.
