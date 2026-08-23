# isha-vector-db

An embedded, offline-first vector database. No server, no dependencies, no network call in the
hot path — it runs inside your process against files on a disk, like SQLite.

```python
import isha_vector_db as vdb

with vdb.open("./notes") as db:
    notes = db.collection("notes", dimension=384)
    notes.upsert("note-1", embedding)
    notes.flush()

    for hit in notes.search(query, k=10):
        print(hit.id, hit.score)
```

The engine is Rust, reached through a frozen C ABI with `ctypes` — so there is nothing to compile
at install time and no Python dependencies at all. NumPy arrays work through the buffer protocol
without this package importing NumPy.

Full documentation: **[Getting started in Python][docs]**, and the
[error-code reference][errors].

[docs]: https://github.com/paramababu/isha-vector-db/blob/main/docs/getting-started/python.md
[errors]: https://github.com/paramababu/isha-vector-db/blob/main/docs/api/error-codes.md

Apache-2.0.
