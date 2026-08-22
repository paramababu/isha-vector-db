# Metadata Filters

A small typed expression tree, not a query language. There is no parser and no SQL — that is a
[deliberate non-goal](../architecture/01-scope.md#14-explicit-non-goals-v1), because a query
language is a large, permanently-supported surface and the workload here is "narrow a vector
search by a handful of fields".

```rust
let tools_under_50 = Filter::eq("category", Value::Str("tools".into()))
    .and(Filter::lt("price", Value::F64(50.0)));

let results = collection.search(
    &SearchRequest::new(VectorView::f32(&query), 10).with_filter(&tools_under_50),
)?;
```

## Filters cannot fail

Evaluation is **total**. A filter is validated once when it is built — node count and depth,
nothing else — and after that every combination of filter and document yields `true` or `false`.
Comparing a string to a number is `false`. Descending into a scalar is `false`. A field no
document has is simply absent.

This is worth more than it sounds. A filter that can fail at evaluation time turns one odd
document into a failed search across an entire collection, and the caller cannot fix it — their
query was fine. Total semantics make a filter's behaviour a property of the filter rather than of
whatever data it happens to meet.

## Field paths

A field is a `.`-separated path. `user.plan` finds `plan` inside the map stored at `user`. A path
that cannot be resolved — a missing key, or an attempt to descend into a scalar — is **absent**,
which is different from present-and-null. `Exists` is the operator that tells them apart.

## Operators

| Operator | Matches when |
|---|---|
| `Eq(field, value)` | the field equals the value |
| `Ne(field, value)` | the exact negation of `Eq` |
| `Gt` `Gte` `Lt` `Lte` | the field orders that way against the value |
| `In(field, values)` | the field equals one of the values |
| `Nin(field, values)` | the field equals none of them |
| `Exists(field)` | the field is present, including an explicit null |
| `IsNull(field)` | the field is absent, or present and null |
| `StartsWith(field, prefix)` | the field is a string with that prefix |
| `Contains(field, value)` | the field is an **array** containing that value |
| `And` `Or` `Not` | the usual, over child filters |

`Contains` is array membership only. Substring matching is a different operation, and giving both
the same name makes both harder to reason about.

## Type rules

**Numbers compare across the integer/float boundary.** `Eq("count", F64(3.0))` matches a document
holding `I64(3)`. A database that disagreed would be technically defensible and practically
infuriating.

**Strings, booleans and byte strings order naturally.** Lexicographic, `false < true`, and
lexicographic respectively.

**Null, arrays and maps have no ordering.** Any `Gt`/`Gte`/`Lt`/`Lte` against them is `false`.

**Arrays and maps compare structurally for equality.** Element by element, in order.

**Mismatched types are never equal and never ordered.** Both are `false`.

## The rules that surprise people

These are the ones worth reading twice. Each is pinned by a test.

**An absent field equals null.** `Eq(field, Null)` matches a document that has no such field.
That is what people write it for; use `Exists` when the distinction matters.

**`Ne` is the exact negation of `Eq`, so it matches absent fields.** `Ne("price", F64(10))` is
true for a document with no price. If you want "has a price, and it is not 10", write
`Exists("price").and(Ne("price", F64(10)))`.

**`Gt` and `Lte` are not negations of each other.** Where no ordering is defined — an absent
field, a type mismatch — *both* are `false`. This is the practical effect of SQL's three-valued
logic without the third value leaking into the API. `!Gt(...)` and `Lte(...)` are therefore
different filters.

**An empty `And` matches everything; an empty `Or` matches nothing.** They are the identity
elements of their operations, so `all(vec![])` behaves the same as passing no filter at all —
which is what code that builds filters programmatically needs.

## Limits

| Limit | Default |
|---|---|
| Nodes in one filter | 256 |
| Nesting depth | 32 |

Both exist because evaluation recurses, and unbounded recursion over a caller-supplied tree
overflows the stack — which aborts the process rather than returning an error anyone can handle.
Chained `.and()` / `.or()` calls flatten into a single node rather than nesting, so a long chain
of conjuncts does not approach the depth limit.

## What filtering costs

A filtered scan reads each candidate's metadata before scoring it, so a filtered-out document
costs a metadata decode instead of a distance computation.

Only the fields a filter actually names are decoded. A candidate's metadata is left as bytes and
the named paths are found by walking the encoded map, skipping over everything else — so a filter
reading one field out of six pays for one.

That was not always true, and the benchmarks are why it changed. Filtering originally decoded
each candidate's entire metadata, which cost **1.5× to 4.5× more than the distance computation
the filter was avoiding** — a filter made a search slower, not faster. With the lazy lookup, at
50,000 documents × 384 dimensions, a filter passing 10% of documents costs 7.3 ms against 6.2 ms
unfiltered: roughly break-even.

It is still not *faster* than scanning everything, despite scoring a tenth as many rows, because
the per-candidate lookup costs about what the skipped distance saves.

An earlier version of this document asserted that a decode must be cheaper than a distance
computation, on grounds anyone would find plausible. Measuring it showed otherwise. The history
is left here rather than quietly deleted, because the reasoning is the kind that recurs.

### Where the time actually goes

A selectivity sweep separates the two costs, which move in opposite directions — a filter removes
distance computations and adds a lookup. At 50,000 documents × 384 dimensions:

| filter | rows scored | rows skipped | p50 |
|---|---|---|---|
| none | 50,000 | 0 | 3.30 ms |
| matches nothing | 0 | 50,000 | 3.03 ms |
| matches 10% | 5,000 | 45,000 | 3.99 ms |
| matches everything | 50,000 | 0 | 6.22 ms |

The costs are additive, and near enough equal: **a metadata lookup costs about what a 384-dimension
SIMD distance costs.** That is why filtering cannot currently be faster than not filtering — you
pay a lookup for every row you skip.

Walking the sorted keys is a real part of it. Metadata stores keys in order, so a filter naming
the alphabetically-first field finds it on the first comparison and one naming the last field
walks past the others:

| filter names | p50 |
|---|---|
| the first key of three | 3.85 ms |
| the last key of three | 5.99 ms |

Two skipped keys cost ~2.1 ms across 50,000 rows, about **22 ns per key walked past**. A document
with ten metadata fields and a filter on the last of them would spend roughly four times a
distance computation on the lookup alone.

That measurement decided between the two candidate fixes, which is why it was worth taking
before building either.

**The field offset table was built, and shipped in format v2** ([ADR-0014](../adr/0014-metadata-offset-table.md)).
A map of eight fields or more now carries a table of `u16` offsets that `find_path`
binary-searches instead of walking. Records below eight fields — including the three-field ones
measured above — are unchanged.

It did not do quite what this document predicted. Measured on the same machine, comparing both
encodings of a sixteen-field corpus with the scalar kernel as a control:

| filter names | walk | offset table | |
|---|---|---|---|
| the first key of sixteen | 206 µs | 317 µs | **1.54× slower** |
| the last key of sixteen | 1385 µs | 296 µs | **4.7× faster** |

The prediction that a seek beats a walk was right about the worst case and wrong to assume there
was no cost. A binary search always pays `log2(n)` probes, so it loses to a walk that was going to
find its answer on the first comparison. What the table really buys is that **lookup cost no
longer depends on which field the filter names** — the cliff between the first key and the last
one is gone, and the average across fields improves 1.75× at eight fields and 2.6× at sixteen.

- **Secondary field indexes** would remove the lookup entirely rather than shortening it, which
  is the only thing that can make a filtered search *faster* than an unfiltered one.
  `Filter::referenced_fields()` exists to feed that decision. Still future work.

One incidental observation worth keeping: the filtered timings are far more stable across runs
than the unfiltered ones (5.98–6.00 ms against 3.30–5.24 ms on the same machine). The scan is
memory-bandwidth-bound and therefore thermally sensitive; the lookup is branch- and
compute-bound and is not.

Filter for correctness. Filtering will not currently make a search faster, though it does not
make it slower, and on wide metadata it no longer matters which field you name.

`SearchStats` reports `considered` and `skipped`, which together give a filter's selectivity, and
therefore how much work a scan is doing for the results it returns.
