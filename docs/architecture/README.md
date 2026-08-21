# Architecture Documentation

Read in order. Each document is self-contained enough to review on its own, and together they are
intended to be sufficient for several developers to implement in parallel without renegotiating
the core.

| # | Document | Covers |
|---|---|---|
| 1 | [Scope](01-scope.md) | Requirements, non-goals, MVP definition |
| 2 | [Technology](02-technology.md) | Language choice and the comparison behind it |
| 3 | [System Design](03-system-design.md) | Layers, crate graph, repo structure, module responsibilities, threading |
| 4 | [Core Interfaces](04-core-interfaces.md) | Public API, data model, search contract, `VectorIndex`, `Storage` |
| 5 | [Storage & Persistence](05-storage-and-persistence.md) | On-disk format, WAL, manifest, recovery, durability, migration |
| 6 | [Index & Search](06-index-and-search.md) | Index abstraction, flat index, determinism, HNSW plan |
| 7 | [Errors, Concurrency, Transactions](07-errors-concurrency-txn.md) | Error taxonomy, snapshot isolation, atomic batches, limits |
| 8 | [Testing](08-testing.md) | Test layers, mandatory cases, fault injection, conformance suites |
| 9 | [Platform Bindings](09-platform-bindings.md) | C ABI, RN, Flutter, Android, iOS, Node, Web/WASM |
| 10 | [CI/CD, Security, Performance](10-ci-security-performance.md) | Workflows, versioning, threat model, benchmark methodology |
| 11 | [Roadmap, Risks, Order](11-roadmap-risks-order.md) | Phases, risk register, step-by-step implementation order |

Decisions are recorded in [`docs/adr/`](../adr/README.md).
