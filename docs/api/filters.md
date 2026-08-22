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

**Today that is a bad trade, and the benchmarks say so.** A filter passing 10% of documents makes
a search *slower*, not faster, despite scoring one tenth as many vectors:

| corpus | unfiltered p50 | filtered p50 | ratio |
|---|---|---|---|
| 5,000 docs × 128 dims | 0.37 ms | 1.67 ms | 4.5× slower |
| 50,000 docs × 384 dims | 12.5 ms | 18.9 ms | 1.5× slower |

The decode dominates: every candidate allocates a fresh map and a string per field. The ratio
does improve with dimension — the distance computation the filter avoids grows while the decode
does not — but it has not crossed over even at 384 dimensions.

An earlier version of this document asserted the opposite, on the reasonable-sounding grounds
that a decode must be cheaper than a distance computation. Measuring it showed that is only true
at high dimensions, and by a smaller margin than expected. The claim is corrected here rather
than quietly deleted, because the reasoning that produced it is the kind anyone would find
plausible.

The fix is known and not yet built, and is now a measured priority rather than a speculative one:

- **Decode only the fields a filter references.** `Filter::referenced_fields()` already reports
  them; the metadata record needs a field offset table so they can be reached without decoding
  the rest. This is the change that closes most of the gap.
- **A row bitmap** built before the scan, for a very selective filter, so an approximate index
  does not traverse into regions it must then discard.
- **Secondary field indexes**, so a filter over an indexed field never touches metadata at all.

Until then: a filter is for correctness — getting the right documents — not for speed. If you
are filtering to make a search *faster*, it currently will not.

`SearchStats` reports `considered` and `skipped`, which together give a filter's selectivity, and
therefore how much work a scan is doing for the results it returns.
