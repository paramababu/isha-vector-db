# Security Policy

## Reporting a vulnerability

Report privately through GitHub Security Advisories on this repository. Please do not open a
public issue for a security problem.

We aim to acknowledge within 3 working days and to ship a fix or a mitigation within 90 days,
coordinating disclosure with you. Patch releases are provided for the current minor version and
the one before it.

## Threat model

There is no network in this project, so the adversary is not a remote attacker. It is:

1. **Corruption** — crashes, power loss, flaky flash.
2. **A malformed or hostile database file** handed to the application.
3. **Data at rest** on a device someone else can reach.

Anything that makes the library crash, hang, exhaust memory, or read out of bounds when given a
**corrupt or hostile database file** is a security issue and we want to hear about it. That is
the highest-value class of report for this project.

## What is and is not implemented

| | Status |
|---|---|
| Bounds-checked, fuzzed format decoders | Implemented, continuously fuzzed |
| Path-traversal prevention | Implemented — `DbPath` cannot represent `..` |
| Checksums on persisted blocks | Implemented (CRC-32C) |
| **Encryption at rest** | **Not implemented.** Designed for as a block codec; see docs/architecture/10 §10.3 |
| **Secure deletion** | **Not achievable** on flash or copy-on-write filesystems. Compaction removes deleted records from live files; genuine erasure needs full-disk encryption and key destruction, which is the platform's job. |

We will not claim encryption until it is implemented and tested.

## A note on embeddings

Vectors are **not** anonymized data. Inversion attacks can reconstruct substantial parts of the
source text from an embedding. Treat a vector database as containing the underlying content, and
apply the same retention, access and disclosure rules you would apply to that content.
