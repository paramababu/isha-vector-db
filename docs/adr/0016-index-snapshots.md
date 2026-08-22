# ADR-0016: An index snapshot is a cache, not data

- **Status**: Accepted
- **Date**: 2026-08-22
- **Format version**: unchanged (v2), and this decision is why

## Context

[ADR-0015](0015-hnsw-index.md) shipped the graph index with a limitation stated plainly: the
graph lives in memory and is rebuilt whenever the row count, dimension or metric changes —
including on every reopen. At 50,000 × 384 that is roughly eighty seconds before the first query
can run, which makes a graph index unusable for anything a person waits for. Building once is a
one-time cost only if it survives a restart.

The manifest has reserved an `index_snapshot` slot since the first design, and `FileKind::Index`
and `layout::index_file` have existed unused since then.

## Decision

`VectorIndex::prepare` takes an `IndexSnapshots` — a two-method trait, `load` and `store` —
implemented by the engine over storage. An index that has nothing worth keeping ignores it; the
graph index writes its structure after building and reads it back instead of rebuilding.

**The governing decision is that a snapshot is a cache, not data.** Everything in it can be
recomputed from the segments, which are the real data. That single choice removes most of the
work this feature would otherwise need:

- **No format version bump.** Nothing existing changed, and an older build simply does not find a
  snapshot it understands.
- **No migration.** The snapshot carries its own layout version, and a mismatch discards it.
- **No crash protection.** One file per index kind, overwritten in place, no rename dance and no
  generation numbers. A half-written snapshot fails its checksum, which is the same outcome as
  not having one.
- **No corruption reporting.** Damage is not an error to surface; it is a rebuild.

The rule that makes this safe is that `load` must never fail a *search*. Anything unreadable is
`Ok(None)`.

What is stored is the graph's structure: levels, neighbour lists, the entry point, and the row
each node stands for. Vectors are not — they are already in the segments, and restoring walks the
source anyway to confirm the graph still describes it.

## Validation

Restoring checks the metric, the dimension, the node count, the build parameters, that every
stored row matches the source row in the same position, and that every neighbour index is in
range. That is expensive next to trusting the file and negligible next to rebuilding.

Parameters are checked because a graph built with a different `m` or seed is structurally
readable but is not the graph this index was asked for: its degree and level distribution belong
to someone else's configuration.

## Consequences

Measured on this machine:

| corpus | build | reopen | |
|---|---|---|---|
| 5,000 × 128 | 1.13 s | 1.70 ms | **665× faster** |
| 50,000 × 384 | 80.1 s | 40.9 ms | **1,958× faster** |

The limitation ADR-0015 recorded is gone. Search latency and recall are unchanged — the restored
graph is the built graph, and a test asserts the two give identical answers.

The remaining cost is that a write invalidates the whole graph: any change to the row count
forces a full rebuild and a new snapshot. Incremental insertion is the obvious next improvement
and is not here.

## How this is tested

The risk is not that restoring fails; it is that it silently succeeds with a graph that no longer
describes the data, which would be worse than the slow rebuild it replaced. So the tests are
mostly about being wrong:

- Restored and built graphs must give identical answers, and restoring must not rewrite the
  snapshot it just read — without that second assertion the whole suite would pass with restore
  permanently broken.
- Rejection is asserted for a changed corpus, a changed row count, a different metric, and
  different build parameters.
- Eight named corruption modes, and **every single-byte flip across the entire snapshot**, must
  produce a rebuild and identical answers — never a panic, never a different result. Removing one
  bounds check on neighbour indices makes that test fail with a panic, which is how the check
  was confirmed to be load-bearing rather than decorative.

`snapshot.rs` is the one module in the crate that reads bytes it did not write, and it carries no
`indexing_slicing` allowance for exactly that reason.
