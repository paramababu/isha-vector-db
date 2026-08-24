# isha-vector-db in Python

## Install

```bash
pip install isha-vector-db
```

Python 3.9 or newer. No Python dependencies at all — the binding is `ctypes` over the engine's C
ABI, so there is nothing to compile at install time and no build toolchain required.

> **Not yet on PyPI.** The per-platform wheel build exists — `release.yml` builds one wheel per
> platform, each carrying that platform's shared library, and they are attached to the 0.1.0
> GitHub release — but PyPI trusted publishing is not configured, so nothing has been uploaded.
> A wheel built locally without that step is `py3-none-any` and contains only the Python files.
> Until the upload happens, take the wheel from the release, or build the library and point at it:
>
> ```bash
> cargo build -p isha-vector-db-ffi --release
> export PYTHONPATH=/path/to/isha-vector-db/sdk/python
> ```
>
> The binding finds a `target/release` build automatically from a checkout.

### Where it looks for the engine

In order: next to the package (where a bundled wheel would put it), then `target/release` and
`target/debug` relative to a checkout, then wherever the system loader can find it.
`ISHA_VECTOR_DB_LIBRARY=/path/to/libisha_vector_db_ffi.dylib` overrides all of that, which is
what you want while working on the engine itself.

A missing library is the most likely first failure, so the error lists every path it tried rather
than leaving `OSError` to be interpreted.

## Your first database

```python
import isha_vector_db as vdb

with vdb.open("./my-notes") as db:
    notes = db.collection("notes", dimension=4)

    notes.upsert("note-1", [1.0, 0.0, 0.0, 0.0])
    notes.upsert("note-2", [0.9, 0.1, 0.0, 0.0])
    notes.upsert("note-3", [0.0, 0.0, 1.0, 0.0])
    notes.flush()

    for hit in notes.search([1.0, 0.0, 0.0, 0.0], k=2):
        print(hit.id, round(hit.score, 4))
```

```text
note-1 1.0
note-2 0.9939
```

`./my-notes` is a directory. It is created if it does not exist, and the data is still there next
time you run this.

Use `with` unless you have a reason not to. It closes the database and releases its lock even if
something raises — and a lock left held is the most common way to be unable to reopen.

## With real embeddings

The package never imports NumPy, but a NumPy array works because it exposes the buffer protocol, which
is the fast path — no per-element conversion.

```python
import numpy as np
import isha_vector_db as vdb

vectors = model.encode(documents)          # (n, 384) float32
assert vectors.dtype == np.float32          # float64 would be silently wrong

with vdb.open("./search-index") as db:
    docs = db.collection("docs", dimension=384)
    for doc_id, vector in zip(ids, vectors):
        docs.upsert(doc_id, vector)
    docs.flush()

    hits = docs.search(model.encode(["what did I write about sailing?"])[0], k=10)
```

**`dtype` matters.** The engine stores 32-bit floats. A `float64` array is twice the bytes per
element, and reinterpreting it as `float32` produces plausible-looking nonsense rather than an
error. `astype(np.float32)` if you are unsure.

## Everything else you will need

```python
len(docs)                    # how many documents
"note-1" in docs             # does it exist
docs.delete("note-1")        # → True if it existed; deleting an absent one is not an error
docs.upsert("note-1", v)     # → False when it replaced rather than inserted
docs.flush()                 # write buffered changes to disk

vdb.version()                # {'library': '0.0.1', 'abi': 1, 'format': 2}
```

Metrics, if cosine is not what you want:

```python
db.collection("docs", dimension=384, metric=vdb.Metric.L2)
db.collection("docs", dimension=384, metric=vdb.Metric.DOT)
```

Read-only, for a second process that must not write:

```python
with vdb.open("./search-index", read_only=True, create_if_missing=False) as db:
    ...
```

## Errors

Every failure is a `vdb.VdbError` carrying the engine's own code. Branch on `code`, not on the
message — the code is stable and the message is not.

```python
try:
    docs.upsert("bad", [1.0, 2.0])       # the collection holds 3-dimensional vectors
except vdb.VdbError as e:
    print(e.code)                        # 4003
    print(e)                             # [VDB-4003] collection "docs" stores 3-dimensional vectors, got 2
```

The full list is in [error-codes.md](../api/error-codes.md). The leading digit is a band —
`4xxx` is a validation mistake, `5xxx` is storage trouble — so you can classify a code you do not
recognise.

## Things that catch people out

**A collection is not iterable.** There is no ordering over a vector index that means anything, so
`for doc in collection` raises `TypeError` rather than quietly returning storage order. Use
`search`.

**Only one process at a time.** Opening a database that another process holds fails with
`VDB-2001`. This is a lock, not a queue; it will not wait.

**`upsert_many` is a loop, not a batch.** It is a convenience over `upsert` and is no faster. The
engine has a real batch path; it is not exposed to Python yet.

**The GIL is held for the duration of a call.** A search over a large collection blocks every
other thread in your process. For a web server, run searches in a thread pool or a separate
process.

## Complete example

```python
"""Semantic search over a directory of text files."""
import pathlib
import numpy as np
import isha_vector_db as vdb
from sentence_transformers import SentenceTransformer

model = SentenceTransformer("all-MiniLM-L6-v2")   # 384 dimensions
files = list(pathlib.Path("notes").glob("*.txt"))

with vdb.open("./notes-index") as db:
    docs = db.collection("notes", dimension=384, metric=vdb.Metric.COSINE)

    if len(docs) == 0:                            # only index once
        texts = [f.read_text() for f in files]
        vectors = model.encode(texts).astype(np.float32)
        for path, vector in zip(files, vectors):
            docs.upsert(str(path), vector)
        docs.flush()
        print(f"indexed {len(docs)} notes")

    query = model.encode(["notes about the boat"]).astype(np.float32)[0]
    for hit in docs.search(query, k=5):
        print(f"{hit.score:.3f}  {hit.id}")
```
