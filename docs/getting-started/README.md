# Getting started

Pick your platform. Each page is self-contained — install, first program, the operations you will
actually use, and the things that go wrong on that platform specifically. You should not need to
read any of the others.

| Platform | Page | Status |
|---|---|---|
| **Python** | [python.md](python.md) | Working, 19 tests |
| **Node.js** | [nodejs.md](nodejs.md) | Working, 26 tests |
| **React / web browsers** | [web.md](web.md) | Working, verified in a browser |
| **React Native** | [react-native.md](react-native.md) | Native logic tested; packaging unverified |
| **Android (Kotlin/Java)** | [android.md](android.md) | Working, 53 checks |
| **iOS (Swift)** | [ios.md](ios.md) | Working, 26 tests |
| **Rust** | [rust.md](rust.md) | Working, 548 tests |
| **C / C++** | [c.md](c.md) | Working, frozen ABI |
| Flutter | — | Not built |

## What this is, in one paragraph

An embedded vector database. It runs inside your process against files on a disk, like SQLite —
there is no server to start, no port, no container, and no network call in the hot path. You give
it vectors with string ids, and it gives you the nearest ones back. It is built for the case where
the data belongs on the device it is used from: a phone, a laptop, a browser tab.

## What you need before any of this

**Embeddings.** This database stores and searches vectors; it does not produce them. Whatever you
use to turn text or images into vectors — a local model, an API — is upstream of everything here,
and the only thing that matters to vdb is that every vector in a collection has the same length.

## Concepts, once

These are the same on every platform, so they are here rather than repeated eight times.

- **Database** — a directory. One process may have it open at a time; a second attempt fails
  rather than corrupting it.
- **Collection** — a named set of vectors that all have the same dimension and one metric. Most
  applications need one.
- **Document** — a string id, a vector, and optional metadata.
- **Metric** — `Cosine` (direction, the usual choice for text embeddings), `L2` (straight-line
  distance), or `Dot` (inner product, which rewards longer vectors).
- **Score** — always higher-is-better, whatever the metric. Results come back best first, and ties
  break on ascending id so the same query always returns the same order.
- **Flush** — writes are buffered. `flush()` puts them on disk. Closing flushes too.

## The two mistakes everyone makes first

**Not closing the database.** An open database holds a lock. If your process exits without
closing, the next run may find the lock held. Every binding has a scope-based way to avoid this —
`with` in Python, `try`/`finally` or `use` elsewhere.

**Expecting a filtered search to be faster.** A filter narrows *results*, not work. The engine
still has to consider the documents to know which ones match. See
[the filter documentation](../api/filters.md).
