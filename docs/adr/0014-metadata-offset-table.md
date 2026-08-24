# ADR-0014: A field offset table in maps of eight fields or more

- **Status**: Accepted
- **Date**: 2026-08-22
- **Format version**: introduced in v2; v1 remains readable

## Context

`docs/api/filters.md` measured a filtered search and found that walking a metadata record's
sorted keys costs roughly 22 ns per key skipped. Metadata keys are stored in order, so a filter
naming the alphabetically-last field of a record pays for every field before it. That document
predicted a field offset table would help and deferred building one until it was measured.

The lookup is on the hot path: it runs once per candidate row of every filtered scan.

## Decision

A map encoded with eight or more entries is written under a new tag, `MAP_INDEXED` (9), carrying
a fixed-width table of `u16` offsets — one per entry, relative to the first key — between the
count and the entries themselves. `find_path` binary-searches the table; below eight entries the
encoding and the linear walk are unchanged.

The threshold is a constant, not a decision about the data. That is what keeps the encoding a
pure function of the value, so the canonical-form rule still holds within a format version: one
logical value still has exactly one byte representation.

Offsets are `u16` to keep the table at two bytes a field. A map whose entries exceed 64 KiB is
written plain; the reader accepts both tags, so correctness never depends on the size.

The tolerance runs one way only. A wide map under the plain tag is what a v1 file contains, so
the reader takes it and re-encoding upgrades it. A map *below* the threshold carrying a table is
an encoding no version has ever written, and the reader rejects it — accepting it would give a
small map two spellings, which is the canonical-form rule gone. That asymmetry was found the
hard way: see the `[9, 0]` entry in the changelog.

## Consequences

Measured on 5,000 documents at 128 dimensions, comparing the two encodings of the same corpus.
The scalar-kernel control moved by 0–1% between runs, so these differences are real:

| fields | best-case key | worst-case key | average |
|---|---|---|---|
| 8 | 198 → 291 µs (**1.47× slower**) | 789 → 272 µs (**2.9× faster**) | **1.75× faster** |
| 12 | 205 → 289 µs (1.41× slower) | 1090 → 281 µs (3.9× faster) | **2.27× faster** |
| 16 | 206 → 317 µs (1.54× slower) | 1385 → 296 µs (4.7× faster) | **2.60× faster** |

**The table makes the best case worse.** This was not in the prediction and is the honest cost of
the decision. A walk finds the first key on its first comparison; a binary search always pays
`log2(n)` probes, each a random access rather than a sequential read. The table converts a cost
that varies from 1 to n into a flat cost of about `log2(n)` — a loss when the answer was going to
be found immediately, a large win otherwise.

Lookup cost also becomes *independent of which field is named*, which removes a sharp
performance cliff that depended on nothing but alphabetical accident.

The threshold pays from the count at which it activates: 1.75× average at eight fields. Four
fields measures near break-even, so eight is deliberately conservative.

Cost in bytes: two per field, on records of eight fields or more.

## Alternatives considered

- **Varint offsets.** Smaller, but variable width means the table cannot be indexed, which is the
  one thing it exists for.
- **Storing keys in the table.** Removes the random access into the entries on each probe, at the
  cost of duplicating every key. Rejected: it roughly doubles metadata size for key-heavy records.
- **A lower threshold.** Not supported by the measurement; four fields is near break-even and
  below that the table is pure cost.
- **Secondary field indexes.** Still the right answer for removing the lookup rather than
  shortening it, and still future work. `Filter::referenced_fields()` exists to feed it.

## Compatibility

`FORMAT_VERSION` moves to 2; `MIN_READABLE_VERSION` stays 1. A v2 build reads v1 databases —
pinned by `this_build_can_read_every_v1_fixture`, which reads the untouched `testdata/v1/`
fixtures committed before this change. A v1 build cannot read v2 files and will say so, because
every file header carries its version.

The C ABI is untouched: `vdb_abi_version()` stays 1 while `vdb_format_version()` returns 2. The
two numbers are deliberately independent, and this change is the first demonstration of that.
