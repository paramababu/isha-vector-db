"""vdb — an embedded vector database.

    import vdb

    db = vdb.open("./notes")
    notes = db.collection("notes", dimension=384)

    notes.upsert("note-1", embedding)
    notes.flush()

    for hit in notes.search(query, k=10):
        print(hit.id, hit.score)

    db.close()

Everything runs in this process against files on disk. There is no server to start and nothing
to configure.
"""

from __future__ import annotations

import ctypes
from ctypes import POINTER, byref, c_bool, c_float, c_size_t, c_uint64, c_void_p
from dataclasses import dataclass
from enum import IntEnum
from typing import Iterable, Iterator, Sequence

from . import _ffi

__all__ = [
    "open",
    "Database",
    "Collection",
    "Hit",
    "Metric",
    "Durability",
    "VdbError",
    "version",
]

_LIB = None


def _lib() -> ctypes.CDLL:
    """The loaded engine, loaded once.

    Deferred to first use rather than done at import: importing a module should not fail because
    a shared library is missing, when the caller might only have wanted ``vdb.__version__``.
    """
    global _LIB
    if _LIB is None:
        library = _ffi.load()
        _ffi.declare(library)
        _LIB = library
    return _LIB


class Metric(IntEnum):
    """How similarity is measured. Values match ``vdb_metric_t``; there is no zero."""

    COSINE = 1
    L2 = 2
    DOT = 3


class Durability(IntEnum):
    """How hard the engine works to survive a crash."""

    #: Sync every write. Safe against power loss, slow on flash.
    FULL = 1
    #: Sync on batch commit, flush and close. The default.
    BATCH = 2
    #: Sync on flush and close only. For bulk import.
    RELAXED = 3


class VdbError(Exception):
    """A failure, carrying the engine's own stable code.

    ``code`` is the number from ``docs/api/error-codes.md`` and is the thing to branch on. The
    message is written for a developer and is not stable — do not match on it.
    """

    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class Hit:
    """One search result. ``score`` is always higher-is-better, whatever the metric."""

    id: str
    score: float


class _Call:
    """Runs one C ABI call with its error out-parameter, and frees it however the call ends.

    Every entry point reports through ``vdb_error_t**`` and the caller owns what lands there, so
    doing it in one place is what keeps the methods below readable and means an early return
    cannot leak.
    """

    def __enter__(self) -> "c_void_p":
        self._err = c_void_p(None)
        return self._err

    def __exit__(self, *_exc: object) -> None:
        if self._err:
            _lib().vdb_error_free(self._err)


def _utf8(text: str):
    """A string as the ``(pointer, length)`` pair the ABI takes."""
    data = text.encode("utf-8")
    buffer = (ctypes.c_uint8 * max(len(data), 1)).from_buffer_copy(data + b"\0")
    return buffer, len(data)


def _floats(vector: Sequence[float]):
    """A vector as a C float array.

    Accepts anything indexable, including a numpy array, without importing numpy — a hard
    dependency on it would be absurd for a library whose job is to store the arrays somebody
    else produced.
    """
    try:
        # numpy and array.array expose the buffer protocol, which avoids a per-element loop.
        return (c_float * len(vector)).from_buffer_copy(memoryview(vector).cast("B")), len(vector)
    except (TypeError, ValueError):
        return (c_float * len(vector))(*vector), len(vector)


def version() -> dict:
    """Version information, without opening anything."""
    lib = _lib()
    return {
        "library": lib.vdb_version().decode(),
        # Frozen. A change to this breaks every compiled caller.
        "abi": int(lib.vdb_abi_version()),
        # Moves independently of the ABI.
        "format": int(lib.vdb_format_version()),
    }


def open(  # noqa: A001 - deliberately shadows the builtin, as sqlite3.connect would if named open
    path: str,
    *,
    create_if_missing: bool = True,
    read_only: bool = False,
    durability: Durability = Durability.BATCH,
) -> "Database":
    """Open or create a database in the directory ``path``.

    Only one handle may have a database open at a time, across every process on the machine; a
    second attempt fails rather than corrupting anything.
    """
    lib = _lib()
    raw_path, length = _utf8(path)
    handle = c_void_p(None)
    with _Call() as err:
        rc = lib.vdb_open(
            raw_path, length, create_if_missing, read_only, int(durability),
            byref(handle), byref(err),
        )
        _check(rc, err)
    return Database(handle)


def _check(rc: int, err: c_void_p) -> None:
    """Raise if the call failed, taking the engine's message with it."""
    if rc == 0:
        return
    lib = _lib()
    code, message = rc, f"vdb error {rc}"
    if err:
        code = int(lib.vdb_error_code(err))
        raw = lib.vdb_error_message(err)
        if raw:
            message = raw.decode("utf-8", "replace")
    raise VdbError(code, message)


