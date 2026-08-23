"""Loading the shared library and declaring the C ABI to ctypes.

Kept apart from the public API so that the part which must exactly mirror ``include/vdb.h`` is
in one place, reviewable against the header, and not tangled with the ergonomics.

ctypes rather than a compiled extension module. A compiled one would be faster at the call
boundary and would need a C toolchain on every machine that installs the package, a build step in
CI for every Python version, and wheels per platform. The engine's work is measured in
milliseconds and a ctypes call in microseconds, so the overhead is not where the time goes — and
"pip install works everywhere" is worth more than a few microseconds a call.
"""

from __future__ import annotations

import ctypes
import os
import platform
import sys
from ctypes import (
    POINTER,
    c_bool,
    c_char_p,
    c_float,
    c_int32,
    c_size_t,
    c_uint8,
    c_uint32,
    c_uint64,
    c_void_p,
)
from pathlib import Path

# Return codes from the C ABI. Negative values are boundary rejections, made before the engine is
# reached; a positive code comes from the engine and is in docs/api/error-codes.md.
VDB_NULL_POINTER = -1
VDB_INTERNAL = -2
VDB_INVALID_UTF8 = -3
VDB_INVALID_ARGUMENT = -4

COLLECTION_ALREADY_EXISTS = 4001


def _library_name() -> str:
    system = platform.system()
    if system == "Darwin":
        return "libvdb_ffi.dylib"
    if system == "Windows":
        return "vdb_ffi.dll"
    return "libvdb_ffi.so"


def _candidates() -> list[Path]:
    """Where to look for the shared library, nearest first.

    ``VDB_LIBRARY`` wins, because a developer working on the engine needs to point at a build
    they just made without reinstalling anything.
    """
    override = os.environ.get("VDB_LIBRARY")
    if override:
        return [Path(override)]

    name = _library_name()
    here = Path(__file__).resolve().parent
    return [
        here / name,                                     # bundled in the wheel
        here.parent.parent.parent / "target" / "release" / name,  # a local cargo build
        here.parent.parent.parent / "target" / "debug" / name,
        Path(name),                                      # whatever the loader can find
    ]


def load() -> ctypes.CDLL:
    """Load the engine, or explain what to do about it.

    A missing shared library is the most likely first failure by a wide margin, so the message
    lists everywhere that was tried rather than leaving ``OSError: cannot open shared object`` to
    be interpreted.
    """
    attempts = _candidates()
    for path in attempts:
        try:
            return ctypes.CDLL(str(path))
        except OSError:
            continue
    tried = "\n  ".join(str(p) for p in attempts)
    raise ImportError(
        "could not load the vdb engine. Tried:\n  "
        + tried
        + "\n\nBuild it with `cargo build -p vdb-ffi --release`, or set VDB_LIBRARY to the "
        "shared library's path."
    )


def declare(lib: ctypes.CDLL) -> None:
    """Declare every function's signature.

    Not optional and not a formality. Without ``argtypes`` ctypes guesses, and on a 64-bit
    platform it truncates pointers to 32 bits — which produces a segfault at a random later
    moment rather than an error, and is the classic way a ctypes binding goes wrong.
    """
    u8p = POINTER(c_uint8)
    err_pp = POINTER(c_void_p)

    lib.vdb_version.restype = c_char_p
    lib.vdb_version.argtypes = []
    lib.vdb_abi_version.restype = c_int32
    lib.vdb_abi_version.argtypes = []
    lib.vdb_format_version.restype = c_uint32
    lib.vdb_format_version.argtypes = []

    lib.vdb_error_code.restype = c_uint32
    lib.vdb_error_code.argtypes = [c_void_p]
    lib.vdb_error_message.restype = c_char_p
    lib.vdb_error_message.argtypes = [c_void_p]
    lib.vdb_error_free.restype = None
    lib.vdb_error_free.argtypes = [c_void_p]

    lib.vdb_open.restype = c_int32
    lib.vdb_open.argtypes = [u8p, c_size_t, c_bool, c_bool, c_int32, POINTER(c_void_p), err_pp]
    lib.vdb_close.restype = c_int32
    lib.vdb_close.argtypes = [c_void_p, err_pp]
    lib.vdb_flush.restype = c_int32
    lib.vdb_flush.argtypes = [c_void_p, err_pp]

    lib.vdb_collection_create.restype = c_int32
    lib.vdb_collection_create.argtypes = [
        c_void_p, u8p, c_size_t, c_uint32, c_int32, c_bool, POINTER(c_void_p), err_pp,
    ]
    lib.vdb_collection_open.restype = c_int32
    lib.vdb_collection_open.argtypes = [c_void_p, u8p, c_size_t, POINTER(c_void_p), err_pp]
    lib.vdb_collection_free.restype = None
    lib.vdb_collection_free.argtypes = [c_void_p]
    lib.vdb_collection_count.restype = c_int32
    lib.vdb_collection_count.argtypes = [c_void_p, POINTER(c_uint64), err_pp]
    lib.vdb_collection_flush.restype = c_int32
    lib.vdb_collection_flush.argtypes = [c_void_p, err_pp]

    lib.vdb_upsert.restype = c_int32
    lib.vdb_upsert.argtypes = [
        c_void_p, u8p, c_size_t, POINTER(c_float), c_uint32, c_void_p, POINTER(c_bool), err_pp,
    ]
    lib.vdb_delete.restype = c_int32
    lib.vdb_delete.argtypes = [c_void_p, u8p, c_size_t, POINTER(c_bool), err_pp]
    lib.vdb_contains.restype = c_int32
    lib.vdb_contains.argtypes = [c_void_p, u8p, c_size_t, POINTER(c_bool), err_pp]

    lib.vdb_search.restype = c_int32
    lib.vdb_search.argtypes = [
        c_void_p, POINTER(c_float), c_uint32, c_size_t, POINTER(c_void_p), err_pp,
    ]
    lib.vdb_results_len.restype = c_size_t
    lib.vdb_results_len.argtypes = [c_void_p]
    lib.vdb_results_score.restype = c_float
    lib.vdb_results_score.argtypes = [c_void_p, c_size_t]
    lib.vdb_results_id.restype = POINTER(c_uint8)
    lib.vdb_results_id.argtypes = [c_void_p, c_size_t, POINTER(c_size_t)]
    lib.vdb_results_free.restype = None
    lib.vdb_results_free.argtypes = [c_void_p]


if sys.version_info < (3, 9):  # pragma: no cover - the package metadata refuses to install
    raise RuntimeError("vdb requires Python 3.9 or newer")
