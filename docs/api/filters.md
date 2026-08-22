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
costs a metadata decode rather than a distance computation. That is the right trade for a flat
scan: the decode is cheaper than the dot product at any realistic dimension, and the filter is
tested before the expensive part.

Two optimisations are designed for and not yet built, because neither helps a flat scan:

- **A row bitmap** built before the scan, for a very selective filter. It saves memory traffic
  for an approximate index that would otherwise traverse into regions it must then discard.
- **Secondary field indexes**, so a filter over an indexed field never decodes metadata at all.
  `Filter::referenced_fields()` exists to feed that decision.

`SearchStats` reports `considered` and `skipped`, which together tell you a filter's selectivity —
and therefore whether a scan is doing far more work than its results justify.