class Database:
    """An open database. Use it as a context manager, or call :meth:`close`."""

    def __init__(self, handle: c_void_p) -> None:
        self._handle = handle
        self._open = True

    def collection(
        self, name: str, dimension: int, metric: Metric = Metric.COSINE
    ) -> "Collection":
        """Create a collection, or open it if it already exists.

        Creating first and falling back is deliberate: asking "does it exist?" and then acting
        would be two decisions with a gap between them, and the engine answers this atomically.
        """
        self._alive()
        lib = _lib()
        raw_name, length = _utf8(name)
        handle = c_void_p(None)
        with _Call() as err:
            rc = lib.vdb_collection_create(
                self._handle, raw_name, length, dimension, int(metric), False,
                byref(handle), byref(err),
            )
            if rc != 0:
                # Fall back *only* when it already exists. Retrying on any failure would replace
                # a precise diagnosis with a misleading "collection not found".
                already = bool(err) and int(lib.vdb_error_code(err)) == _ffi.COLLECTION_ALREADY_EXISTS
                if not already:
                    _check(rc, err)
                lib.vdb_error_free(err)
                err.value = None
                rc = lib.vdb_collection_open(
                    self._handle, raw_name, length, byref(handle), byref(err)
                )
                _check(rc, err)
        return Collection(handle)

    def flush(self) -> None:
        """Flush every collection to storage."""
        self._alive()
        with _Call() as err:
            _check(_lib().vdb_flush(self._handle, byref(err)), err)

    def close(self) -> None:
        """Close the database and release its lock. Closing twice is harmless."""
        if not self._open:
            return
        self._open = False
        with _Call() as err:
            _check(_lib().vdb_close(self._handle, byref(err)), err)

    @property
    def is_open(self) -> bool:
        return self._open

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def _alive(self) -> None:
        if not self._open:
            raise VdbError(0, "this database is closed")


class Collection:
    """One collection of vectors, all of the same dimension."""

    def __init__(self, handle: c_void_p) -> None:
        self._handle = handle
        self._live = True

    def upsert(self, id: str, vector: Sequence[float]) -> bool:  # noqa: A002
        """Insert or replace a document. Returns whether it was newly inserted."""
        self._alive()
        raw_id, id_len = _utf8(id)
        data, dimension = _floats(vector)
        inserted = c_bool(False)
        with _Call() as err:
            _check(
                _lib().vdb_upsert(
                    self._handle, raw_id, id_len, data, dimension, None,
                    byref(inserted), byref(err),
                ),
                err,
            )
        return bool(inserted)

    def upsert_many(self, documents: Iterable[tuple[str, Sequence[float]]]) -> int:
        """Insert or replace many documents, returning how many were new.

        A convenience over :meth:`upsert`, not a faster path — the engine's batch API is not yet
        exposed here, and pretending otherwise would be worse than the loop.
        """
        return sum(1 for id, vector in documents if self.upsert(id, vector))

    def delete(self, id: str) -> bool:  # noqa: A002
        """Remove a document. Returns whether it existed; deleting an absent one is not an error."""
        self._alive()
        raw_id, id_len = _utf8(id)
        existed = c_bool(False)
        with _Call() as err:
            _check(
                _lib().vdb_delete(self._handle, raw_id, id_len, byref(existed), byref(err)), err
            )
        return bool(existed)

    def __contains__(self, id: str) -> bool:  # noqa: A002
        self._alive()
        raw_id, id_len = _utf8(id)
        found = c_bool(False)
        with _Call() as err:
            _check(
                _lib().vdb_contains(self._handle, raw_id, id_len, byref(found), byref(err)), err
            )
        return bool(found)

    def __len__(self) -> int:
        self._alive()
        count = c_uint64(0)
        with _Call() as err:
            _check(_lib().vdb_collection_count(self._handle, byref(count), byref(err)), err)
        return int(count.value)

    def flush(self) -> None:
        """Flush this collection's writes."""
        self._alive()
        with _Call() as err:
            _check(_lib().vdb_collection_flush(self._handle, byref(err)), err)

    def search(self, query: Sequence[float], k: int = 10) -> list[Hit]:
        """The ``k`` nearest documents, best first.

        Ties break on ascending id, so the same query over the same data always returns the same
        order.
        """
        self._alive()
        lib = _lib()
        data, dimension = _floats(query)
        results = c_void_p(None)
        with _Call() as err:
            _check(
                lib.vdb_search(self._handle, data, dimension, k, byref(results), byref(err)), err
            )
        try:
            out = []
            for i in range(lib.vdb_results_len(results)):
                length = c_size_t(0)
                pointer = lib.vdb_results_id(results, i, byref(length))
                # Copied before the results are freed: the ids point into the engine's memory.
                raw = ctypes.string_at(pointer, length.value)
                out.append(Hit(raw.decode("utf-8", "replace"), float(lib.vdb_results_score(results, i))))
            return out
        finally:
            lib.vdb_results_free(results)

    def release(self) -> None:
        """Release this handle. The database stays open."""
        if not self._live:
            return
        self._live = False
        _lib().vdb_collection_free(self._handle)

    def __enter__(self) -> "Collection":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.release()

    def __iter__(self) -> Iterator[Hit]:  # pragma: no cover - deliberately absent
        raise TypeError(
            "a collection is not iterable: there is no ordering over a vector index that would "
            "mean anything. Use search() with a query."
        )

    def _alive(self) -> None:
        if not self._live:
            raise VdbError(0, "this collection has been released")
